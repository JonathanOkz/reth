//! -----------------------------------------------------------------------------
//! Created by **Jonathan Okz – BaaS.sh corporate**
//! -----------------------------------------------------------------------------
//!
//! Local miner signer utilities.
//!
//! Provides a default signer backed by a hard-coded private key (for dev/test).

extern crate alloc;

use alloc::sync::Arc;

use alloy_primitives::{address, Address, B256};
use alloy_signer_local::PrivateKeySigner;
use alloy_signer::SignerSync;
use async_trait::async_trait;

use crate::header_signer::HeaderSigner;
use crate::kms::SignerError;

/// Hard-coded miner private key (secp256k1).
///
/// **DO NOT USE ON MAINNET!**
const MINER_PRIVKEY_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000001";

/// Corresponding beneficiary address (`0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf`).
/// This must match the address recovered from the above private key.
pub const MINER_BENEFICIARY: Address = address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf");

/// Adapter that turns an [`alloy_signer_local::PrivateKeySigner`] into a
/// [`HeaderSigner`] implementation expected by the block assembler.
struct WalletHeaderSigner(PrivateKeySigner);

#[async_trait]
impl HeaderSigner for WalletHeaderSigner {
    fn address(&self) -> Address {
        self.0.address()
    }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], SignerError> {
        // The LocalSigner already produces a 65-byte (r‖s‖v) signature.
        let sig = self.0.sign_hash_sync(&hash).map_err(|e| SignerError::UnknownBackend(e.to_string()))?;
        Ok((&sig).into())
    }
}

/// Returns the default [`HeaderSigner`] for the local miner.
pub fn default_header_signer() -> Arc<dyn HeaderSigner> {
    let pk_hex = MINER_PRIVKEY_HEX.trim_start_matches("0x");
    let wallet: PrivateKeySigner = pk_hex.parse().expect("invalid hard-coded key");
    debug_assert_eq!(wallet.address(), MINER_BENEFICIARY);
    Arc::new(WalletHeaderSigner(wallet))
}
