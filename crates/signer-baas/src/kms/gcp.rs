    //! Google Cloud KMS-backed SECP256k1 signer.
//!
//! Enabled with the `gcp` crate feature. Uses the async `google-cloud-kms` SDK
//! with a fully asynchrone initialization and signing path.
#![cfg(feature = "gcp")]

use std::sync::Arc;

use alloy_primitives::{keccak256, Address, B256};
use base64::Engine;
use crate::util::{assemble_signature, find_recovery_id};
use google_cloud_kms::client::{Client, ClientConfig};
use google_cloud_kms::grpc::kms::v1::{digest as gdigest, Digest, GetPublicKeyRequest, AsymmetricSignRequest};
use k256::{ecdsa::Signature, EncodedPoint, PublicKey as K256Pub};
use k256::pkcs8::DecodePublicKey;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use thiserror::Error;
use tracing::info;

use crate::header_signer::HeaderSigner;
use async_trait::async_trait;

#[derive(Debug, Error)]
enum GcpSignerError {
    #[error("DER/base64 decoding error: {0}")]
    Decode(#[from] Box<dyn std::error::Error + Send + Sync>),
    #[error("secp256k1 recovery failed")]
    Recover,
}

impl From<GcpSignerError> for super::SignerError {
    fn from(e: GcpSignerError) -> Self {
        super::SignerError::UnknownBackend(e.to_string())
    }
}

/// Google Cloud signer instance.
#[derive(Debug, Clone)]
pub struct GcpKmsSigner {
    client: Arc<Client>,
    key_name: String,
    address: Address,
}

impl GcpKmsSigner {
    /// Build a new signer.
    ///
    /// `key_name` must be the *CryptoKeyVersion* resource name, e.g.:
    /// `projects/PROJECT/locations/LOCATION/keyRings/RING/cryptoKeys/KEY/cryptoKeyVersions/1`
    /// If `credentials_file` is `None` the default GCP auth chain is used.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        project_id: String,
        location: String,
        key_ring: String,
        key: String,
        credentials_file: Option<String>,
    ) -> Self {
        // Build auth config asynchronously.
        let cfg = if let Some(path) = credentials_file {
            let cred: google_cloud_kms::client::google_cloud_auth::credentials::CredentialsFile =
                serde_json::from_str(&std::fs::read_to_string(&path).expect("read creds")).expect("parse creds");
            ClientConfig::default().with_credentials(cred).await.expect("cfg w/ cred")
        } else {
            ClientConfig::default().with_auth().await.expect("cfg auth")
        };
        let client = Client::new(cfg).await.expect("client init");

        // Build full key version name (v1 by default).
        let key_name = format!(
            "projects/{project_id}/locations/{location}/keyRings/{key_ring}/cryptoKeys/{key}/cryptoKeyVersions/1"
        );

        // Fetch public key once to derive address.
        let pub_key_resp = client.get_public_key(GetPublicKeyRequest { name: key_name.clone() }, None).await.expect("get_public_key");
        let pem = pub_key_resp.pem;
        let der_bytes = {
            let pem_str = pem.trim();
            // Strip PEM headers.
            let b64 = pem_str
                .lines()
                .filter(|l| !l.starts_with("-----"))
                .collect::<String>();
            let decoder = base64::engine::general_purpose::STANDARD;
            decoder.decode(b64.as_bytes()).expect("base64 decode pem")
        };
        let pk = K256Pub::from_public_key_der(&der_bytes).expect("invalid der");
        let uncompressed: EncodedPoint = pk.to_encoded_point(false);
        let hash = keccak256(&uncompressed.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);

        info!(target="signer", "Initialized GCP KMS signer: {key_name}");
        Self { client: Arc::new(client), key_name, address: Address::from(addr) }
    }

    fn der_to_eth_sig(&self, hash: &B256, der: &[u8]) -> Result<[u8; 65], GcpSignerError> {
        let sig = Signature::from_der(der).map_err(|e| GcpSignerError::Decode(Box::new(e)))?;
        let r = sig.r().to_bytes();
        let s = sig.s().to_bytes();
        if let Some(v) = find_recovery_id(hash, &r, &s, self.address) {
            return Ok(assemble_signature(&r, &s, v));
        }
        Err(GcpSignerError::Recover)
    }
}

#[async_trait]
impl HeaderSigner for GcpKmsSigner {
    fn address(&self) -> Address { self.address }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], super::SignerError> {
        // Build digest proto (sha256 oneof)
        let digest = Digest { digest: Some(gdigest::Digest::Sha256(hash.to_vec())) };
        let req = AsymmetricSignRequest { name: self.key_name.clone(), digest: Some(digest), ..Default::default() };
        let resp = self.client.asymmetric_sign(req, None).await.map_err(|e| super::SignerError::UnknownBackend(e.to_string()))?;
        let sig_der = resp.signature;
        self.der_to_eth_sig(&hash, &sig_der).map_err(Into::into)
    }
}
