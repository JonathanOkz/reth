//! Time utilities for Reth primitives. Provides helpers to normalize timestamps from milliseconds to seconds at interpretation boundaries.

/// Milliseconds threshold to decide if a timestamp is in ms or s.
pub const MS_THRESHOLD: u64 = 1_000_000_000_000;
/// Milliseconds in one second.
pub const MS_PER_SEC: u64 = 1_000;

/// Normalizes a timestamp that may be provided in milliseconds to seconds.
///
/// Heuristic: if `ts >= 1_000_000_000_000` assume milliseconds and divide by 1000; else return as-is.
#[inline]
pub const fn normalize_timestamp_to_seconds(ts: u64) -> u64 {
    if ts >= MS_THRESHOLD { ts / MS_PER_SEC } else { ts }
}
