#![allow(missing_docs)]
//! KMS-backed and local signers registry for BaaS.
//!
//! All concrete back-ends implement the common [`HeaderSigner`] trait so they
//! can be plugged anywhere in the node (miner, JSON-RPC, etc.).
//!
//! Use [`SignerRegistry::from_config`] to load a JSON configuration at runtime.

use core::fmt;
use std::{collections::HashMap, fs, path::Path};

use alloc::sync::Arc;

use alloy_primitives::{Address, B256};
use serde::Deserialize;
use tracing::{info, warn};

use crate::header_signer::HeaderSigner;
use async_trait::async_trait;

#[cfg(feature = "aws")] mod aws;
#[cfg(feature = "gcp")] mod gcp;
#[cfg(feature = "vault")] mod vault;

#[derive(Debug, thiserror::Error)]
pub enum SignerError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown backend '{0}' (feature disabled)")]
    UnknownBackend(String),
    #[error("key '{0}' not found in registry")]
    KeyNotFound(String),
    #[error("duplicate key name '{0}' in config file")]
    DuplicateKey(String),
    #[error("keystore error: {0}")]
    Keystore(#[from] crate::local::KeystoreError),
}

// === SignerKMS =============================================================
/// Unified enum that wraps all supported signer implementations.
#[allow(clippy::large_enum_variant)]
pub enum SignerKMS {
    Local(crate::local::LocalSigner),
    #[cfg(feature = "aws")] Aws(aws::AwsKmsSigner),
    #[cfg(feature = "gcp")] Gcp(gcp::GcpKmsSigner),
    #[cfg(feature = "vault")] Vault(vault::VaultSigner),
}

impl fmt::Debug for SignerKMS {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(_) => f.write_str("SignerKMS::Local"),
            #[cfg(feature = "aws")] Self::Aws(_) => f.write_str("SignerKMS::Aws"),
            #[cfg(feature = "gcp")] Self::Gcp(_) => f.write_str("SignerKMS::Gcp"),
            #[cfg(feature = "vault")] Self::Vault(_) => f.write_str("SignerKMS::Vault"),
        }
    }
}

#[async_trait]
impl HeaderSigner for SignerKMS {
    fn address(&self) -> Address {
        match self {
            Self::Local(s) => s.address(),
            #[cfg(feature = "aws")] Self::Aws(s) => s.address(),
            #[cfg(feature = "gcp")] Self::Gcp(s) => s.address(),
            #[cfg(feature = "vault")] Self::Vault(s) => s.address(),
        }
    }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], SignerError> {
        match self {
            Self::Local(s) => s.sign_hash(hash).await,
            #[cfg(feature = "aws")] Self::Aws(s) => s.sign_hash(hash).await,
            #[cfg(feature = "gcp")] Self::Gcp(s) => s.sign_hash(hash).await,
            #[cfg(feature = "vault")] Self::Vault(s) => s.sign_hash(hash).await,
        }
    }
}

// === Config parsing ========================================================
#[derive(Debug, Deserialize)]
struct SignerFile {
    default_key: String,
    keys: Vec<KeyEntry>,
}

