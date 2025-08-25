//! HashiCorp Vault-backed SECP256k1 signer (Transit Engine).
//!
//! Enabled with the `vault` crate feature. Requires that the specified key is
//! created in the Vault *transit* engine with `type=ecdsa-p256k1` and
//! `exportable=true`. Uses the async `reqwest` client; the signer API is now
//! entièrement asynchrone.
#![cfg(feature = "vault")]

use std::time::Duration;

use alloy_primitives::{keccak256, Address, B256};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use crate::util::{assemble_signature, find_recovery_id};
use k256::{ecdsa::Signature, EncodedPoint};
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use thiserror::Error;
use tracing::{info};

use crate::header_signer::HeaderSigner;
use async_trait::async_trait;

#[derive(Debug, Error)]
enum VaultSignerError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("vault api error: {0}")]
    Api(String),
    #[error("signature decode: {0}")]
    Decode(String),
    #[error("secp256k1 recovery failed")]
    Recover,
}

impl From<VaultSignerError> for super::SignerError {
    fn from(e: VaultSignerError) -> Self { super::SignerError::UnknownBackend(e.to_string()) }
}

/// Vault signer instance.
#[derive(Debug, Clone)]
pub struct VaultSigner {
    client: Client,
    base_url: String,
    token: Option<String>,
    key_path: String,
    address: Address,
}

impl VaultSigner {
    /// `address` – URL of Vault server (e.g. `https://vault:8200`).
    /// `token` – optional auth token. If `None`, unauthenticated requests are sent.
    /// `key_path` – full path to the transit key, e.g. `transit/keys/eth-key`.
    pub async fn new(address: String, token: Option<String>, key_path: String) -> Self {
        // Build reqwest client.
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("client");

        // Fetch public key once to derive Ethereum address.
        let pubkey = Self::fetch_pubkey(&client, &address, token.as_deref(), &key_path)
            .await
            .expect("vault public key");
        let uncompressed = EncodedPoint::from_bytes(&pubkey).expect("enc pt");
        let hash = keccak256(&uncompressed.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);

        info!(target="signer", "Initialized Vault signer for key {key_path}");
        Self { client, base_url: address, token, key_path, address: Address::from(addr) }
    }

    /// GET /v1/{key_path} to retrieve public key.
    async fn fetch_pubkey(client: &Client, base: &str, token: Option<&str>, key_path: &str) -> Result<Vec<u8>, VaultSignerError> {
        let url = format!("{base}/v1/{key_path}");
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(t) = token {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}")).unwrap());
        }
        let resp = client.get(url).headers(headers).send().await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(VaultSignerError::Api(json.to_string()));
        }
        let pubkey_b64 = json["data"]["keys"]["1"]["public_key"].as_str().ok_or_else(|| VaultSignerError::Decode("no public_key".into()))?;
        let der = B64.decode(pubkey_b64).map_err(|e| VaultSignerError::Decode(e.to_string()))?;
        Ok(der)
    }

    /// POST /v1/{key_path}/sign/
    async fn sign_digest(&self, hash: &B256) -> Result<Vec<u8>, VaultSignerError> {
        let url = format!("{}/v1/{}/sign", self.base_url, self.key_path);
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(t) = &self.token {
            headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {t}")).unwrap());
        }
        let body = serde_json::json!({ "hash_algorithm": "sha256", "signature_algorithm": "ecdsa", "input": B64.encode(hash.as_slice()) });
        let resp = self.client.post(url).headers(headers).json(&body).send().await?;
        let status = resp.status();
        let json: serde_json::Value = resp.json().await?;
        if !status.is_success() {
            return Err(VaultSignerError::Api(json.to_string()));
        }
        let sig_b64 = json["data"]["signature"].as_str().ok_or_else(|| VaultSignerError::Decode("no signature".into()))?;
        // Vault returns string like "vault:v1:BASE64" – split on last colon.
        let b64 = sig_b64.rsplit(':').next().unwrap_or(sig_b64);
        let der = B64.decode(b64).map_err(|e| VaultSignerError::Decode(e.to_string()))?;
        Ok(der)
    }

    fn der_to_eth_sig(&self, hash: &B256, der: &[u8]) -> Result<[u8; 65], VaultSignerError> {
        let sig = Signature::from_der(der).map_err(|e| VaultSignerError::Decode(e.to_string()))?;
        let r = sig.r().to_bytes();
        let s = sig.s().to_bytes();
        if let Some(recid) = find_recovery_id(hash, &r, &s, self.address) {
            Ok(assemble_signature(&r, &s, recid))
        } else {
            Err(VaultSignerError::Recover)
        }
    }
}

#[async_trait]
impl HeaderSigner for VaultSigner {
    fn address(&self) -> Address { self.address }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], super::SignerError> {
        let der = self.sign_digest(&hash).await.map_err(|e| super::SignerError::from(e))?;
        self.der_to_eth_sig(&hash, &der).map_err(Into::into)
    }
}
