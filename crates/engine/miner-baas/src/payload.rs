//! The implementation of the [`PayloadAttributesBuilder`] for the
//! [`Miner`](super::Miner).

use alloy_primitives::{address, Address, B256};

/// Hard-coded beneficiary address matching the miner-baas signer.
const MINER_BENEFICIARY: Address = address!("7E5F4552091A69125d5DfCb7b8C2659029395Bdf");
use reth_chainspec::EthereumHardforks;
use reth_ethereum_engine_primitives::EthPayloadAttributes;
use reth_payload_primitives::PayloadAttributesBuilder as PayloadAttributesBuilderTrait;
use std::sync::Arc;

/// The attributes builder for BaaS miner payload.
#[derive(Debug)]
#[non_exhaustive]
pub struct PayloadAttributesBuilder<ChainSpec> {
    /// The chainspec
    pub chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec> PayloadAttributesBuilder<ChainSpec> {
    /// Creates a new instance of the builder.
    pub const fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl<ChainSpec> PayloadAttributesBuilderTrait<EthPayloadAttributes>
    for PayloadAttributesBuilder<ChainSpec>
where
    ChainSpec: Send + Sync + EthereumHardforks + 'static,
{
    fn build(&self, timestamp: u64) -> EthPayloadAttributes {
        EthPayloadAttributes {
            timestamp,
            prev_randao: B256::random(),
            suggested_fee_recipient: MINER_BENEFICIARY,
            withdrawals: self
                .chain_spec
                .is_shanghai_active_at_timestamp(timestamp)
                .then(Default::default),
            parent_beacon_block_root: self
                .chain_spec
                .is_cancun_active_at_timestamp(timestamp)
                .then(B256::random),
        }
    }
}

#[cfg(feature = "op")]
impl<ChainSpec> reth_payload_primitives::PayloadAttributesBuilder<op_alloy_rpc_types_engine::OpPayloadAttributes>
    for PayloadAttributesBuilder<ChainSpec>
where
    ChainSpec: Send + Sync + EthereumHardforks + 'static,
{
    fn build(&self, timestamp: u64) -> op_alloy_rpc_types_engine::OpPayloadAttributes {
        op_alloy_rpc_types_engine::OpPayloadAttributes {
            payload_attributes: self.build(timestamp),
            // Add dummy system transaction
            transactions: Some(vec![
                reth_optimism_chainspec::constants::TX_SET_L1_BLOCK_OP_MAINNET_BLOCK_124665056
                    .into(),
            ]),
            no_tx_pool: None,
            gas_limit: None,
            eip_1559_params: None,
        }
    }
}
