//! Advance helper: build and submit a block for LocalMiner.

use std::time::{Duration, UNIX_EPOCH};

/// Maximum allowed clock skew between the local wall clock and any timestamp we
const MAX_CLOCK_SKEW_MS: u64 = 60_000;

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

// ---------------------------------------------------------------------
// Helper utilities
// ---------------------------------------------------------------------

/// Returns `true` if the absolute difference between the two millisecond timestamps
/// exceeds [`MAX_CLOCK_SKEW_MS`]. Extracted so we can unit-test the policy in
/// isolation.
#[inline]
fn exceeds_clock_skew(a_ms: u64, b_ms: u64) -> bool {
    a_ms.abs_diff(b_ms) > MAX_CLOCK_SKEW_MS
}

/// Full block build-and-submit path previously implemented inside `LocalMiner::advance`.
///
/// Returns Ok(()) on success, otherwise an error that callers may handle with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn build_and_submit_block<T, B, P, R>(
    to_engine: &BeaconConsensusEngineHandle<T>,
    payload_attributes_builder: &B,
    payload_builder: &PayloadBuilderHandle<T>,
    pool: &P,
    metrics: &LocalMinerMetrics,
    head_history: &mut HeadHistory,
    provider: &R,
    last_timestamp: &mut u64,
    adaptive: &mut AdaptiveTarget,
    burst_interval_ms: u64,
) -> eyre::Result<()>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool,
    R: reth_provider::HeaderProvider,
{
    // Get current system time with proper error handling
    let current_time = match std::time::SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs(),
        Err(e) => {
            error!(target: "engine::local", "System time error: {:?}, retrying later", e);
            return Ok(()); // block and retry later
        }
    };

    // Ensure timestamp is non-decreasing (allow equality with parent)
    let timestamp = std::cmp::max(*last_timestamp, current_time);
    

    // ---------------------------------------------------------------------
    // Sanity-check local clock against timestamp in extra_data of parent block
    // ---------------------------------------------------------------------
    if let Some(parent_extra_ts_ms) = head_history.parent_extra_timestamp_ms(provider) {
        let now_ms = current_time.saturating_mul(1000);
        if exceeds_clock_skew(now_ms, parent_extra_ts_ms) {
            let delta = now_ms.abs_diff(parent_extra_ts_ms);
            warn!(target: "engine::local", delta_ms = delta, "Local clock differs from parent extra_data timestamp by > {} ms, skipping mining", MAX_CLOCK_SKEW_MS);
            return Ok(());
        }
    }
    // Reject if chosen timestamp drifts too far *into the future* relative to
    // the local wall-clock.
    let now_ms = current_time.saturating_mul(1000);
    let ts_ms = timestamp.saturating_mul(1000);
    if exceeds_clock_skew(ts_ms, now_ms) {
        warn!(
            target: "engine::local",
            "Clock ahead by > {} ms (ts_ms={}, now_ms={}). Skipping mining until wall-clock catches up",
            MAX_CLOCK_SKEW_MS,
            ts_ms,
            now_ms
        );
        return Ok(());
    }

    warn!(
        target: "engine::local",
        "timestamp ::::::::: current_time:{} -> last_timestamp:{} -> timestamp:{} -> parent_extra_ts_ms:{:?} -> block time:{}s",
        current_time,
        *last_timestamp,
        timestamp,
        head_history.parent_extra_timestamp_ms(provider),
        timestamp - *last_timestamp
    );

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

// ---------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_skew_within_bounds() {
        // delta 30 secondes < MAX_CLOCK_SKEW_MS (1 min)
        let now = 1_000_000u64;
        let prev = now + 30 * 1000;
        assert!(!exceeds_clock_skew(now, prev));
    }

    #[test]
    fn clock_skew_exceeds_bounds() {
        // delta 70 secondes > MAX_CLOCK_SKEW_MS
        let now = 1_000_000u64;
        let prev = now + 70 * 1000;
        assert!(exceeds_clock_skew(now, prev));
    }
}