#[derive(Debug, Deserialize)]
struct KeyEntry {
    name: String,
    #[serde(flatten)]
    cfg: KeyConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
enum KeyConfig {
    /// Local key stored on this host (no remote auth).
    /// At least one of the sources must be provided.
    Local {
        /// Raw hex string (0x…)
        #[serde(default)]
        private_key_hex: Option<String>,
        /// Name of an env var that contains the hex key
        #[serde(default)]
        env_var: Option<String>,
        /// Local keystore file (Web3 JSON). Mutually exclusive with other fields.
        #[serde(default)]
        keystore: Option<crate::local::KeystoreConfig>,
    },
    #[cfg(feature = "aws")]
    Aws {
        region: String,
        key_id: String,
        access_key: Option<String>,
        secret_key: Option<String>,
    },
    #[cfg(feature = "gcp")]
    Gcp {
        project_id: String,
        location: String,
        key_ring: String,
        key: String,
        credentials_file: Option<String>,
    },
    #[cfg(feature = "vault")]
    Vault {
        address: String,
        token: Option<String>,
        key_path: String,
    },
    #[cfg(not(any(feature = "aws", feature = "gcp", feature = "vault")))]
    #[serde(other)]
    _Unsupported,
}

impl KeyConfig {
    async fn build_signer(self) -> Result<SignerKMS, SignerError> {
        match self {
            Self::Local { private_key_hex, env_var, keystore } => {
                if let Some(cfg) = keystore {
                    let pk_hex = cfg.load_hex()?;
                    return Ok(SignerKMS::Local(crate::local::LocalSigner::from_hex(pk_hex)?));
                }
                let pk_hex = if let Some(hex) = private_key_hex {
                    Some(hex)
                } else if let Some(var) = env_var {
                    std::env::var(&var).ok()

                } else {
                    None
                }
                .ok_or_else(|| SignerError::UnknownBackend("no local key provided".into()))?;
                Ok(SignerKMS::Local(crate::local::LocalSigner::from_hex(pk_hex)?))
            }
            #[cfg(feature = "aws")]
            Self::Aws { region, key_id, access_key, secret_key } => Ok(SignerKMS::Aws(
                aws::AwsKmsSigner::new(region, key_id, access_key, secret_key).await
            )),
            #[cfg(feature = "gcp")]
            Self::Gcp { project_id, location, key_ring, key, credentials_file } => Ok(
                SignerKMS::Gcp(gcp::GcpKmsSigner::new(project_id, location, key_ring, key, credentials_file).await),
            ),
            #[cfg(feature = "vault")]
            Self::Vault { address, token, key_path } => Ok(SignerKMS::Vault(
                vault::VaultSigner::new(address, token, key_path).await
            )),
            #[allow(unreachable_patterns)]
            _ => Err(SignerError::UnknownBackend("disabled".into())),
        }
    }
}

// === Registry ==============================================================

/// Holds a map of KMS/local signers keyed by name.
pub struct SignerRegistry {
    #[allow(clippy::type_complexity)]
    signers: HashMap<String, Arc<dyn HeaderSigner>>,
    default: String,
}

impl fmt::Debug for SignerRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&String> = self.signers.keys().collect();
        f.debug_struct("SignerRegistry")
            .field("keys", &keys)
            .field("default", &self.default)
            .finish()
    }
}

impl SignerRegistry {
    /// Load the registry from a JSON configuration file.
    pub async fn from_config(path: impl AsRef<Path>) -> Result<Self, SignerError> {
        let text = fs::read_to_string(&path)?;
        let file: SignerFile = serde_json::from_str(&text)?;

        let mut signers: HashMap<String, Arc<dyn HeaderSigner>> = HashMap::with_capacity(file.keys.len());
        for entry in file.keys {
            if signers.contains_key(&entry.name) {
                return Err(SignerError::DuplicateKey(entry.name));
            }
            let is_local = matches!(&entry.cfg, KeyConfig::Local { .. });
            let signer_kms = entry.cfg.build_signer().await?;
            let signer: Arc<dyn HeaderSigner> = Arc::new(signer_kms);
            if is_local {
                warn!(target: "signer", "Using local signer '{}' — DO NOT use in production", entry.name);
            }
            signers.insert(entry.name.clone(), signer);
        }

        if !signers.contains_key(&file.default_key) {
            return Err(SignerError::KeyNotFound(file.default_key));
        }
        info!(target="signer", "Loaded {} signer(s) from {:?}", signers.len(), path.as_ref());
        Ok(Self { signers, default: file.default_key })
    }

    /// Returns the requested signer or the default one if `name` is `None`.
    pub fn get(&self, name: Option<&str>) -> Result<Arc<dyn HeaderSigner>, SignerError> {
        let key = name.unwrap_or(&self.default);
        self.signers
            .get(key)
            .cloned()
            .ok_or_else(|| SignerError::KeyNotFound(key.to_owned()))
    }
}

// === Tests =================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_local_config() {
        let json = r#"{
            "default_key": "dev",
            "keys": [
                { "name": "dev", "backend": "local", "private_key_hex": "0x01" }
            ]
        }"#;
        let file: SignerFile = serde_json::from_str(json).unwrap();
        assert_eq!(file.default_key, "dev");
        assert_eq!(file.keys.len(), 1);
    }
}
