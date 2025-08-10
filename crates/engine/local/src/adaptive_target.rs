//! Adaptive gas target algorithm used by `LocalMiner`.
//!
//! This module exposes [`AdaptiveTarget`], a lightweight controller that
//! decides **how many transactions** should be packed into the next
//! block in order to keep gas utilisation near a configurable target.
//! The algorithm is designed for private / dev networks with a fixed
//! gas limit and no fees.

use std::cmp::{max, min};

/// Exponentially-weighted moving average (EWMA) configuration.
#[derive(Debug, Clone)]
pub struct GasAvgConfig {
    /// Initial value for the EWMA of *gas per transaction*.
    pub initial_avg_tx_gas: f64,
    /// Smoothing factor in the range **(0, 1]**. Higher → faster reaction.
    pub alpha: f64,
}

/// PID controller configuration (proportional + derivative terms).
#[derive(Debug, Clone)]
pub struct GasLimitConfig {
    /// Proportional gain coefficient that scales the instantaneous error.
    pub kp: f64,
    /// Derivative gain coefficient that reacts to the change in error.
    pub kd: f64,
}

/// Hard bounds for the number of transactions allowed in a block.
#[derive(Debug, Clone)]
pub struct TxBounds {
    /// Minimum number of transactions allowed in a block.
    pub min: usize,
    /// Maximum number of transactions allowed in a block.
    pub max: usize,
}

/// All parameters required to initialise an [`AdaptiveTarget`].
#[derive(Debug, Clone)]
pub struct AdaptiveTargetConfig {
    /// Fixed block gas limit used to compute utilisation.
    pub gas_limit: u64,
    /// Initial target utilisation expressed as a percentage of `gas_limit`.
    pub target_gas_percent: f64,
    /// Configuration parameters for the EWMA of *gas per transaction*.
    pub gas_avg_config: GasAvgConfig,
    /// PID controller coefficients for utilisation error adjustment.
    pub gas_limit_config: GasLimitConfig,
    /// Hard bounds on the transaction count target.
    pub tx_bounds: TxBounds,
}

/// Adaptive gas target algorithm responsible for computing the desired
/// number of transactions per block based on the current *average* gas
/// used by transactions that have already been mined.
///
/// The objective is to keep the block gas utilisation close to
/// `target_gas_percent` of the block gas limit.  Because transaction
/// complexity (and hence gas usage) varies over time, we adapt the
/// **transaction count target** instead of the gas limit itself.  This
/// prevents the miner from producing over-sized or under-utilised
/// blocks when the mix of simple / complex transactions changes.
///
/// Algorithm outline
/// -----------------
/// 1. Track an exponentially-weighted moving average (EWMA) of the
///    observed gas per transaction (`avg_tx_gas`).
/// 2. Compute the desired gas per block as:
///    `desired_block_gas = gas_limit * target_gas_percent / 100.0`
/// 3. Derive the transaction target as:
///    `tx_target = desired_block_gas / avg_tx_gas`
/// 4. Clamp `tx_target` so it always stays within `[min_tx_per_block, max_tx_per_block]`.
///
/// The resulting `tx_target` can be queried via [`Self::tx_target`] and
/// should be re-evaluated each time the miner wants to decide whether
/// to seal a new block.
#[derive(Debug, Clone)]
pub struct AdaptiveTarget {
    /// Fixed block gas limit (private network => constant).
    gas_limit: u64,
    /// Target block utilisation in percentage (0-100).
    target_gas_percent: f64,
    /// EWMA of gas consumed per transaction.
    avg_tx_gas: f64,
    /// EWMA smoothing factor for gas/tx in range (0,1].  Higher => faster reaction.
    alpha_gas: f64,

    // --- PID controller coefficients ---
    kp: f64,
    kd: f64,
    prev_error: f64,
    /// Hard bounds for tx count per block.
    tx_bounds: TxBounds,
}

impl AdaptiveTarget {
    /// Create a new [`AdaptiveTarget`] from a bundled [`AdaptiveTargetConfig`].
    pub fn new(cfg: AdaptiveTargetConfig) -> Self {
        debug_assert!(cfg.gas_limit > 0, "gas_limit must be > 0");
        debug_assert!((0.0..=1.0).contains(&cfg.gas_avg_config.alpha));
        debug_assert!(cfg.gas_limit_config.kp >= 0.0 && cfg.gas_limit_config.kd >= 0.0, "kp and kd must be non-negative");
        debug_assert!(cfg.tx_bounds.min >= 1 && cfg.tx_bounds.min <= cfg.tx_bounds.max);

        Self {
            gas_limit: cfg.gas_limit,
            target_gas_percent: cfg.target_gas_percent,
            avg_tx_gas: cfg.gas_avg_config.initial_avg_tx_gas,
            alpha_gas: if cfg.gas_avg_config.alpha == 0.0 { 0.6 } else { cfg.gas_avg_config.alpha },
            kp: cfg.gas_limit_config.kp,
            kd: cfg.gas_limit_config.kd,
            prev_error: 0.0,
            tx_bounds: cfg.tx_bounds,
        }
    }

    /// Update the EWMA with the gas usage of a newly mined block.
    pub fn on_mined_block(&mut self, block_gas_used: u64, tx_count: usize) {
        if tx_count == 0 {
            return; // skip empty blocks
        }
        // --- 1. Mettre à jour la moyenne de gas par transaction -------------------
        let observed_avg = block_gas_used as f64 / tx_count as f64;
        self.avg_tx_gas = self
            .alpha_gas
            .mul_add(observed_avg, (1.0 - self.alpha_gas) * self.avg_tx_gas);

        // --- 2. Ajuster dynamiquement la cible de remplissage ---------------------
        let observed_fill_percent = block_gas_used as f64 * 100.0 / self.gas_limit as f64;

        // --- 2a. PID controller on utilisation ----------------------------------
        let error = observed_fill_percent - self.target_gas_percent;
        let delta = self.kp.mul_add(error, self.kd * (error - self.prev_error));
        self.prev_error = error;
        self.target_gas_percent += delta;

        // Bornes finales
        self.target_gas_percent = self.target_gas_percent.clamp(2.0, 95.0);
    }

    /// Calculate the current *transaction count target* based on the
    /// latest EWMA.
    pub fn tx_target(&self) -> usize {
        let desired_block_gas = self.gas_limit as f64 * self.target_gas_percent / 100.0;
        // Defensive: ensure denominator is at least 1 gas to avoid division-by-zero or huge values.
        let denom = self.avg_tx_gas.max(1.0);
        let raw_target = (desired_block_gas / denom).round() as i64;
        let clamped = max(
            self.tx_bounds.min as i64,
            min(self.tx_bounds.max as i64, raw_target),
        );
        clamped as usize
    }

    /// Decide whether the miner should seal a block given the current
    /// number of pending transactions.
    #[inline]
    pub fn should_mine(&self, pending_txs: usize) -> bool {
        pending_txs >= self.tx_target()
    }
}
