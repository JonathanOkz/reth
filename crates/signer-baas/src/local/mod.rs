//! Local signer backend.
//! WARNING: For development/testing only. Do **not** use in production.

use alloy_primitives::{Address, B256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;

use crate::header_signer::HeaderSigner;
use crate::kms::SignerError;

mod keystore;
pub use keystore::{KeystoreConfig, KeystoreError};

/// Local signer backed by an in-memory secp256k1 private key.
#[derive(Clone, Debug)]
pub struct LocalSigner(PrivateKeySigner);

impl LocalSigner {
    pub fn from_hex(pk_hex: String) -> Result<Self, SignerError> {
        let pk_hex = pk_hex.trim_start_matches("0x");
        let signer: PrivateKeySigner = pk_hex.parse().map_err(|_| SignerError::UnknownBackend("invalid private key hex".into()))?;
        Ok(Self(signer))
    }
}

#[async_trait]
impl HeaderSigner for LocalSigner {
    fn address(&self) -> Address {
        self.0.address()
    }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], SignerError> {
        let sig = self.0.sign_hash_sync(&hash).map_err(|e| SignerError::UnknownBackend(e.to_string()))?;
        Ok((&sig).into())
    }
}
