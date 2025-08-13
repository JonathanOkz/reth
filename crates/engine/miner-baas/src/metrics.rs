//! Metrics for `Miner`.
//!
//! This module defines all Prometheus metrics used by the local miner implementation.

use reth_metrics::{metrics::{Counter, Gauge, Histogram}, metrics, Metrics};

/// Metrics for `Miner`.
#[derive(Metrics, Clone)]
#[metrics(scope = "local_miner")]
pub struct MinerMetrics {
    /// Counter for errors encountered while mining.
    pub miner_errors_total: Counter,
    /// Counter for payload build timeouts (non-fatal).
    pub miner_timeouts_total: Counter,
    /// Histogram for payload build duration in seconds.
    pub payload_build_duration_seconds: Histogram,
    /// Gauge tracking block gas utilization (0-100).
    pub block_utilization_percent: Gauge,
    /// Gauge tracking pending transactions at the moment a block is mined.
    pub pending_txs_at_mine: Gauge,
    /// Gauge of current pool depth.
    pub pending_pool_depth: Gauge,
    /// Counter for successfully mined blocks.
    pub blocks_mined_total: Counter,
    /// Counter for total transactions included in mined blocks.
    pub txs_mined_total: Counter,
}
