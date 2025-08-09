//! Advance helper: build and submit a block for LocalMiner.

use std::time::{Duration, UNIX_EPOCH};

use eyre::OptionExt;
use tracing::{debug, error, info, warn};

use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{
    EngineApiMessageVersion, PayloadAttributesBuilder, PayloadKind, PayloadTypes, BuiltPayload,
};
use reth_primitives_traits::block::body::BlockBody;
use alloy_consensus::BlockHeader;
use reth_transaction_pool::TransactionPool;

use crate::adaptive_target::AdaptiveTarget;
use crate::forkchoice::HeadHistory;
use crate::metrics::LocalMinerMetrics;

/// Full block build-and-submit path previously implemented inside `LocalMiner::advance`.
///
/// Returns Ok(()) on success, otherwise an error that callers may handle with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn build_and_submit_block<T, B, P>(
    to_engine: &BeaconConsensusEngineHandle<T>,
    payload_attributes_builder: &B,
    payload_builder: &PayloadBuilderHandle<T>,
    pool: &P,
    metrics: &LocalMinerMetrics,
    head_history: &mut HeadHistory,
    last_timestamp: &mut u64,
    adaptive: &mut AdaptiveTarget,
    burst_interval_ms: u64,
) -> eyre::Result<()>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool,
{
    // Get current system time with proper error handling
    let current_time = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_else(|e| {
            error!(target: "engine::local", "System time error: {:?}, using last_timestamp + 1", e);
            *last_timestamp + 1
        });

    // Ensure timestamp always moves forward
    let mut timestamp = std::cmp::max(*last_timestamp + 1, current_time);
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
    if *last_timestamp > 0 && timestamp > *last_timestamp + 3600 {
        warn!(
            target: "engine::local",
            "Large timestamp jump detected: {} -> {} (diff: {}s)",
            *last_timestamp,
            timestamp,
            timestamp - *last_timestamp
        );
    }

    // FCU with attributes to kick off payload build
    let res = to_engine
        .fork_choice_updated(
            head_history.state(),
            Some(payload_attributes_builder.build(timestamp)),
            EngineApiMessageVersion::default(),
        )
        .await?;

    if !res.is_valid() {
        eyre::bail!("Invalid payload status")
    }

    let payload_id = res.payload_id.ok_or_eyre("No payload id")?;

    // start timing payload build
    let build_start = std::time::Instant::now();

    // metrics: current pool depth
    let pending = pool.pool_size().pending as usize;
    metrics.pending_pool_depth.set(pending as f64);

    // Clamp build timeout to a sane range [50ms, 5s]
    let timeout_ms = burst_interval_ms.clamp(50, 5_000);

    let payload_res = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending),
    )
    .await;

    let payload = match payload_res {
        Ok(Some(Ok(p))) => p,
        other => {
            // First attempt failed: classify error type
            if other.is_err() {
                metrics.miner_timeouts_total.increment(1);
            } else {
                metrics.miner_errors_total.increment(1);
            }
            // Retry once immediately with `Earliest` kind under a short timeout
            let retry_timeout_ms = burst_interval_ms.min(200).max(50);
            let retry = tokio::time::timeout(
                Duration::from_millis(retry_timeout_ms),
                payload_builder.resolve_kind(payload_id, PayloadKind::Earliest),
            )
            .await;

            match retry {
                Ok(Some(Ok(p))) => p,
                Ok(_) => eyre::bail!("No payload after retry"),
                Err(_) => {
                    metrics.miner_timeouts_total.increment(1);
                    eyre::bail!("Retry timeout waiting for payload")
                }
            }
        }
    };

    // record duration metric
    metrics
        .payload_build_duration_seconds
        .record(build_start.elapsed().as_secs_f64());

    let block = payload.block();

    // Skip mining when block would contain zero transactions
    let tx_count = BlockBody::transaction_count(block.body());
    // Record gauge even for empty blocks
    metrics.pending_txs_at_mine.set(tx_count as f64);
    if tx_count == 0 {
        // Log this as debug, not error - it's normal behavior
        debug!(target: "engine::local", "Skipping empty block (no transactions)");
        return Ok(());
    }

    // Log successful block preparation
    info!(
        target: "engine::local",
        "Prepared block with {} transactions, timestamp: {}",
        tx_count,
        timestamp
    );

    let payload = T::block_to_payload(payload.block().clone());
    let res = to_engine.new_payload(payload).await?;

    if !res.is_valid() {
        eyre::bail!("Invalid payload")
    }

    // Counters for successful block/tx production
    metrics.blocks_mined_total.increment(1);
    metrics.txs_mined_total.increment(tx_count as u64);

    // Update adaptive estimator with actual gas usage of this block
    adaptive.on_mined_block(block.gas_used(), tx_count);

    *last_timestamp = timestamp;

    // Metrics: utilization & pending tx gauges
    let gas_limit = block.gas_limit();
    let utilization = if gas_limit > 0 {
        (block.gas_used() as f64 / gas_limit as f64) * 100.0
    } else {
        0.0
    };
    metrics.block_utilization_percent.set(utilization);
    metrics.pending_txs_at_mine.set(tx_count as f64);

    // maintain recent head history
    head_history.push(block.hash());

    // Send immediate FCU after new head is known; log on failure
    if let Err(e) = to_engine
        .fork_choice_updated(head_history.state(), None, EngineApiMessageVersion::default())
        .await
    {
        error!(target: "engine::local", "Error updating fork choice after new block: {:?}", e);
    }

    Ok(())
}
