//! Contains the implementation of the mining mode for the local engine.

use crate::adaptive_target::{AdaptiveTarget, AdaptiveTargetConfig, GasAvgConfig, GasLimitConfig, TxBounds};
use crate::{forkchoice::HeadHistory, metrics::LocalMinerMetrics, mode::MiningMode};
use crate::advance::build_and_submit_block;
use crate::backoff::handle_with_backoff;
use std::time::{UNIX_EPOCH, Duration, Instant};
use alloy_consensus::BlockHeader;
use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{
    EngineApiMessageVersion, PayloadAttributesBuilder, PayloadTypes,
};
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use alloy_primitives::B256;
use tracing::warn;
use tokio_util::sync::CancellationToken;
use crate::switch_policy::ModeSwitchPolicy;
use crate::resync::{ResyncController, ResyncConfig};

/// A mining mode for the local dev engine.
/// Algorithm used to decide when to mine.

/// Local miner advancing the chain
#[derive(Debug)]
pub struct LocalMiner<T: PayloadTypes, B, P, R>
where
    P: TransactionPool + Clone + Send + 'static,
    R: BlockReader,
{
    /// Blockchain data provider for latest headers (currently unused but kept for future enhancements)
    pub(crate) _provider: R,
    /// The transaction pool
    pub(crate) pool: P,
    /// The payload attribute builder for the engine
    payload_attributes_builder: B,
    /// Sender for events to engine.
    to_engine: BeaconConsensusEngineHandle<T>,
    /// The mining mode for the engine
    pub(crate) mode: MiningMode,
    /// The payload builder for the engine
    payload_builder: PayloadBuilderHandle<T>,
    /// Pending tx count threshold to switch to burst mining mode.
    pub(crate) burst_threshold: usize,
    /// Interval in milliseconds to mine blocks when above threshold.
    pub(crate) burst_interval_ms: u64,
    /// Timestamp for the next block.
    pub(crate) last_timestamp: u64,
    /// Recent head hashes and forkchoice helper.
    pub(crate) head_history: HeadHistory,
    /// Consecutive errors when advancing the chain – used for exponential back-off.
    consecutive_errors: u8,
    /// Prometheus metrics
    metrics: LocalMinerMetrics,
    /// Adaptive gas target controller
    pub(crate) adaptive: AdaptiveTarget,
    /// Hysteresis policy for switching mining modes.
    pub(crate) policy: ModeSwitchPolicy,
    /// Timestamp of the last mode switch to enforce minimum dwell time.
    pub(crate) last_mode_switch_at: Instant,
    /// Controller for handling repeated invalid FCU/newPayload with minimal coupling.
    pub(crate) resync: ResyncController,
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
        let (last_timestamp, head_history) = match provider
            .best_block_number()
            .and_then(|num| provider.sealed_header(num))
        {
            Ok(Some(header)) => {
                (header.timestamp(), HeadHistory::new(Some(header.hash())))
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
                (
                    std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    HeadHistory::new(Some(genesis_hash))
                )
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
                (
                    std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    HeadHistory::new(Some(genesis_hash))
                )
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

        // --- Hysteresis policy defaults ---
        // By default, enter threshold equals the provided burst_threshold,
        // and exit threshold is 80% of enter threshold. Minimum dwell: 500ms.
        let exit_threshold = std::cmp::max(1, burst_threshold.saturating_mul(8) / 10);
        let policy = ModeSwitchPolicy::new(
            burst_threshold,
            exit_threshold,
            Duration::from_millis(500),
        );

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
            head_history,
            consecutive_errors: 0,
            metrics: LocalMinerMetrics::default(),
            adaptive,
            policy,
            last_mode_switch_at: Instant::now(),
            resync: ResyncController::new(ResyncConfig::default()),
        }
    }

    /// Runs the [`LocalMiner`] in a loop, polling the miner and building payloads.
    /// Handle errors during `advance` with back-off and state reset.
    pub(crate) async fn handle_error(&mut self) {
        handle_with_backoff(&mut self.consecutive_errors, &self.metrics).await;
    }

    /// Reset adaptive state after a successful block.
    pub(crate) fn reset_state_after_success(&mut self) {
        self.consecutive_errors = 0;

    }

    /// Main event loop wrapper.
    ///
    /// For backward compatibility, this spawns the miner loop without an external
    /// shutdown handle by creating a new `CancellationToken`. Prefer
    /// `run_with_shutdown(token)` from `miner_loop.rs` when you want a graceful
    /// shutdown signal from the caller.
    pub async fn run(self) {
        let token = CancellationToken::new();
        self.run_with_shutdown(token).await;
    }

    /// Sends a FCU to the engine.
    pub(crate) async fn update_forkchoice_state(&self) -> eyre::Result<()> {
        let res = self
            .to_engine
            .fork_choice_updated(self.head_history.state(), None, EngineApiMessageVersion::default())
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid fork choice update")
        }

        Ok(())
    }

    /// Advances the chain by one block.
    pub(crate) async fn advance(&mut self) -> eyre::Result<()> {
        build_and_submit_block(
            &self.to_engine,
            &self.payload_attributes_builder,
            &self.payload_builder,
            &self.pool,
            &self.metrics,
            &mut self.head_history,
            &mut self.last_timestamp,
            &mut self.adaptive,
            self.burst_interval_ms,
        ).await
    }
}
