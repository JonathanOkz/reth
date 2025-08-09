//! Contains the implementation of the mining mode for the local engine.

use alloy_consensus::BlockHeader;
use alloy_primitives::{TxHash, B256};
use alloy_rpc_types_engine::ForkchoiceState;
use std::collections::VecDeque;
use crate::adaptive_target::{AdaptiveTarget, AdaptiveTargetConfig, GasAvgConfig, GasLimitConfig, TxBounds};
use reth_metrics::{metrics::{Counter, Gauge, Histogram}, metrics, Metrics};
use eyre::OptionExt;
use futures_util::{stream::Fuse, StreamExt};
use std::time::{Duration, UNIX_EPOCH};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use tokio::sync::mpsc;
use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{
    BuiltPayload, EngineApiMessageVersion, PayloadAttributesBuilder, PayloadKind, PayloadTypes,
};
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use reth_primitives_traits::block::body::BlockBody;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, warn};

/// A mining mode for the local dev engine.
/// Algorithm used to decide when to mine.

/// Mining trigger mode used by `LocalMiner` to wait for transactions before building a block.
/// Currently only `Instant` is supported, which wakes as soon as a new pending transaction arrives.
/// Metrics for `LocalMiner`.
#[derive(Metrics, Clone)]
#[metrics(scope = "local_miner")]
pub struct LocalMinerMetrics {
    /// Counter for errors encountered while mining.
    pub miner_errors_total: Counter,
    /// Counter for payload build timeouts (non-fatal).
    pub miner_timeouts_total: Counter,
    /// Histogram for payload build duration in seconds.
    pub payload_build_duration_seconds: Histogram,
    /// Gauge tracking block gas utilization (0-100).
    pub block_utilization_percent: Gauge,
    /// Gauge tracking pending transactions at the moment a block is mined.
    pub pending_txs_at_mine: Gauge,
    /// Gauge of current pool depth.
    pub pending_pool_depth: Gauge,
    /// Counter for successfully mined blocks.
    pub blocks_mined_total: Counter,
    /// Counter for total transactions included in mined blocks.
    pub txs_mined_total: Counter,
}

#[derive(Debug)]
/// Mining trigger modes (currently only `Instant`).
pub enum MiningMode {
    /// In this mode a block is built as soon as
    /// a valid transaction reaches the pool.
    Instant(Fuse<ReceiverStream<TxHash>>),
    /// Debounced mode: emits at most one trigger per period when busy
    Debounced(mpsc::Receiver<()>),
}

impl MiningMode {
    /// Constructor for a [`MiningMode::Instant`]
    pub fn instant<Pool: TransactionPool>(pool: Pool) -> Self {
        let rx = pool.pending_transactions_listener();
        Self::Instant(ReceiverStream::new(rx).fuse())
    }

    /// Constructor for debounced mode
    pub fn debounced<Pool>(pool: Pool, period_ms: u64) -> Self
    where
        Pool: TransactionPool + Clone + Send + 'static,
    {
        let period_ms = period_ms.clamp(50, 500);
        let flag = Arc::new(AtomicBool::new(false));
        let (tx_alert, rx_alert) = mpsc::channel::<()>(1);

        // Spawn listener task that sets the flag on every new tx
        {
            let flag_clone = Arc::clone(&flag);
            let mut listener = pool.pending_transactions_listener();
            tokio::spawn(async move {
                while listener.recv().await.is_some() {
                    flag_clone.store(true, Ordering::Relaxed);
                }
            });
        }

        // Spawn timer task that checks the flag periodically
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                if flag.swap(false, Ordering::Relaxed) {
                    let _ = tx_alert.try_send(());
                }
            }
        });

        Self::Debounced(rx_alert)
    }
}

