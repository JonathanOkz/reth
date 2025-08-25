//! Signer utilities (address + header signatures) for BaaS dev & test environments.
//!
//! Exposes:
//! * `header_signer` — trait for EVM header signing
//! * `miner_signer` — default local implementation used by the dev miner.
#![cfg_attr(not(feature = "std"), no_std)]
#![allow(missing_docs, dead_code, clippy::doc_markdown)]

extern crate alloc;

pub mod header_signer;
pub mod miner_signer;
pub mod kms;
pub mod local;
#[cfg(any(feature = "aws", feature = "gcp", feature = "vault"))]
mod util;
/// Consensus modes and async KMS queue/worker.
pub mod consensus_modes;
/// Prometheus metrics for this crate.
pub mod metrics;
/// Stub system-transaction publisher for signatures.
pub mod tx_system;
/// Global outbox for signed tuples to be embedded in blocks.
pub mod outbox;
