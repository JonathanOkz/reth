//! Forkchoice state and block head history utilities.
//!
//! This module encapsulates the ring buffer that tracks the most recent block hashes and builds
//! the Engine API `ForkchoiceState` based on head/safe/finalized distances.

use alloy_primitives::{B256, Address};
use alloy_consensus::BlockHeader;
use tracing::trace;
use alloy_rpc_types_engine::ForkchoiceState;
use std::collections::VecDeque;

/// Number of recent heads to retain.
const HEAD_HISTORY_CAPACITY: usize = 64;
/// SAFE head lag in blocks.
const SAFE_DISTANCE: usize = 32;
/// FINALIZED head lag in blocks.
const FINALIZED_DISTANCE: usize = 64;

/// Compact ring buffer to track recent head hashes and derive forkchoice state.
#[derive(Clone, Debug, Default)]
pub struct HeadHistory {
    buf: VecDeque<B256>,
}

impl HeadHistory {
    /// Extracts the millisecond timestamp embedded in the `extra_data` field of the
    /// parent (most recent) block header.
    ///
    /// Returns millisecond timestamp from `extra_data` or `0` if unavailable/invalid.
    #[inline]
    pub fn parent_extra_timestamp_ms<P>(&self, provider: &P) -> u64
    where
        P: reth_provider::HeaderProvider,
    {
        let Some(hash) = self.last_hash() else { return 0; };
        let Some(header) = provider.header(&hash).ok().flatten() else { return 0; };
        let extra = header.extra_data();
        if extra.len() < 8 {
            trace!(target: "engine::miner-baas", "parent_extra_timestamp_ms: extra_data < 8 bytes, returning 0");
            return 0;
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&extra[..8]);
        u64::from_be_bytes(buf)
    }

    /// Extracts the miner address (20 bytes) located at bytes 32‥52 of the
    /// `extra_data` field, following the `timestamp_ms‖address‖signature` layout.
    ///
    /// Returns miner address found in `extra_data` or `Address::ZERO` if unavailable/invalid.
    #[inline]
    pub fn parent_extra_miner_address<P>(&self, provider: &P) -> Address
    where
        P: reth_provider::HeaderProvider,
    {
        let Some(hash) = self.last_hash() else { return Address::ZERO };    
        let Some(header) = provider.header(&hash).ok().flatten() else { return Address::ZERO };
        let extra = header.extra_data();
        if extra.len() < 52 {
            trace!(target: "engine::miner-baas", "parent_extra_miner_address: extra_data < 52 bytes, returning ZERO");
            return Address::ZERO;
        }
        let mut buf = [0u8; 20];
        buf.copy_from_slice(&extra[32..52]);
        Address::from_slice(&buf)
    }

    /// Create a new history initialized with an optional first head.
    pub fn new(initial: Option<B256>) -> Self {
        let mut buf = VecDeque::with_capacity(HEAD_HISTORY_CAPACITY);
        if let Some(h) = initial { buf.push_back(h); }
        Self { buf }
    }

    /// Push a new head hash, maintaining the fixed capacity.
    pub fn push(&mut self, hash: B256) {
        if self.buf.len() == HEAD_HISTORY_CAPACITY { self.buf.pop_front(); }
        self.buf.push_back(hash);
    }

    /// Current forkchoice state derived from the buffer.
    /// Returns the hash of the most recent head (`None` if history empty). This is
    /// useful for fetching the parent header from storage providers.
    #[inline]
    pub fn last_hash(&self) -> Option<B256> {
        self.buf.back().copied()
    }

    /// Compose a `ForkchoiceState` snapshot from the ring buffer.
    ///
    /// * `head_block_hash`   – newest hash.
    /// * `safe_block_hash`   – hash `SAFE_DISTANCE` blocks behind head.
    /// * `finalized_block_hash` – hash `FINALIZED_DISTANCE` blocks behind head.
    #[inline]
    pub fn state(&self) -> ForkchoiceState {
        let len = self.buf.len();
        let head = *self.buf.back().unwrap_or(&B256::ZERO);
        let safe = if len >= SAFE_DISTANCE { *self.buf.get(len - SAFE_DISTANCE).unwrap_or(&B256::ZERO) } else { B256::ZERO };
        let finalized = if len >= FINALIZED_DISTANCE { *self.buf.get(len - FINALIZED_DISTANCE).unwrap_or(&B256::ZERO) } else { B256::ZERO };
        ForkchoiceState { head_block_hash: head, safe_block_hash: safe, finalized_block_hash: finalized }
    }

    /// Whether the history has any entries.
    pub fn is_empty(&self) -> bool { self.buf.is_empty() }

    /// Number of stored heads (up to 64).
    pub fn len(&self) -> usize { self.buf.len() }
}
