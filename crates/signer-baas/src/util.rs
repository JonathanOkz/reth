//! Shared utilities for signer-baas crate.

#![allow(dead_code)]

use alloy_primitives::{keccak256, Address, B256};
use k256::{
    ecdsa::{RecoveryId, Signature as FixedSig, VerifyingKey},
    FieldBytes,
};

/// Recover the Ethereum recovery id (0/1) for the given `(r,s)` signature such
/// that the recovered public key hashes to `expected`.
/// Returns `None` if no recovery id matches.
pub(crate) fn find_recovery_id(hash: &B256, r: &FieldBytes, s: &FieldBytes, expected: Address) -> Option<u8> {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(r);
    buf[32..].copy_from_slice(s);
    let sig = FixedSig::from_slice(&buf).ok()?;
    for recid in 0..=1u8 {
        // Try with x_reduced = false which corresponds to 0/1 ids
        let rid = RecoveryId::from_byte(recid)?;
        let vk = VerifyingKey::recover_from_prehash(hash.as_slice(), &sig, rid).ok()?;
        let uncompressed = vk.to_encoded_point(false);
        let h = keccak256(&uncompressed.as_bytes()[1..]);
        if &h[12..] == expected.as_slice() {
            return Some(recid);
        }
    }
    None
}

/// Assemble `(r,s,v)` into 65-byte array.
pub(crate) fn assemble_signature(r: &FieldBytes, s: &FieldBytes, v: u8) -> [u8; 65] {
    let mut out = [0u8; 65];
    out[..32].copy_from_slice(r);
    out[32..64].copy_from_slice(s);
    out[64] = v;
    out
}
