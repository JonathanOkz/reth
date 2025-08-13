//! Signer utilities (address + header signatures) for BaaS dev & test environments.
//!
//! Exposes:
//! * `header_signer` — trait for EVM header signing
//! * `miner_signer` — default local implementation used by the dev miner.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod header_signer;
pub mod miner_signer;
