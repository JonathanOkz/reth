//! AWS KMS-backed SECP256k1 signer.
//!
//! Enabled with the `aws` crate feature. Relies on the async `aws-sdk-kms`.
//! The `sign_hash` method is **blocking**: it synchronously waits on the async
//! AWS request using the current Tokio runtime handle. This keeps the external
//! `HeaderSigner` trait synchronous while avoiding a full refactor.
//!
//! In production, prefer driving signing on a dedicated async task.
#![cfg(feature = "aws")]

use std::sync::Arc;

use alloy_primitives::{keccak256, Address, B256};
use crate::util::{assemble_signature, find_recovery_id};
use aws_config::meta::region::RegionProviderChain;
use aws_sdk_kms as kms;
use kms::config::Region as AwsRegion;
use kms::primitives::Blob;
use kms::types::SigningAlgorithmSpec;
use kms::Client;
use k256::{ecdsa::Signature, PublicKey as K256Pub};
use k256::pkcs8::DecodePublicKey;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use thiserror::Error;
// use tracing::{debug, info};

use crate::header_signer::HeaderSigner;
use async_trait::async_trait;

#[derive(Debug, Error)]
enum AwsSignerError {
    #[error("aws error: {0}")]
    Aws(#[from] kms::Error),
    #[error("DER decoding error: {0}")]
    Der(#[from] k256::ecdsa::Error),
    #[error("secp256k1 recovery failed")]
    Recover,
}

impl From<AwsSignerError> for super::SignerError {
    fn from(e: AwsSignerError) -> Self {
        super::SignerError::UnknownBackend(e.to_string())
    }
}

/// AWS signer instance.
#[derive(Debug, Clone)]
pub struct AwsKmsSigner {
    client: Arc<Client>,
    key_id: String,
    address: Address,
}

impl AwsKmsSigner {
    /// Build a new signer.
    ///
    /// * `region` – AWS region, e.g. `us-east-1`.
    /// * `key_id` – KMS key ID or ARN.
    /// * `access_key`/`secret_key` – optional explicit creds; otherwise default chain.
    pub async fn new(region: String, key_id: String, _access_key: Option<String>, _secret_key: Option<String>) -> Self {
        // Build AWS config via async loader.
        let region_provider = RegionProviderChain::first_try(Some(AwsRegion::new(region.clone())))
            .or_default_provider();
        let loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(region_provider);
        let cfg = loader.load().await;
        let client = Client::new(&cfg);

        // Fetch the public key once to derive address.
        let rsp = client
            .get_public_key()
            .key_id(&key_id)
            .send()
            .await
            .expect("get_public_key");
        let pubkey_der = rsp.public_key.expect("no pubkey returned").as_ref().to_vec();
        let pk = K256Pub::from_public_key_der(&pubkey_der).expect("invalid der");
        let uncompressed = pk.to_encoded_point(false);
        let hash = keccak256(&uncompressed.as_bytes()[1..]);
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&hash[12..]);

        Self { client: Arc::new(client), key_id, address: Address::from(addr) }
    }

    fn der_to_eth_sig(&self, hash: &B256, der: &[u8]) -> Result<[u8; 65], AwsSignerError> {
        let sig = Signature::from_der(der)?;
        let r = sig.r().to_bytes();
        let s = sig.s().to_bytes();

        // Try both recovery ids.
        if let Some(recid) = find_recovery_id(hash, &r, &s, self.address) {
            Ok(assemble_signature(&r, &s, recid))
        } else {
            Err(AwsSignerError::Recover)
        }
    }
}

#[async_trait]
impl HeaderSigner for AwsKmsSigner {
    fn address(&self) -> Address { self.address }

    async fn sign_hash(&self, hash: B256) -> Result<[u8; 65], super::SignerError> {
        let resp = self.client
            .sign()
            .key_id(&self.key_id)
            .message_type(kms::types::MessageType::Digest)
            .message(Blob::new(hash.to_vec()))
            .signing_algorithm(SigningAlgorithmSpec::EcdsaSha256)
            .send()
            .await
            .map_err(|e| super::SignerError::UnknownBackend(e.to_string()))?;
        let der = resp
            .signature
            .ok_or_else(|| super::SignerError::UnknownBackend("no sig".into()))?
            .as_ref()
            .to_vec();
        self.der_to_eth_sig(&hash, &der).map_err(Into::into)
    }
}
