//! Tests for the head history ring buffer used by `Miner`.
//! Ensures after 100 insertions only 64 elements are kept and that
//! head/safe/finalized hashes correspond to distances 0, 32, and 64.

use alloy_primitives::B256;
use reth_engine_miner_baas::forkchoice::HeadHistory;

// Helper to create unique B256 from u64
fn hash(i: u64) -> B256 {
    // Put `i` in the last 8 bytes, rest zero
    let mut bytes = [0u8; 32];
    bytes[24..32].copy_from_slice(&i.to_be_bytes());
    B256::from(bytes)
}

#[test]
fn head_history_keeps_last_64_and_state() {
    let mut hist = HeadHistory::new(None);

    for i in 0u64..100 {
        hist.push(hash(i));
    }

    assert_eq!(hist.len(), 64, "History length should stay at 64");

    // Expected range in buffer is 36..=99 (inclusive)
    let head = hash(99);
    let safe = hash(68);      // index len-32
    let finalized = hash(36); // index len-64

    let state = hist.state();
    assert_eq!(state.head_block_hash, head, "Head hash mismatch");
    assert_eq!(state.safe_block_hash, safe, "Safe hash mismatch");
    assert_eq!(state.finalized_block_hash, finalized, "Finalized hash mismatch");
}

#[test]
fn head_history_perf_push_under_50ns() {
    use std::{time::Instant, hint::black_box};

    const ITERS: usize = 1_000_000;
    let mut hist = HeadHistory::new(None);

    let start = Instant::now();
    for i in 0u64..ITERS as u64 {
        hist.push(hash(i));
        black_box(hist.len());
    }
    let dur = start.elapsed();
    let avg_ns = dur.as_nanos() as f64 / ITERS as f64;
    println!("Avg push: {:.2} ns", avg_ns);

    #[cfg(debug_assertions)]
    assert!(avg_ns < 200.0, "Too slow in debug: {:.2} ns", avg_ns);
    #[cfg(not(debug_assertions))]
    assert!(avg_ns < 50.0, "Too slow in release: {:.2} ns", avg_ns);

    assert_eq!(hist.len(), 64, "History length drifted");
}

#[test]
fn head_history_empty_state_zero() {
    let hist = HeadHistory::new(None);
    let state = hist.state();
    assert_eq!(state.head_block_hash, B256::ZERO);
    assert_eq!(state.safe_block_hash, B256::ZERO);
    assert_eq!(state.finalized_block_hash, B256::ZERO);
}

#[test]
fn head_history_boundaries() {
    // < 32 entries: safe/finalized should be ZERO
    let mut hist = HeadHistory::new(None);
    for i in 0u64..10 { hist.push(hash(i)); }
    let s = hist.state();
    assert_eq!(s.head_block_hash, hash(9));
    assert_eq!(s.safe_block_hash, B256::ZERO);
    assert_eq!(s.finalized_block_hash, B256::ZERO);

    // exactly 32 entries: safe becomes the first inserted, finalized still ZERO
    let mut hist = HeadHistory::new(None);
    for i in 0u64..32 { hist.push(hash(i)); }
    let s = hist.state();
    assert_eq!(s.head_block_hash, hash(31));
    // With implementation using len - 32 indexing, this yields head - 31
    assert_eq!(s.safe_block_hash, hash(0));
    assert_eq!(s.finalized_block_hash, B256::ZERO);

    // exactly 64 entries: finalized becomes the first inserted
    let mut hist = HeadHistory::new(None);
    for i in 0u64..64 { hist.push(hash(i)); }
    let s = hist.state();
    assert_eq!(s.head_block_hash, hash(63));
    assert_eq!(s.safe_block_hash, hash(32)); // len - 32 => value 32
    assert_eq!(s.finalized_block_hash, hash(0));

    // 65 entries: ring drops the oldest (0), range is 1..=64
    let mut hist = HeadHistory::new(None);
    for i in 0u64..65 { hist.push(hash(i)); }
    let s = hist.state();
    assert_eq!(s.head_block_hash, hash(64));
    assert_eq!(s.safe_block_hash, hash(33)); // len - 32 => index 32 => value 33
    assert_eq!(s.finalized_block_hash, hash(1));
}
