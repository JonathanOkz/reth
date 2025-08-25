//! Minimal stub for publishing a signature as a system transaction.
//! This can be replaced with a real implementation that enqueues an internal tx
//! into the node's transaction processing pipeline.

use alloy_primitives::B256;

/// Publish a signature for the given block as a system transaction.
///
/// Currently a no-op that logs the intent.
#[inline]
pub fn publish_signature_system_tx(block_number: u64, header_hash: B256, signature: [u8; 65]) {
    let _ = signature; // suppress unused for now
    tracing::info!(
        block = block_number,
        ?header_hash,
        "(stub) publish_signature_system_tx called"
    );
}
