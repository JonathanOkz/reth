//! Error backoff utility for Miner.

use std::time::Duration;
use tracing::{error, warn};

use crate::metrics::MinerMetrics;

/// Handle an error with exponential backoff and cooldown.
///
/// - Increments `miner_errors_total` metric
/// - Increments `consecutive_errors`
/// - Sleeps with exponential backoff up to 30s
/// - After 10 consecutive errors, sleeps 60s and resets counter
pub async fn handle_with_backoff(
    consecutive_errors: &mut u8,
    metrics: &MinerMetrics,
) {
    // Increment error metric and counter
    metrics.miner_errors_total.increment(1);
    *consecutive_errors = consecutive_errors.saturating_add(1);

    // Cooldown after too many consecutive errors
    if *consecutive_errors > 10 {
        error!(target: "engine::miner-baas", "Too many consecutive errors ({}), entering 60s cooldown", *consecutive_errors);
        tokio::time::sleep(Duration::from_secs(60)).await;
        // reset error counter after cooldown
        *consecutive_errors = 0;
        return;
    }

    // Exponential backoff with max cap at 30 seconds
    let wait_time = Duration::from_millis(
        100u64.saturating_mul(2u64.saturating_pow((*consecutive_errors).min(10) as u32)),
    );
    let capped_wait = std::cmp::min(wait_time, Duration::from_secs(30));

    warn!(
        target: "engine::miner-baas",
        "Error #{}, waiting {:?} before retry",
        *consecutive_errors,
        capped_wait
    );

    tokio::time::sleep(capped_wait).await;
}
