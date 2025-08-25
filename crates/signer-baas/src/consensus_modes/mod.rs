//! Consensus modes and signature handling strategies for BaaS.
//! This stays outside of core Reth crates to minimize merge conflicts.

extern crate alloc;

use alloc::collections::VecDeque;

use alloc::sync::Arc;
use once_cell::sync::Lazy;
use std::sync::Mutex;
use std::time::Instant;
use core::time::Duration;
use tokio::task::JoinHandle;

use alloy_consensus::Header;

use crate::header_signer::HeaderSigner;
use crate::metrics::KmsMetrics;
use crate::tx_system::publish_signature_system_tx;
use crate::outbox::{push as outbox_push, SigStatus, SignedTuple};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsensusMode {
    Standard,
    BaasConsensus, // async KMS + system tx
}

// Global mode configured at node startup.
static MODE: Lazy<std::sync::RwLock<ConsensusMode>> = Lazy::new(|| {
    std::sync::RwLock::new(ConsensusMode::Standard)
});

pub fn set_mode(mode: ConsensusMode) {
    if let Ok(mut w) = MODE.write() {
        *w = mode;
    }
}

pub fn mode() -> ConsensusMode {
    // If the lock is poisoned, fall back to Standard mode.
    if let Ok(g) = MODE.read() { *g } else { ConsensusMode::Standard }
}

// Job put in the KMS queue when BaasConsensus is active.
#[derive(Clone)]
pub struct KmsJob {
    pub block_number: u64,
    pub header_hash: alloy_primitives::B256,
    pub signer: Option<Arc<dyn HeaderSigner>>, // used by worker to sign
}

impl core::fmt::Debug for KmsJob {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KmsJob")
            .field("block_number", &self.block_number)
            .field("header_hash", &self.header_hash)
            .field("has_signer", &self.signer.is_some())
            .finish()
    }
}

// Metrics registry
static KMS_METRICS: Lazy<KmsMetrics> = Lazy::new(KmsMetrics::default);
/// FIFO queue holding block hashes awaiting signature.
static FIFO_QUEUE: Lazy<Mutex<VecDeque<KmsJob>>> = Lazy::new(|| Mutex::new(VecDeque::new()));
/// JoinHandle of the periodic scheduler.
static SCHEDULER_JOIN: Lazy<Mutex<Option<JoinHandle<()>>>> = Lazy::new(|| Mutex::new(None));

/// Initialize periodic scheduler for BaasConsensus mode.
/// Must be called once at node startup if mode == BaasConsensus.
pub fn init_async_workers() {
    if mode() != ConsensusMode::BaasConsensus {
        return;
    }
    // Ensure scheduler is started only once.
    let mut sched_guard = SCHEDULER_JOIN.lock().expect("sched poisoned");
    if sched_guard.is_some() {
        return;
    }
    let handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let maybe_job = {
                if let Ok(mut q) = FIFO_QUEUE.lock() {
                    q.pop_front()
                } else { None }
            };
            if let Some(job) = maybe_job {
                KMS_METRICS.kms_jobs_processed_total.increment(1);
                // Sign with exponential backoff (3 tries)
                if let Some(signer) = job.signer.clone() {
                    let mut tries = 0;
                    let start = Instant::now();
                    let sig_opt: Option<[u8;65]> = loop {
                        tries += 1;
                        match tokio::time::timeout(Duration::from_millis(500), signer.sign_hash(job.header_hash)).await {
                            Ok(Ok(sig)) => break Some(sig),
                            Ok(Err(err)) => {
                                tracing::error!(?err, block=job.block_number, "kms sign failed");
                            }
                            Err(_) => {
                                tracing::warn!(block=job.block_number, "kms sign timeout");
                                KMS_METRICS.kms_sign_timeout_total.increment(1);
                            }
                        }
                        if tries >= 3 { break None; }
                        KMS_METRICS.kms_sign_retries_total.increment(1);
                        tokio::time::sleep(Duration::from_millis(100 * tries)).await;
                    };
                    let elapsed = start.elapsed().as_secs_f64();
                    KMS_METRICS.kms_sign_latency_seconds.record(elapsed);
                    match sig_opt {
                        Some(sig_bytes) => {
                            KMS_METRICS.kms_sign_success_total.increment(1);
                            let mut arr = [0u8;64];
                            arr.copy_from_slice(&sig_bytes[..64]);
                            outbox_push(SignedTuple { block_hash: job.header_hash, status: SigStatus::Ok, signature: arr });
                            publish_signature_system_tx(job.block_number, job.header_hash, sig_bytes);
                        }
                        None => {
                            KMS_METRICS.kms_sign_failure_total.increment(1);
                            outbox_push(SignedTuple { block_hash: job.header_hash, status: SigStatus::Failed, signature: [0u8;64] });
                        }
                    }
                }
            }
        }
    });
    *sched_guard = Some(handle);
    KMS_METRICS.kms_worker_spawns_total.increment(1);
}

/// Called by the block assembler when a block is sealed.
/// Pushes the block hash into the FIFO queue for later signing.
pub fn on_block_sealed(header: &Header, signer: Option<&Arc<dyn HeaderSigner>>) {
    if mode() != ConsensusMode::BaasConsensus { return; }

    let job = KmsJob {
        block_number: header.number,
        header_hash: header.hash_slow(),
        signer: signer.cloned(),
    };

    if let Ok(mut guard) = FIFO_QUEUE.lock() {
        guard.push_back(job);
        KMS_METRICS.kms_jobs_enqueued_total.increment(1);
    } else {
        KMS_METRICS.kms_queue_uninit_total.increment(1);
    }
}
