//! Forkchoice state and block head history utilities.
//!
//! This module encapsulates the ring buffer that tracks the most recent block hashes and builds
//! the Engine API `ForkchoiceState` based on head/safe/finalized distances.

use alloy_primitives::B256;
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
