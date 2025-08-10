//! Mode switching policy with hysteresis and minimum dwell time.
//!
//! This module defines [`ModeSwitchPolicy`], a small, production-ready policy
//! for deciding when to switch between `Instant` and `Debounced` mining modes
//! in `LocalMiner`. It prevents rapid oscillations by using:
//! - Separate enter/exit thresholds (hysteresis)
//! - A minimum dwell time between switches

use std::time::{Duration, Instant};

use crate::mode::MiningMode;

/// Decision result indicating whether a mode switch should occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDecision {
    /// Switch to `Instant` mode.
    ToInstant,
    /// Switch to `Debounced` mode.
    ToDebounced,
}

/// Configurable policy to control mode switches.
#[derive(Debug, Clone)]
pub struct ModeSwitchPolicy {
    /// Pending tx threshold to ENTER burst (Debounced) mode.
    pub enter_burst_threshold: usize,
    /// Pending tx threshold to EXIT burst mode back to Instant.
    /// Must be strictly less than `enter_burst_threshold`.
    pub exit_burst_threshold: usize,
    /// Minimum time to stay in a given mode before another switch is allowed.
    pub min_mode_dwell: Duration,
}

impl ModeSwitchPolicy {
    /// Create a new policy.
    pub fn new(enter_burst_threshold: usize, exit_burst_threshold: usize, min_mode_dwell: Duration) -> Self {
        debug_assert!(enter_burst_threshold > 0, "enter threshold must be > 0");
        debug_assert!(exit_burst_threshold > 0, "exit threshold must be > 0");
        debug_assert!(exit_burst_threshold < enter_burst_threshold, "exit threshold must be < enter threshold");
        debug_assert!(min_mode_dwell >= Duration::from_millis(1));
        Self { enter_burst_threshold, exit_burst_threshold, min_mode_dwell }
    }

    /// Decide if a switch should happen based on current mode and pending txs.
    pub fn should_switch(
        &self,
        current_mode: &MiningMode,
        pending_txs: usize,
        last_switch_at: Instant,
        now: Instant,
    ) -> Option<SwitchDecision> {
        // Enforce minimum dwell time
        if now.duration_since(last_switch_at) < self.min_mode_dwell {
            return None;
        }

        match current_mode {
            MiningMode::Instant(_) => {
                (pending_txs >= self.enter_burst_threshold)
                    .then_some(SwitchDecision::ToDebounced)
            }
            MiningMode::Debounced { .. } => {
                (pending_txs <= self.exit_burst_threshold)
                    .then_some(SwitchDecision::ToInstant)
            }
        }
    }
}
