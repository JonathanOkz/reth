//! -----------------------------------------------------------------------------
//! Created by **Jonathan Okz – BaaS.sh corporate**
//! -----------------------------------------------------------------------------
//!
//! Local miner signer utilities.
//!
//! For now the miner uses a hard-coded private key for signing the block header
//! hash when `beneficiary` (a.k.a. `suggested_fee_recipient`) is non-zero.
//!
//! The signature is appended to the block header `extra_data` by
//! `EthBlockAssembler`. The first 8 bytes of `extra_data` hold the block
//! timestamp in milliseconds (big-endian `u64`) followed by 20 bytes of the
//! signer address and the 65-byte uncompressed ECDSA signature `r‖s‖v`.
//!
//! NOTE: This is *temporary* boot-strapping logic until proper CLI / config
//! wiring is in place.

use alloc::sync::Arc;

use alloy_primitives::{address, Address, B256};
use alloy_signer_local::PrivateKeySigner;
use alloy_signer::SignerSync;

use crate::header_signer::HeaderSigner;

/// Hard-coded miner private key (secp256k1).
///
/// **DO NOT USE ON MAINNET!**
const MINER_PRIVKEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Corresponding beneficiary address (`0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf`).
/// This must match the address recovered from the above private key.
pub(crate) const MINER_BENEFICIARY: Address = address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf");

/// Adapter that turns an [`alloy_signer_local::PrivateKeySigner`] into a
/// [`HeaderSigner`] implementation expected by the block assembler.
struct WalletHeaderSigner(PrivateKeySigner);

impl HeaderSigner for WalletHeaderSigner {
    fn address(&self) -> Address {
        self.0.address()
    }

    fn sign_hash(&self, hash: B256) -> [u8; 65] {
        // The LocalSigner already produces a 65-byte (r‖s‖v) signature.
        // Panic is fine here because the key is hard-coded and should always
        // be able to sign.
        let sig = self.0.sign_hash_sync(&hash).expect("sign failed");
        let bytes: [u8; 65] = (&sig).into();
        bytes
    }
}

/// Returns the default [`HeaderSigner`] for the local miner.
pub(crate) fn default_header_signer() -> Arc<dyn HeaderSigner> {
    // Strip optional 0x prefix for `FromStr` impl.
    let pk_hex = MINER_PRIVKEY_HEX.trim_start_matches("0x");
    let wallet: PrivateKeySigner = pk_hex.parse().expect("invalid hard-coded key");

    // Sanity check that the derived address matches the constant.
    debug_assert_eq!(wallet.address(), MINER_BENEFICIARY);

    Arc::new(WalletHeaderSigner(wallet))
}
