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
    // Get current system time in milliseconds with proper error handling
    let current_time_ms: u64 = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_millis() as u128).min(u64::MAX as u128) as u64)
        .unwrap_or_else(|e| {
            error!(target: "engine::local", "System time error: {:?}, using last_timestamp + 1", e);
            *last_timestamp + 1 // keep monotonic progression in ms
        });

    // Ensure timestamp always moves forward (ms precision)
    let mut timestamp_ms = std::cmp::max(*last_timestamp + 1, current_time_ms);
    const MAX_SKEW_MS: u64 = 600_000; // allow up to 10 minutes into the future (in ms)
    if timestamp_ms > current_time_ms + MAX_SKEW_MS {
        warn!(
            target: "engine::local",
            "System clock appears to have jumped forward by more than {} ms (ts={}, now={}), clamping to now+MAX_SKEW",
            MAX_SKEW_MS, timestamp_ms, current_time_ms
        );
        timestamp_ms = current_time_ms + MAX_SKEW_MS;
    }

    // Sanity check: warn on large jumps
    if *last_timestamp > 0 && timestamp_ms > *last_timestamp + 600_000 {
        warn!(
            target: "engine::local",
            "Large timestamp jump detected: {} -> {} (diff: {}ms)",
            *last_timestamp,
            timestamp_ms,
            timestamp_ms - *last_timestamp
        );
    }

    // ---------------------------------------------------------------------
    // Convert to *seconds* for the engine and enforce strict monotonicity
    // ---------------------------------------------------------------------
    let mut timestamp = timestamp_ms / 1_000; // floor division
    let last_secs = *last_timestamp / 1_000;
    if timestamp <= last_secs {
        timestamp = last_secs + 1;
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
    let pending = pool.pool_size().pending;
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
            let retry_timeout_ms = burst_interval_ms.clamp(50, 200);
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
        "Prepared block with {} transactions, timestamp(ms): {}",
        tx_count,
        timestamp_ms
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

    *last_timestamp = timestamp_ms;

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
