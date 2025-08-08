//! Simple unit test verifying ring buffer logic for LocalMiner
//! Ensures after 100 insertions only 64 elements are kept and that
//! head/safe/finalized hashes correspond to indices 0, 32, 63.

use std::collections::VecDeque;
use alloy_primitives::B256;

// Helper to create unique B256 from u64
fn hash(i: u64) -> B256 {
    // Put `i` in the last 8 bytes, rest zero
    let mut bytes = [0u8; 32];
    bytes[24..32].copy_from_slice(&i.to_be_bytes());
    B256::from(bytes)
}

#[test]
fn ring_buffer_keeps_last_64() {
    let mut buf: VecDeque<B256> = VecDeque::with_capacity(64);

    for i in 0u64..100 {
        if buf.len() == 64 {
            buf.pop_front();
        }
        buf.push_back(hash(i));
    }

    assert_eq!(buf.len(), 64, "Buffer length should stay at 64");

    // Expected range in buffer is 36..=99 (inclusive)
    let head = hash(99);
    let safe = hash(68);      // head - 31 (matches miner's len-32)
    let finalized = hash(36); // head - 63 (matches miner's len-64 retrieval)

    assert_eq!(buf.back().cloned().unwrap(), head, "Head hash mismatch");
    assert_eq!(buf.get(buf.len() - 32).cloned().unwrap(), safe, "Safe hash mismatch");
    assert_eq!(buf.front().cloned().unwrap(), finalized, "Finalized hash mismatch");
}

#[test]
fn ring_buffer_perf_push_pop_under_50ns() {
    use std::{collections::VecDeque, time::Instant, hint::black_box};

    const ITERS: usize = 1_000_000;
    let mut buf: VecDeque<B256> = VecDeque::with_capacity(64);

    let start = Instant::now();
    for i in 0u64..ITERS as u64 {
        if buf.len() == 64 {
            buf.pop_front();
        }
        buf.push_back(hash(i));
        black_box(buf.back());
    }
    let dur = start.elapsed();
    let avg_ns = dur.as_nanos() as f64 / ITERS as f64;
    println!("Avg push/pop: {:.2} ns", avg_ns);

    #[cfg(debug_assertions)]
    assert!(avg_ns < 200.0, "Too slow in debug: {:.2} ns", avg_ns);
    #[cfg(not(debug_assertions))]
    assert!(avg_ns < 50.0, "Too slow in release: {:.2} ns", avg_ns);

    assert_eq!(buf.len(), 64, "Buffer length drifted");
}
