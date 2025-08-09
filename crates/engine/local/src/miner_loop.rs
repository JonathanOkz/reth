//! Miner event loop and mode switching logic extracted from `miner.rs`.

use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use reth_transaction_pool::TransactionPool;

use crate::mode::MiningMode;
use crate::switch_policy::SwitchDecision;
use crate::miner::LocalMiner;
use reth_payload_primitives::{PayloadAttributesBuilder, PayloadTypes};
use reth_provider::BlockReader;

impl<T, B, P, R> LocalMiner<T, B, P, R>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool + Clone + Send + 'static,
    R: BlockReader,
{
    /// Run the miner with a shutdown token for graceful termination.
    ///
    /// This mirrors the original `run()` logic but listens for `shutdown.cancelled()` and exits
    /// cleanly, letting `MiningMode` drop abort background tasks.
    pub async fn run_with_shutdown(mut self, shutdown: CancellationToken) {

        // Ensure reasonable interval bounds
        let interval_ms = std::cmp::max(self.burst_interval_ms, 100);
        let mut burst_interval = interval(Duration::from_millis(interval_ms));
        burst_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut fcu_interval = interval(Duration::from_secs(1));
        fcu_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Avoid too-frequent mining
        let mut last_mine_attempt = std::time::Instant::now();
        let min_mine_interval = Duration::from_millis(50);

        info!(
            target: "engine::local",
            "LocalMiner started: enter_burst_threshold={}, exit_burst_threshold={}, dwell_ms={}, burst_interval={}ms",
            self.policy.enter_burst_threshold,
            self.policy.exit_burst_threshold,
            self.policy.min_mode_dwell.as_millis() as u64,
            interval_ms
        );

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!(target: "engine::local", "Shutdown signal received; stopping LocalMiner loop");
                    break;
                }

                // Triggered on every new pending tx (instant mode)
                _ = self.mode.wait() => {
                    // If a resync is happening, skip work this tick
                    if self.resync.in_progress() { continue; }

                    let pending_usize = self.pool.pool_size().pending as usize;

                    // Switch between Instant and Debounced dynamically using hysteresis policy
                    self.switch_mode_if_needed(pending_usize, interval_ms);

                    let pending = pending_usize;

                    // Prevent mining too frequently
                    let now = std::time::Instant::now();
                    if now.duration_since(last_mine_attempt) < min_mine_interval {
                        continue;
                    }

                    // Mine immediately only in Instant mode if adaptive target says so
                    if matches!(&self.mode, MiningMode::Instant(_)) && self.adaptive.should_mine(pending) {
                        last_mine_attempt = now;

                        if let Err(e) = self.advance().await {
                            error!(target: "engine::local", "Error advancing the chain: {:?}", e);
                            self.handle_error().await;
                            // If errors look like repeated invalid newPayload, trigger resync
                            let should = self.resync.on_advance_error_flag(&e);
                            if should { crate::resync::perform_resync(&mut self, "new_payload").await; }
                        } else {
                            self.reset_state_after_success();
                        }
                    }
                    // If in Debounced, let burst interval handle it
                }

                // Burst mining interval - for high transaction loads
                _ = burst_interval.tick() => {
                    // If a resync is happening, skip work this tick
                    if self.resync.in_progress() { continue; }

                    let pending = self.pool.pool_size().pending as usize;

                    // In Debounced mode, mine on cadence while pending is above the exit threshold
                    if matches!(&self.mode, MiningMode::Debounced { .. }) && pending >= self.policy.exit_burst_threshold {
                        last_mine_attempt = std::time::Instant::now();

                        if let Err(e) = self.advance().await {
                            error!(target: "engine::local", "Error advancing the chain (burst): {:?}", e);
                            self.handle_error().await;
                            // If errors look like repeated invalid newPayload, trigger resync
                            let should = self.resync.on_advance_error_flag(&e);
                            if should { crate::resync::perform_resync(&mut self, "new_payload").await; }
                        } else {
                            self.reset_state_after_success();

                            // Log successful burst mining
                            info!(
                                target: "engine::local",
                                "Burst mined block with {} pending transactions",
                                pending as u64
                            );
                        }
                    }
                }

                // send FCU once in a while
                _ = fcu_interval.tick() => {
                    // If a resync is happening, skip work this tick
                    if self.resync.in_progress() { continue; }

                    let fcu_ok = match self.update_forkchoice_state().await {
                        Ok(()) => true,
                        Err(e) => {
                            error!(target: "engine::local", "Error updating fork choice: {:?}", e);
                            false
                        }
                    };

                    // Update FCU invalid streaks and maybe trigger resync
                    let trigger = self.resync.on_fcu_flag(fcu_ok);
                    if trigger { crate::resync::perform_resync(&mut self, "fcu").await; }
                }
            }
        }

        info!(target: "engine::local", "LocalMiner loop exited");
    }

    /// Helper that toggles between `Instant` and `Debounced` modes based on pool size.
    fn switch_mode_if_needed(&mut self, pending: usize, interval_ms: u64) {
        let now = std::time::Instant::now();
        if let Some(decision) = self.policy.should_switch(&self.mode, pending, self.last_mode_switch_at, now) {
            match decision {
                SwitchDecision::ToDebounced => {
                    info!(target: "engine::local", "Switching mining mode → Debounced (pending={}, enter_threshold={})", pending, self.policy.enter_burst_threshold);
                    self.mode = MiningMode::debounced(self.pool.clone(), interval_ms.clamp(50, 500));
                    self.last_mode_switch_at = now;
                }
                SwitchDecision::ToInstant => {
                    info!(target: "engine::local", "Switching mining mode → Instant (pending={}, exit_threshold={})", pending, self.policy.exit_burst_threshold);
                    self.mode = MiningMode::instant(self.pool.clone());
                    self.last_mode_switch_at = now;
                }
            }
        }
    }
}
