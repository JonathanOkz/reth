//! -----------------------------------------------------------------------------
//! Created by **Jonathan Okz – BaaS.sh corporate**
//! -----------------------------------------------------------------------------
//!
//! Utilities for building the `extra_data` field of a block header.
//!
//! Split out of `build.rs` to isolate logic and minimize future merge conflicts.
#![allow(clippy::needless_return)]

extern crate alloc;

use once_cell::sync::Lazy;
use tokio::runtime::Runtime;

// Dedicated runtime, reused for all KMS signatures to avoid
// allocating a new runtime for every call.
static SIGN_RT: Lazy<Runtime> = Lazy::new(|| {
    Runtime::new().expect("signer dedicated runtime")
});

/// Quick validation of a compact Ethereum signature (r‖s‖v).
fn is_valid_eth_sig(sig: &[u8; 65]) -> bool {
    let v = sig[64];
    (v == 0 || v == 1) && sig[..64].iter().any(|&b| b != 0)
}

use alloc::{sync::Arc, vec::Vec};

use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::Bytes;

/// Build the `extra_data` bytes (vanity timestamp + optional `address`).
///
/// * `timestamp_secs` – Header timestamp in **seconds**.
/// * `signer`         – Optional header signer (beneficiary address & signature).
/// * `header`         – Reference header used to compute signing hash when `signer` present.
pub(crate) fn build_extra_data_sync(
    _timestamp_secs: u64,
    signer: Option<&Arc<dyn crate::header_signer::HeaderSigner>>, // borrow, no cloning needed
    _header: &alloy_consensus::Header, 
) -> Bytes {
    // ---------------------------------------------------------------------
    // 1. Millisecond timestamp (big-endian u64)
    // ---------------------------------------------------------------------
    let timestamp_ms: u64 = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(dur) => dur
            .as_millis()
            .min(u128::from(u64::MAX) as u128) as u64,
        Err(err) => {
            tracing::error!(?err, "failed to read system time, defaulting extra_data timestamp_ms to 0");
            0
        }
    };

    // Warn if the ms timestamp diverges too much
    {
        let delta_ms = ((timestamp_ms as i128) - (_timestamp_secs as i128 * 1000)).abs();
        if delta_ms > 1_000 {
            tracing::warn!(
                delta_ms,
                "extra_data timestamp_ms diverges from header.timestamp by >1s"
            );
        }
    }

    // 32-byte vanity: first 8 bytes = timestamp_ms, rest zero-padding
    let mut vanity = [0u8; 32];
    vanity[..8].copy_from_slice(&timestamp_ms.to_be_bytes());
    let mut ed: Vec<u8> = vanity.to_vec();

    // ---------------------------------------------------------------------
    // 2. Optional `address` when signer present (no signature in sync path)
    // ---------------------------------------------------------------------
    if let Some(signer) = signer {
        ed.extend_from_slice(signer.address().as_slice());
    }

    Bytes::from(ed)
}

/// Build the `extra_data` bytes synchronously and include a signer-produced signature if available.
///
/// This function remains synchronous for compatibility with the sync `BlockAssembler` trait, while
/// avoiding blocking a Tokio worker by using `tokio::task::block_in_place` and `Handle::block_on`.
pub(crate) fn build_extra_data(
    timestamp_secs: u64,
    signer: Option<&Arc<dyn crate::header_signer::HeaderSigner>>,
    header: &alloy_consensus::Header,
) -> Bytes {
    let mut ed = build_extra_data_sync(timestamp_secs, signer, header).to_vec();

    if let Some(signer) = signer {
        // Recompute hash with current extra_data prefix (timestamp + optional address)
        let mut tmp_header = header.clone();
        tmp_header.extra_data = Bytes::from(ed.clone());

        // Execute the signature in a blocking thread so as not to block the Tokio worker.
        use alloc::sync::Arc;
        use core::time::Duration;

        let signer_arc: Arc<dyn crate::header_signer::HeaderSigner> = Arc::clone(signer);
        let hash = tmp_header.hash_slow();

        // Launch the blocking task for signing using the static runtime.
        let handle = tokio::task::spawn_blocking(move || {
            // Cooperative timeout _within_ the blocking thread so that the future is actually canceled.
            SIGN_RT.block_on(async {
                match tokio::time::timeout(Duration::from_millis(500), signer_arc.sign_hash(hash)).await {
                    Ok(res) => res,
                    Err(_) => Err(signer_baas::kms::SignerError::Io(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "sign_hash timeout",
                    ))),
                }
            })
        });

        // Wait for the JoinHandle (quick, already timed-out inside)
        let join_res = tokio::runtime::Handle::current().block_on(handle);

        match join_res {
            // The future timeout completed; parse the JoinHandle.
            Ok(join_res) => match join_res {
                Ok(sig) => {
                    if is_valid_eth_sig(&sig) {
                        ed.extend_from_slice(&sig);
                    } else {
                        tracing::error!("invalid signature format; omitting signature in extra_data");
                    }
                }
                Err(err) => tracing::error!(?err, "sign_hash failed; omitting signature in extra_data"),
            },
            // This branch should no longer occur (internal timeout) but is kept for safety.
            Err(join_err) => {
                tracing::error!(?join_err, "spawn_blocking join failed; omitting signature in extra_data");
            }
        }
    }

    Bytes::from(ed)
}
