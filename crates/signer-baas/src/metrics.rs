//! Prometheus metrics for signer-baas KMS async worker.

use reth_metrics::{metrics::{Counter, Histogram}, metrics, Metrics};

/// Metrics for async KMS signing worker and queue.
#[derive(Metrics, Clone)]
#[metrics(scope = "signer_baas")]
pub struct KmsMetrics {
    /// Total KMS jobs enqueued.
    pub kms_jobs_enqueued_total: Counter,
    /// Total KMS jobs dequeued/processed by the worker.
    pub kms_jobs_processed_total: Counter,
    /// Total KMS sign operations that succeeded.
    pub kms_sign_success_total: Counter,
    /// Total KMS sign operations that failed after retries.
    pub kms_sign_failure_total: Counter,
    /// Total KMS sign timeouts across all attempts.
    pub kms_sign_timeout_total: Counter,
    /// Total number of retry attempts (excluding the first attempt).
    pub kms_sign_retries_total: Counter,
    /// Enqueue failures due to full/closed channel.
    pub kms_queue_full_total: Counter,
    /// Attempts to enqueue when the queue was not initialized.
    pub kms_queue_uninit_total: Counter,
    /// Number of times the worker was spawned.
    pub kms_worker_spawns_total: Counter,
    /// Signing latency in seconds (time from dequeue to final result).
    pub kms_sign_latency_seconds: Histogram,
}