impl MiningMode {
    /// Wait until the next mining trigger (tx arrival or interval tick).
    pub async fn wait(&mut self) {
        match self {
            Self::Instant(rx) => {
                // If the stream finished (pool listener closed), fall back to a short sleep.
                if rx.next().await.is_none() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Self::Debounced(rx) => {
                // One tick per debounce period while busy
                let _ = rx.recv().await;
            }
        }
    }
}

/// Local miner advancing the chain
#[derive(Debug)]
pub struct LocalMiner<T: PayloadTypes, B, P, R>
where
    P: TransactionPool + Clone + Send + 'static,
    R: BlockReader,
{
    /// Blockchain data provider for latest headers (currently unused but kept for future enhancements)
    _provider: R,
    /// The transaction pool
    pool: P,
    /// The payload attribute builder for the engine
    payload_attributes_builder: B,
    /// Sender for events to engine.
    to_engine: BeaconConsensusEngineHandle<T>,
    /// The mining mode for the engine
    mode: MiningMode,
    /// The payload builder for the engine
    payload_builder: PayloadBuilderHandle<T>,
    /// Pending tx count threshold to switch to burst mining mode.
    burst_threshold: usize,
    /// Interval in milliseconds to mine blocks when above threshold.
    burst_interval_ms: u64,
    /// Timestamp for the next block.
    last_timestamp: u64,
    /// Stores latest mined blocks.
    last_block_hashes: VecDeque<B256>,
    /// Consecutive errors when advancing the chain – used for exponential back-off.
    consecutive_errors: u8,
    /// Prometheus metrics
    metrics: LocalMinerMetrics,
    /// Adaptive gas target controller
    adaptive: AdaptiveTarget,
}

impl<T, B, P, R> LocalMiner<T, B, P, R>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool + Clone + Send + 'static,
    R: BlockReader,
{
    /// Spawns a new [`LocalMiner`] with the given parameters.
    pub fn new(
        provider: R,
        payload_attributes_builder: B,
        to_engine: BeaconConsensusEngineHandle<T>,
        mode: MiningMode,
        payload_builder: PayloadBuilderHandle<T>,
        pool: P,
        burst_threshold: usize,
        burst_interval_ms: u64,
        initial_avg_tx_gas: f64,
        alpha_gas: f64,
        kp: f64,
        kd: f64,
        min_tx_per_block: usize,
        max_tx_per_block: usize,
    ) -> Self {
        // Try to fetch the latest sealed header for initial state. If unavailable, fall back to
        // genesis-like defaults instead of panicking.
        let (last_timestamp, last_block_hashes) = match provider
            .best_block_number()
            .and_then(|num| provider.sealed_header(num))
        {
            Ok(Some(header)) => {
                let mut d: VecDeque<B256> = VecDeque::with_capacity(64);
                d.push_back(header.hash());
                (header.timestamp(), d)
            },
            Ok(None) => {
                warn!(target: "engine::local", "No header found for best block – starting with empty state");
                let genesis_hash = provider
                    .sealed_header(0)
                    .ok()
                    .flatten()
                    .map(|h| h.hash())
                    .unwrap_or_else(|| {
                        warn!(target: "engine::local", "Could not fetch genesis header; using B256::ZERO");
                        B256::ZERO
                    });
                (std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), { let mut d: VecDeque::<B256> = VecDeque::with_capacity(64); d.push_back(genesis_hash); d })
            }
            Err(err) => {
                warn!(target: "engine::local", ?err, "Error fetching best header – starting with empty state");
                let genesis_hash = provider
                    .sealed_header(0)
                    .ok()
                    .flatten()
                    .map(|h| h.hash())
                    .unwrap_or_else(|| {
                        warn!(target: "engine::local", "Could not fetch genesis header; using B256::ZERO");
                        B256::ZERO
                    });
                (std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), { let mut d: VecDeque::<B256> = VecDeque::with_capacity(64); d.push_back(genesis_hash); d })
            }
        };

        // Determine gas limit from latest sealed header, or fall back to genesis header.
        let gas_limit = provider
            .best_block_number()
            .and_then(|n| provider.sealed_header(n))
            .ok()
            .flatten()
            .or_else(|| provider.sealed_header(0).ok().flatten())
            .map(|h| h.gas_limit())
            .expect("Unable to determine block gas limit (no sealed or genesis header present)");

        // Validate numeric parameters
                assert!(alpha_gas > 0.0 && alpha_gas <= 1.0, "mine-alpha must be within (0,1]");
        assert!(min_tx_per_block >= 1, "mine-min-tx must be at least 1");
        assert!(min_tx_per_block <= max_tx_per_block, "mine-min-tx must be <= mine-max-tx");
        assert!(initial_avg_tx_gas > 0.0, "mine-initial-avg-tx-gas must be positive");
        assert!(burst_threshold > 0, "mine-burst-threshold must be positive");
        assert!(burst_interval_ms > 0, "mine-burst-interval-ms must be positive");

