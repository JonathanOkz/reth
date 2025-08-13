//! -----------------------------------------------------------------------------
//! Created by **Jonathan Okz – BaaS.sh corporate**
//! -----------------------------------------------------------------------------
//!
//! Utilities for building the `extra_data` field of a block header.
//!
//! Split out of `build.rs` to isolate logic and minimize future merge conflicts.
#![allow(clippy::needless_return)]

extern crate alloc;

use alloc::{sync::Arc, vec::Vec};

#[cfg(feature = "std")] use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::Bytes;

/// Build the `extra_data` bytes (vanity timestamp + optional `[address | signature]`).
///
/// * `timestamp_secs` – Header timestamp in **seconds**.
/// * `signer`         – Optional header signer (beneficiary address & signature).
/// * `header`         – Reference header used to compute signing hash when `signer` present.
pub(crate) fn build_extra_data(
    _timestamp_secs: u64,
    signer: Option<&Arc<dyn crate::header_signer::HeaderSigner>>, // borrow, no cloning needed
    header: &alloy_consensus::Header, 
) -> Bytes {
    // ---------------------------------------------------------------------
    // 1. Millisecond timestamp (big-endian u64)
    // ---------------------------------------------------------------------
    let timestamp_ms: u64 = {
        #[cfg(feature = "std")]
        {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX) as u128) as u64
        }
        #[cfg(not(feature = "std"))]
        {
            // Fallback: no reliable clock in `no_std`; use 0 to signal "unknown seal time".
            0
        }
    };

    // Warn if the ms timestamp diverges too much
    #[cfg(feature = "std")]
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
    // 2. Optional `[address | signature]` when signer present
    // ---------------------------------------------------------------------
    if let Some(signer) = signer {
        ed.extend_from_slice(signer.address().as_slice());
        let mut tmp_header = header.clone();
        tmp_header.extra_data = Bytes::from(ed.clone());
        let sig = signer.sign_hash(tmp_header.hash_slow());
        ed.extend_from_slice(&sig);
    }

    Bytes::from(ed)
}
