//! Contains the implementation of the mining mode for the local engine.

use alloy_consensus::BlockHeader;
use alloy_primitives::{TxHash, B256};
use alloy_rpc_types_engine::ForkchoiceState;
use std::collections::VecDeque;
use reth_metrics::{metrics::{Counter, Gauge, Histogram}, metrics, Metrics};
use eyre::OptionExt;
use futures_util::{stream::Fuse, StreamExt};
use std::time::{Duration, UNIX_EPOCH};
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
}

#[derive(Debug)]
/// Mining trigger modes (currently only `Instant`).
pub enum MiningMode {
    /// In this mode a block is built as soon as
    /// a valid transaction reaches the pool.
    Instant(Fuse<ReceiverStream<TxHash>>),
}

impl MiningMode {
    /// Constructor for a [`MiningMode::Instant`]
    pub fn instant<Pool: TransactionPool>(pool: Pool) -> Self {
        let rx = pool.pending_transactions_listener();
        Self::Instant(ReceiverStream::new(rx).fuse())
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
        }
    }
}

/// Local miner advancing the chain
#[derive(Debug)]
pub struct LocalMiner<T: PayloadTypes, B, P, R>
where
    P: TransactionPool,
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
}

impl<T, B, P, R> LocalMiner<T, B, P, R>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool,
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
                    let pending = self.pool.pool_size().pending;
                    
                    // Prevent mining too frequently
                    let now = std::time::Instant::now();
                    if now.duration_since(last_mine_attempt) < min_mine_interval {
                        continue;
                    }
                    
                    // Only mine in instant mode if below burst threshold
                    if pending > 0 && pending < burst_threshold {
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
        ForkchoiceState {
            head_block_hash: *self.last_block_hashes.back().unwrap_or(&B256::ZERO),
            safe_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(32))
                .unwrap_or(&B256::ZERO),
            finalized_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(64))
                .unwrap_or(&B256::ZERO),
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
        let timestamp = std::cmp::max(self.last_timestamp + 1, current_time);
        
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

        // Dynamic timeout based on pool size: base 2s + 1s per 500 tx, capped at 30s
        let pending = self.pool.pool_size().pending as u64;
        // metrics: current pool depth
        self.metrics.pending_pool_depth.set(pending as f64);
        let timeout_secs = (2u64.saturating_add(pending.saturating_div(500))).clamp(2, 30);

        let payload_res = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
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
                // Retry once immediately with `Earliest` kind (no extra timeout)
                match self
                    .payload_builder
                    .resolve_kind(payload_id, PayloadKind::Earliest)
                    .await
                {
                    Some(Ok(p)) => p,
                    _ => eyre::bail!("No payload after retry"),
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

        Ok(())
    }
}