        let adaptive_cfg = AdaptiveTargetConfig {
            gas_limit,
            target_gas_percent: 50.0,
            gas_avg_config: GasAvgConfig {
                initial_avg_tx_gas,
                alpha: alpha_gas,
            },
            gas_limit_config: GasLimitConfig { kp, kd },
            tx_bounds: TxBounds {
                min: min_tx_per_block,
                max: max_tx_per_block,
            },
        };
        let adaptive = AdaptiveTarget::new(adaptive_cfg);

        Self {
            _provider: provider,
            pool,
            burst_threshold,
            burst_interval_ms,
            payload_attributes_builder,
            to_engine,
            mode,
            payload_builder,
            last_timestamp,
            last_block_hashes: last_block_hashes,
            consecutive_errors: 0,
            metrics: LocalMinerMetrics::default(),
            adaptive,
        }
    }

    /// Runs the [`LocalMiner`] in a loop, polling the miner and building payloads.
    /// Handle errors during `advance` with back-off and state reset.
    async fn handle_error(&mut self) {
        // Increment error metric
        self.metrics.miner_errors_total.increment(1);
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        
        // If too many consecutive errors, enter cooldown instead of panicking
        if self.consecutive_errors > 10 {
            error!(target: "engine::local", "Too many consecutive errors ({}), entering 60s cooldown", self.consecutive_errors);
            tokio::time::sleep(Duration::from_secs(60)).await;
            // reset error counter after cooldown
            self.consecutive_errors = 0;
            return;
        }
        
        // Exponential backoff with max cap at 30 seconds
        let wait_time = Duration::from_millis(
            100u64.saturating_mul(2u64.saturating_pow(self.consecutive_errors.min(10) as u32))
        );
        let capped_wait = std::cmp::min(wait_time, Duration::from_secs(30));
        
        warn!(
            target: "engine::local",
            "Error #{}, waiting {:?} before retry",
            self.consecutive_errors,
            capped_wait
        );
        
        tokio::time::sleep(capped_wait).await;
    }

    /// Reset adaptive state after a successful block.
    fn reset_state_after_success(&mut self) {
        self.consecutive_errors = 0;

    }

    /// Main event loop of the miner.
    ///
    /// The loop waits on three asynchronous sources:
    /// 1. `self.mode.wait()` – wakes up on new pending transactions (instant mode).
    /// 2. `burst_interval.tick()` – periodic timer used when the pool is above `burst_threshold`.
    /// 3. `fcu_interval.tick()` – periodic fork-choice update to keep the engine in sync.
    ///
    /// It applies basic rate-limiting (`min_mine_interval`) to avoid building blocks too
    /// frequently under noisy transaction spam and uses [`Self::handle_error`] with an
    /// exponential back-off strategy to recover from engine failures.  The function never
    /// returns under normal operation – it will panic only after `consecutive_errors > 10`,
    /// signalling that a supervising process should restart the node.
    pub async fn run(mut self) {
        let burst_threshold = self.burst_threshold;
        
        // Ensure burst_interval_ms is at least 100ms to prevent CPU issues
        let interval_ms = std::cmp::max(self.burst_interval_ms, 100);
        let mut burst_interval = tokio::time::interval(Duration::from_millis(interval_ms));
        burst_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        
        let mut fcu_interval = tokio::time::interval(Duration::from_secs(1));
        fcu_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        
        // Track last mining attempt to prevent race conditions
        let mut last_mine_attempt = std::time::Instant::now();
        let min_mine_interval = Duration::from_millis(50); // Minimum 50ms between mining attempts
        
        info!(
            target: "engine::local",
            "LocalMiner started with burst_threshold={}, burst_interval={}ms",
            burst_threshold,
            interval_ms
        );
        
        loop {
            tokio::select! {
                // Triggered on every new pending tx (instant mode)
                _ = self.mode.wait() => {
                    let pending = self.pool.pool_size().pending as usize;

                    // Dynamic switch between Instant and Debounced based on pending txs
                    if pending >= burst_threshold {
                        if !matches!(self.mode, MiningMode::Debounced(_)) {
                            info!(target: "engine::local", "Switching mining mode → Debounced (pending={})", pending);
                            self.mode = MiningMode::debounced(self.pool.clone(), interval_ms.clamp(50, 500));
                        }
                    } else {
                        if !matches!(self.mode, MiningMode::Instant(_)) {
                            info!(target: "engine::local", "Switching mining mode → Instant (pending={})", pending);
                            self.mode = MiningMode::instant(self.pool.clone());
                        }
                    }


                    let pending = self.pool.pool_size().pending;
                    
                    
                    // Prevent mining too frequently
                    let now = std::time::Instant::now();
                    if now.duration_since(last_mine_attempt) < min_mine_interval {
                        continue;
                    }
                    
                    // Decide using adaptive target instead of static threshold
                    if self.adaptive.should_mine(pending as usize) && (pending as usize) < burst_threshold {
                        last_mine_attempt = now;
                        
                        // Mine immediately with whatever is pending (fast path)
                        if let Err(e) = self.advance().await {
                            error!(target: "engine::local", "Error advancing the chain: {:?}", e);
                            self.handle_error().await;
                        } else {
                            self.reset_state_after_success();
                        }
                    }
                    // If pending >= burst_threshold, let burst interval handle it
                }
                // Burst mining interval - for high transaction loads
                _ = burst_interval.tick() => {
                    let pending = self.pool.pool_size().pending;
                    
                    // Only mine if we have enough transactions for burst mode
                    if pending >= burst_threshold {
                        last_mine_attempt = std::time::Instant::now();
                        
                        if let Err(e) = self.advance().await {
                            error!(target: "engine::local", "Error advancing the chain (burst): {:?}", e);
                            self.handle_error().await;
                        } else {
                            self.reset_state_after_success();
                            
                            // Log successful burst mining
                            info!(
                                target: "engine::local",
                                "Burst mined block with {} pending transactions",
                                pending
                            );
                        }
                    }
                }
                // send FCU once in a while
                _ = fcu_interval.tick() => {
                    if let Err(e) = self.update_forkchoice_state().await {
                        error!(target: "engine::local", "Error updating fork choice: {:?}", e);
                    }
                }
            }
        }
    }

    /// Returns current forkchoice state.
    fn forkchoice_state(&self) -> ForkchoiceState {
        let len = self.last_block_hashes.len();
        let head = *self.last_block_hashes.back().unwrap_or(&B256::ZERO);
        let safe = if len >= 32 {
            *self.last_block_hashes.get(len - 32).unwrap_or(&B256::ZERO)
        } else {
            B256::ZERO
        };
        let finalized = if len >= 64 {
            *self.last_block_hashes.get(len - 64).unwrap_or(&B256::ZERO)
        } else {
            B256::ZERO
        };
        ForkchoiceState {
            head_block_hash: head,
            safe_block_hash: safe,
            finalized_block_hash: finalized,
        }
    }

    /// Sends a FCU to the engine.
    async fn update_forkchoice_state(&self) -> eyre::Result<()> {
        let res = self
            .to_engine
            .fork_choice_updated(self.forkchoice_state(), None, EngineApiMessageVersion::default())
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid fork choice update")
        }

        Ok(())
    }


    async fn advance(&mut self) -> eyre::Result<()> {
        // Get current system time with proper error handling
        let current_time = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_else(|e| {
                error!(target: "engine::local", "System time error: {:?}, using last_timestamp + 1", e);
                self.last_timestamp + 1
            });
        
        // Ensure timestamp always moves forward
        let mut timestamp = std::cmp::max(self.last_timestamp + 1, current_time);
        const MAX_SKEW_SECS: u64 = 3600; // allow up to 1 hour into the future
        if timestamp > current_time + MAX_SKEW_SECS {
            warn!(
                target: "engine::local",
                "System clock appears to have jumped forward by more than {} s (ts={}, now={}), clamping to now+MAX_SKEW", 
                MAX_SKEW_SECS, timestamp, current_time
            );
            timestamp = current_time + MAX_SKEW_SECS;
        }
        
        // Sanity check: if timestamp jumps too far, log a warning
        if self.last_timestamp > 0 && timestamp > self.last_timestamp + 3600 {
            warn!(
                target: "engine::local",
                "Large timestamp jump detected: {} -> {} (diff: {}s)",
                self.last_timestamp,
                timestamp,
                timestamp - self.last_timestamp
            );
        }

        let res = self
            .to_engine
            .fork_choice_updated(
                self.forkchoice_state(),
                Some(self.payload_attributes_builder.build(timestamp)),
                EngineApiMessageVersion::default(),
            )
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload status")
        }

        let payload_id = res.payload_id.ok_or_eyre("No payload id")?;

        // start timing payload build
        let build_start = std::time::Instant::now();

                // Timeout aligned with target cadence (burst_interval_ms).
        let pending = self.pool.pool_size().pending as usize;
        // metrics: current pool depth
        self.metrics.pending_pool_depth.set(pending as f64);
        // Clamp build timeout to a sane range [50ms, 5s]
        let timeout_ms = self.burst_interval_ms.clamp(50, 5_000);

        let payload_res = tokio::time::timeout(
            Duration::from_millis(timeout_ms),
            self.payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending),
        )
        .await;

        let payload = match payload_res {
            Ok(Some(Ok(p))) => p,
            other => {
                // First attempt failed: classify error type
                if other.is_err() {
                    self.metrics.miner_timeouts_total.increment(1);
                } else {
                    self.metrics.miner_errors_total.increment(1);
                }
                // Retry once immediately with `Earliest` kind under a short timeout
                let retry_timeout_ms = self.burst_interval_ms.min(200).max(50);
                let retry = tokio::time::timeout(
                    Duration::from_millis(retry_timeout_ms),
                    self.payload_builder.resolve_kind(payload_id, PayloadKind::Earliest),
                )
                .await;

                match retry {
                    Ok(Some(Ok(p))) => p,
                    Ok(_) => eyre::bail!("No payload after retry"),
                    Err(_) => {
                        self.metrics.miner_timeouts_total.increment(1);
                        eyre::bail!("Retry timeout waiting for payload")
                    }
                }
            }
        };

        // record duration metric
        self.metrics.payload_build_duration_seconds.record(build_start.elapsed().as_secs_f64());

        let block = payload.block();

        // Skip mining when block would contain zero transactions
        let tx_count = block.body().transaction_count();
        // Record gauge even for empty blocks
        self.metrics.pending_txs_at_mine.set(tx_count as f64);
        if tx_count == 0 {
            // Even empty block outcome should update adaptive avg gas mildly (skip)

            // Log this as debug, not error - it's normal behavior
            debug!(target: "engine::local", "Skipping empty block (no transactions)");
            return Ok(());
        }
        
        // Log successful block preparation
        info!(
            target: "engine::local",
            "Prepared block #{} with {} transactions, timestamp: {}",
            block.number(),
            tx_count,
            timestamp
        );

        let payload = T::block_to_payload(payload.block().clone());
        let res = self.to_engine.new_payload(payload).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload")
        }

        // Counters for successful block/tx production
        self.metrics.blocks_mined_total.increment(1);
        self.metrics.txs_mined_total.increment(tx_count as u64);

        // Update adaptive estimator with actual gas usage of this block
        self.adaptive.on_mined_block(block.gas_used(), tx_count);

        self.last_timestamp = timestamp;
        // Metrics: utilization & pending tx gauges
        let utilization = if block.gas_limit() > 0 {
            (block.gas_used() as f64 / block.gas_limit() as f64) * 100.0
        } else { 0.0 };
        self.metrics.block_utilization_percent.set(utilization);
        self.metrics.pending_txs_at_mine.set(tx_count as f64);

    // maintain ring buffer of last 64 hashes
    if self.last_block_hashes.len() == 64 {
        self.last_block_hashes.pop_front();
    }
    self.last_block_hashes.push_back(block.hash());

        // Send immediate FCU after new head is known; log on failure
        if let Err(e) = self.update_forkchoice_state().await {
            error!(target: "engine::local", "Error updating fork choice after new block: {:?}", e);
        }

        Ok(())
    }
}


