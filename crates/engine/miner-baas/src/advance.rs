//! Advance helper: build and submit a block for Miner.

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
use crate::metrics::MinerMetrics;

use alloy_consensus::{
    constants::MAXIMUM_EXTRA_DATA_SIZE
};

/// Full block build-and-submit path previously implemented inside `Miner::advance`.
///
/// Returns Ok(()) on success, otherwise an error that callers may handle with backoff.
#[allow(clippy::too_many_arguments)]
pub async fn build_and_submit_block<T, B, P, R>(
    to_engine: &BeaconConsensusEngineHandle<T>,
    payload_attributes_builder: &B,
    payload_builder: &PayloadBuilderHandle<T>,
    pool: &P,
    metrics: &MinerMetrics,
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
    // Capture current wall-clock time once and derive seconds & milliseconds.
    let current_time = match std::time::SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d, Err(e) => {
            error!(target: "engine::miner-baas", "System time error: {:?}, retrying later", e);
            return Ok(()); // block and retry later
        }
    };

    // Exit if the local wall-clock time is earlier than the last known timestamp.
    if current_time.as_secs() < *last_timestamp {
        error!(
            target: "engine::miner-baas",
            "Clock is behind (current_time={}, last_timestamp={}). Skipping mining until wall-clock catches up",
            current_time.as_secs(),
            *last_timestamp
        );
        return Ok(());
    }

    // // Prevent mining bursts: ensure that at least `burst_interval_ms` has elapsed between the parent block extra_data timestamp (in ms) and the current wall-clock millisecond portion. If not, skip this mining attempt to respect the CLI option `--mine-burst-interval-ms`.
    // if head_history.parent_extra_timestamp_ms(provider) + burst_interval_ms > current_time.as_millis() as u64 {
    //     warn!(
    //         target: "engine::miner-baas",
    //         parent_extra_ts_ms = head_history.parent_extra_timestamp_ms(provider),
    //         burst_interval_ms,
    //         now_sub_ms = current_time.as_millis(),
    //         "Skipping mining: burst interval ({} ms) since parent extra_data timestamp not yet elapsed",
    //         burst_interval_ms,
    //     );
    //     // return Ok(());
    // }
    
    // ---------------------------------------------------------------------
    // ----- Diagnostic log with parent extra_data details (BEGIN) -----
    // ---------------------------------------------------------------------
    let parent_ts_ms = head_history.parent_extra_timestamp_ms(provider);
    let delta_ms = current_time.as_millis().saturating_sub(parent_ts_ms as u128);
    warn!(
        target: "engine::miner-baas",
        "timestamp ::::::::: MAXIMUM_EXTRA_DATA_SIZE:{} | current_time:{} s -> last_timestamp:{} s -> current_time:{} ms -> last_timestamp:{:?} ms -> delta_ms:{:?} -> miner:{:?}",
        MAXIMUM_EXTRA_DATA_SIZE,
        current_time.as_secs(),
        *last_timestamp,
        current_time.as_millis(),
        parent_ts_ms,
        delta_ms,
        head_history.parent_extra_miner_address(provider)
    );
    // ----- Diagnostic log (END) -----

    // FCU with attributes to kick off payload build
    let res = to_engine
        .fork_choice_updated(
            head_history.state(),
            Some(payload_attributes_builder.build(current_time.as_secs())),
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
        debug!(target: "engine::miner-baas", "Skipping empty block (no transactions)");
        return Ok(());
    }

    // Log successful block preparation
    info!(
        target: "engine::miner-baas",
        "Prepared block with {} transactions, timestamp: {}",
        tx_count,
        current_time.as_secs()
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

    *last_timestamp = current_time.as_secs();

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
        error!(target: "engine::miner-baas", "Error updating fork choice after new block: {:?}", e);
    }

    Ok(())
}

