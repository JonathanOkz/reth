//! Resynchronization controller for Miner.
//!
//! This module encapsulates detection of repeated invalid FCU/newPayload outcomes and exposes
//! lightweight gating helpers and the top-level resync routine operating on `Miner`.
//! The resync execution lives here; `miner.rs` only holds state and minimal utilities.

use std::time::{Duration, Instant};
use tracing::{warn, error, info};
use alloy_consensus::BlockHeader;
use reth_payload_primitives::{PayloadAttributesBuilder, PayloadTypes};
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use crate::forkchoice::HeadHistory;
use crate::miner::Miner;

/// Configuration for resynchronization behavior.
#[derive(Clone, Debug)]
pub struct ResyncConfig {
    /// Number of consecutive invalid newPayload results required to trigger resync.
    pub invalid_new_payload_threshold: u32,
    /// Number of consecutive invalid forkchoice updates required to trigger resync.
    pub invalid_fcu_threshold: u32,
    /// Minimum time between two resyncs to avoid thrashing.
    pub min_resync_dwell: Duration,
    /// Max number of recent heads to re-read during resync (upper bound).
    pub max_replay: usize,
}

impl Default for ResyncConfig {
    fn default() -> Self {
        Self {
            invalid_new_payload_threshold: 3,
            invalid_fcu_threshold: 3,
            min_resync_dwell: Duration::from_millis(2000),
            max_replay: 64,
        }
    }
}

/// Controller that tracks invalid streaks and performs resync when needed.
#[derive(Debug)]
pub struct ResyncController {
    cfg: ResyncConfig,
    consecutive_invalid_new_payload: u32,
    consecutive_invalid_fcu: u32,
    last_resync_at: Instant,
    in_progress: bool,
}

impl ResyncController {
    /// Creates a new resync controller with the provided configuration.
    pub fn new(cfg: ResyncConfig) -> Self {
        // Initialize last_resync_at in the past so the first eligible resync isn't blocked by dwell
        let now = Instant::now();
        let last = now
            .checked_sub(cfg.min_resync_dwell)
            .unwrap_or(now);
        Self {
            cfg,
            consecutive_invalid_new_payload: 0,
            consecutive_invalid_fcu: 0,
            last_resync_at: last,
            in_progress: false,
        }
    }

    /// Whether a resync is currently ongoing (the miner loop can briefly skip work).
    pub const fn in_progress(&self) -> bool { self.in_progress }

    /// Record an FCU outcome and return whether a resync should be performed.
    ///
    /// This does not mutate miner state; callers can invoke `perform_resync()` if this returns true.
    pub fn on_fcu_flag(&mut self, is_valid: bool) -> bool {
        if is_valid {
            self.consecutive_invalid_fcu = 0;
            return false;
        }
        self.consecutive_invalid_fcu = self.consecutive_invalid_fcu.saturating_add(1);
        self.consecutive_invalid_fcu >= self.cfg.invalid_fcu_threshold
            && self.last_resync_at.elapsed() >= self.cfg.min_resync_dwell
    }

    /// Inspect an `advance()` error and return whether a resync should be performed.
    ///
    /// Heuristic classification based on error string avoids plumbing new error types through the
    /// stack while keeping testability (pure counter logic here).
    pub fn on_advance_error_flag(&mut self, err: &eyre::Report) -> bool {
        let s = err.to_string();
        let dbg = format!("{err:?}");
        let is_invalid_new_payload = s.contains("Invalid payload")
            || s.contains("Invalid payload status")
            || dbg.contains("Invalid payload");

        if is_invalid_new_payload {
            self.consecutive_invalid_new_payload = self.consecutive_invalid_new_payload.saturating_add(1);
        } else {
            // Non-invalid errors reset the new_payload streak to avoid false positives
            self.consecutive_invalid_new_payload = 0;
        }

        self.consecutive_invalid_new_payload >= self.cfg.invalid_new_payload_threshold
            && self.last_resync_at.elapsed() >= self.cfg.min_resync_dwell
    }

    /// Check if resync should be performed.
    pub const fn should_resync(&self) -> bool {
        self.consecutive_invalid_new_payload >= self.cfg.invalid_new_payload_threshold
            || self.consecutive_invalid_fcu >= self.cfg.invalid_fcu_threshold
    }

    /// Attempt to mark a resync start if dwell has elapsed and none is in progress.
    /// Returns true if the caller should proceed with the resync routine.
    pub fn start_if_due(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_resync_at) < self.cfg.min_resync_dwell || self.in_progress {
            return false;
        }
        self.in_progress = true;
        true
    }

    /// Mark resync finished and reset counters/timers.
    pub fn finish(&mut self) {
        self.consecutive_invalid_new_payload = 0;
        self.consecutive_invalid_fcu = 0;
        self.last_resync_at = Instant::now();
        self.in_progress = false;
    }

    /// Returns the maximum number of headers to replay during resync.
    pub const fn max_replay(&self) -> usize { self.cfg.max_replay }
}

/// Execute the resync routine using the provided `Miner`.
/// This coordinates via the miner's internal `ResyncController` to ensure throttling and
/// single-flight semantics. Idempotent if called while resync is throttled or in progress.
pub async fn perform_resync<T, B, P, R>(miner: &mut Miner<T, B, P, R>, reason: &'static str)
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool + Clone + Send + 'static,
    R: BlockReader,
{
    if !miner.resync.start_if_due() { return; }

    let started = Instant::now();
    warn!(target: "engine::miner-baas", "Resync starting (reason={})", reason);

    // Re-read best head and up to max_replay-1 previous heads
    let provider = &miner._provider;
    let best_num = match provider.best_block_number() {
        Ok(n) => n,
        Err(e) => {
            error!(target: "engine::miner-baas", "Resync: failed to read best block number: {:?}", e);
            miner.resync.finish();
            return;
        }
    };

    let max_replay = miner.resync.max_replay();
    let start_num = best_num.saturating_sub(max_replay.saturating_sub(1) as u64);
    let mut history = HeadHistory::new(None);
    let mut last_ts = miner.last_timestamp;
    let mut had_head = false;
    for n in start_num..=best_num {
        match provider.sealed_header(n) {
            Ok(Some(h)) => {
                history.push(h.hash());
                last_ts = h.timestamp();
                if n == best_num { had_head = true; }
            }
            Ok(None) => {}
            Err(e) => {
                warn!(target: "engine::miner-baas", "Resync: error reading header {}: {:?}", n, e);
            }
        }
    }

    // Require that the history includes the current best head; otherwise skip applying state.
    if !had_head {
        warn!(target: "engine::miner-baas", "Resync: failed to read best head {} ; skipping state apply", best_num);
        miner.resync.finish();
        return;
    }

    // Apply refreshed state and realign engine
    miner.last_timestamp = miner.last_timestamp.max(last_ts);
    miner.head_history = history;
    if let Err(e) = miner.update_forkchoice_state().await {
        error!(target: "engine::miner-baas", "Resync: FCU failed after refresh: {:?}", e);
    }

    // Mark done and log duration
    miner.resync.finish();
    let elapsed = started.elapsed();
    info!(target: "engine::miner-baas", "Resync completed in {:?} (reason={})", elapsed, reason);
}
