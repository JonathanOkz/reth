//! Simple global outbox for signed block tuples (hash, status, signature).
//! Consumed by block assembler to embed into `extra_data`.

extern crate alloc;

use alloc::collections::VecDeque;
use once_cell::sync::Lazy;
use std::sync::Mutex;

use alloy_primitives::B256;
use alloy_primitives::Bytes;

/// ExtraData schema constants.
pub const EXTRA_DATA_VERSION: u8 = 1;
/// Status codes for the signature tuple.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigStatus {
    /// Signature OK.
    Ok = 0,
    /// Failed after retries.
    Failed = 1,
}

impl From<SigStatus> for u8 {
    fn from(s: SigStatus) -> Self {
        s as Self
    }
}

/// Tuple stored in the outbox.
#[derive(Clone, Debug)]
pub struct SignedTuple {
    pub block_hash: B256,
    pub status: SigStatus,
    pub signature: [u8; 64],
}

impl SignedTuple {
    /// Encode to 98-byte `extra_data` payload.
    pub fn encode(&self) -> Bytes {
        let mut out = [0u8; 98];
        out[0] = EXTRA_DATA_VERSION;
        out[1] = self.status as u8;
        out[2..34].copy_from_slice(self.block_hash.as_slice());
        out[34..].copy_from_slice(&self.signature);
        Bytes::copy_from_slice(&out)
    }
}

static OUTBOX: Lazy<Mutex<VecDeque<SignedTuple>>> = Lazy::new(|| Mutex::new(VecDeque::new()));

/// Push a signed tuple into the global outbox.
pub fn push(tuple: SignedTuple) {
    if let Ok(mut guard) = OUTBOX.lock() {
        guard.push_back(tuple);
    }
}

/// Pop the next tuple to include in a block, if any.
pub fn pop() -> Option<SignedTuple> {
    if let Ok(mut guard) = OUTBOX.lock() {
        guard.pop_front()
    } else {
        None
    }
}
