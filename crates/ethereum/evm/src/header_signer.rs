//! -----------------------------------------------------------------------------
//! Created by **Jonathan Okz – BaaS.sh corporate**
//! -----------------------------------------------------------------------------
//!
//! Trait for signing Ethereum execution headers in a Clique-like fashion.
//!
//! The `HeaderSigner` is injected into `EthBlockAssembler`. When present it is
//! used to generate the 65-byte ECDSA seal that is appended to `extra_data`.
//! The final layout becomes:
//!
//! ```text
//! | 32 bytes vanity | 20 bytes signer address | 65 bytes signature |
//! ```
//!
//! * The first 8 bytes of the vanity are the block timestamp in **milliseconds**
//!   encoded as big-endian `u64`.
//! * The remaining 24 vanity bytes are zero-padding.
//!
//! Implementations are free to provide the signing backend of their choice
//! (software key, HSM, remote signer …).
//!
//! Only the hash of the RLP-encoded header (with vanity + address, **without**
//! the signature) is provided to the signer.

use alloy_primitives::{Address, B256};

/// A type that can sign the hash of a header and expose its Ethereum address.
pub trait HeaderSigner: Send + Sync + 'static {
    /// Returns the 20-byte address corresponding to the signer public key.
    fn address(&self) -> Address;

    /// Signs the given Keccak-256 hash of the header RLP **without** the
    /// signature. Must return the **compact** 65-byte representation `r‖s‖v`.
    fn sign_hash(&self, hash: B256) -> [u8; 65];
}
