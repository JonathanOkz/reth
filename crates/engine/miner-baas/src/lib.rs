//! A local engine service that can be used to drive a dev chain.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/reth/main/assets/reth-docs.png",
    html_favicon_url = "https://avatars0.githubusercontent.com/u/97369466?s=256",
    issue_tracker_base_url = "https://github.com/paradigmxyz/reth/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod miner;
pub mod payload;
/// Adaptive gas target controller used by `Miner`.
pub mod adaptive_target;
/// Mining trigger modes (instant/debounced).
pub mod mode;
/// Prometheus metrics for the local miner.
pub mod metrics;
/// Forkchoice state and head history abstraction.
pub mod forkchoice;
/// Helper for building and submitting blocks (extracted from miner).
pub mod advance;
/// Error handling and exponential backoff helpers.
pub mod backoff;
/// Extracted run loop and mode switching helpers.
/// Hysteresis-based mining mode switch policy.
pub mod switch_policy;
/// Extracted run loop and mode switching helpers.
pub mod miner_loop;
/// Resynchronization controller and helpers.
pub mod resync;

pub use miner::Miner;
pub use mode::MiningMode;
pub use adaptive_target::AdaptiveTarget;
pub use crate::payload::PayloadAttributesBuilder;
