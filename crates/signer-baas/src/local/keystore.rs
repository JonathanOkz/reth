//! Web3 (Ethereum V3) keystore file support.
//! NOTE: Decryption is **NOT** yet implemented; placeholder to unblock config parsing.

use std::path::PathBuf;

use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum KeystoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("decrypt error: unsupported (TODO)")]
    Decrypt,
}

/// JSON-serialisable keystore config section.
#[derive(Debug, Deserialize)]
pub struct KeystoreConfig {
    /// Path to the keystore JSON file on disk.
    pub path: PathBuf,
    /// Environment variable containing the password.
    #[serde(default)]
    pub password_env: Option<String>,
    /// Clear-text password (discouraged – use only in dev).
    #[serde(default)]
    pub password: Option<String>,
}

impl KeystoreConfig {
    /// Load and decrypt the keystore, returning the private key hex string.
    /// Currently returns dummy key until decryption support is added.
    pub fn load_hex(&self) -> Result<String, KeystoreError> {
        warn!(target = "signer", "Keystore decryption not implemented – using dummy private key");
        Ok("0x01".to_string())
    }
}
