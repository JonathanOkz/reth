//! Contains the implementation of the mining mode for the local engine.

use alloy_consensus::BlockHeader;
use alloy_primitives::{TxHash, B256};
use alloy_rpc_types_engine::ForkchoiceState;
use eyre::OptionExt;
use futures_util::{stream::Fuse, StreamExt};
use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{
    BuiltPayload, EngineApiMessageVersion, PayloadAttributesBuilder, PayloadKind, PayloadTypes,
};
use reth_provider::BlockReader;
use reth_transaction_pool::TransactionPool;
use std::time::{Duration, UNIX_EPOCH};
use reth_primitives_traits::block::body::BlockBody;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, warn};

/// A mining mode for the local dev engine.
#[derive(Debug)]
pub enum MiningMode {
    /// In this mode a block is built as soon as
    /// a valid transaction reaches the pool.
    Instant(Fuse<ReceiverStream<TxHash>>),
}

impl MiningMode {
    /// Constructor for a [`MiningMode::Instant`]
    pub fn instant<Pool: TransactionPool>(pool: Pool) -> Self {
        let rx = pool.pending_transactions_listener();
        Self::Instant(ReceiverStream::new(rx).fuse())
    }

}

impl MiningMode {
    /// Wait until the next mining trigger (tx arrival or interval tick).
    pub async fn wait(&mut self) {
        match self {
            Self::Instant(rx) => {
                // If the stream finished (pool listener closed), fall back to a short sleep.
                if rx.next().await.is_none() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }
}

/// Local miner advancing the chain
#[derive(Debug)]
pub struct LocalMiner<T: PayloadTypes, B, P, R>
where
    P: TransactionPool,
    R: BlockReader,
{
    /// Blockchain data provider for latest headers
    provider: R,
    /// The transaction pool
    pool: P,
    /// The payload attribute builder for the engine
    payload_attributes_builder: B,
    /// Sender for events to engine.
    to_engine: BeaconConsensusEngineHandle<T>,
    /// The mining mode for the engine
    mode: MiningMode,
    /// The payload builder for the engine
    payload_builder: PayloadBuilderHandle<T>,
    /// Target gas percentage to fill blocks under sustained load
    target_gas_percentage: u8,
    /// Tracks pending transactions between mining decisions
    last_pending_txs: usize,
    /// Timestamp for the next block.
    last_timestamp: u64,
    /// Stores latest mined blocks.
    last_block_hashes: Vec<B256>,
    /// Consecutive errors when advancing the chain – used for exponential back-off.
    consecutive_errors: u8,
}

impl<T, B, P, R> LocalMiner<T, B, P, R>
where
    T: PayloadTypes,
    B: PayloadAttributesBuilder<<T as PayloadTypes>::PayloadAttributes>,
    P: TransactionPool,
    R: BlockReader,
{
    /// Spawns a new [`LocalMiner`] with the given parameters.
    pub fn new(
        provider: R,
        payload_attributes_builder: B,
        to_engine: BeaconConsensusEngineHandle<T>,
        mode: MiningMode,
        payload_builder: PayloadBuilderHandle<T>,
        pool: P,
        target_gas_percentage: u8,
    ) -> Self {
        // Try to fetch the latest sealed header for initial state. If unavailable, fall back to
        // genesis-like defaults instead of panicking.
        let (last_timestamp, last_block_hashes) = match provider
            .best_block_number()
            .and_then(|num| provider.sealed_header(num))
        {
            Ok(Some(header)) => (header.timestamp(), vec![header.hash()]),
            Ok(None) => {
                warn!(target: "engine::local", "No header found for best block – starting with empty state");
                let genesis_hash = provider
                    .sealed_header(0)
                    .ok()
                    .flatten()
                    .map(|h| h.hash())
                    .unwrap_or_else(|| {
                        warn!(target: "engine::local", "Could not fetch genesis header; using B256::ZERO");
                        B256::ZERO
                    });
                (std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), vec![genesis_hash])
            }
            Err(err) => {
                warn!(target: "engine::local", ?err, "Error fetching best header – starting with empty state");
                let genesis_hash = provider
                    .sealed_header(0)
                    .ok()
                    .flatten()
                    .map(|h| h.hash())
                    .unwrap_or_else(|| {
                        warn!(target: "engine::local", "Could not fetch genesis header; using B256::ZERO");
                        B256::ZERO
                    });
                (std::time::SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(), vec![genesis_hash])
            }
        };

        Self {
            provider,
            pool,
            target_gas_percentage: target_gas_percentage.clamp(1, 100),
            last_pending_txs: 0,
            payload_attributes_builder,
            to_engine,
            mode,
            payload_builder,
            last_timestamp,
            last_block_hashes,
            consecutive_errors: 0,
        }
    }

    /// Runs the [`LocalMiner`] in a loop, polling the miner and building payloads.
    pub async fn run(mut self) {
        let mut fcu_interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                // Wait for the interval or the pool to receive a transaction
                _ = self.mode.wait() => {
                    let pending = self.pool.pool_size().pending;
                    // fetch latest gas limit each loop – avoid panicking on errors
                    let latest_header = match self
                        .provider
                        .best_block_number()
                        .and_then(|num| self.provider.sealed_header(num))
                    {
                        Ok(Some(h)) => h,
                        Ok(None) => {
                            warn!(target: "engine::local", "No header for best block – skipping mining decision");
                            continue;
                        }
                        Err(err) => {
                            warn!(target: "engine::local", ?err, "Error fetching best header – skipping mining decision");
                            continue;
                        }
                    };
                    let gas_limit = latest_header.gas_limit();

                    let should_mine = {
                        if pending == 0 {
                            false
                        } else if self.last_pending_txs == 0 {
                            // Premier tx après minage précédent → mine tout de suite.
                            true
                        } else {
                            // Charge continue : on attend jusqu’à remplir le bloc au seuil cible.
                            let txs = self.pool.pending_transactions();
                            if txs.is_empty() {
                                false // cas théorique, mais soyons prudents
                            } else {
                                // Overflow safe addition on 128-bit.
                                let total_gas: u128 = txs
                                    .iter()
                                    .fold(0u128, |acc, tx| acc.saturating_add(tx.gas_limit() as u128));
                                total_gas >= (gas_limit as u128 * self.target_gas_percentage as u128 / 100)
                            }
                        }
                    };

                    if should_mine {
                        if let Err(e) = self.advance().await {
                            error!(target: "engine::local", "Error advancing the chain: {:?}", e);
                            // Increment error counter and apply exponential back-off (capped at ~6.4s)
                            self.consecutive_errors = self.consecutive_errors.saturating_add(1);
                            let backoff_ms = (1u64 << self.consecutive_errors.min(6)) * 100; // 100ms,200ms,400ms,…,6400ms
                            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        } else {
                            // Success – reset error counter
                            self.consecutive_errors = 0;
                        }
                        // reset counter after mining attempt
                        self.last_pending_txs = 0;
                    } else {
                        self.last_pending_txs = pending;
                    }
                }
                // send FCU once in a while
                _ = fcu_interval.tick() => {
                    if let Err(e) = self.update_forkchoice_state().await {
                        error!(target: "engine::local", "Error updating fork choice: {:?}", e);
                    }
                }
            }
        }
    }

    /// Returns current forkchoice state.
    fn forkchoice_state(&self) -> ForkchoiceState {
        ForkchoiceState {
            head_block_hash: *self.last_block_hashes.last().unwrap_or(&B256::ZERO),
            safe_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(32))
                .unwrap_or(&B256::ZERO),
            finalized_block_hash: *self
                .last_block_hashes
                .get(self.last_block_hashes.len().saturating_sub(64))
                .unwrap_or(&B256::ZERO),
        }
    }

    /// Sends a FCU to the engine.
    async fn update_forkchoice_state(&self) -> eyre::Result<()> {
        let res = self
            .to_engine
            .fork_choice_updated(self.forkchoice_state(), None, EngineApiMessageVersion::default())
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid fork choice update")
        }

        Ok(())
    }

    /// Generates payload attributes for a new block, passes them to FCU and inserts built payload
    /// through newPayload.
    async fn advance(&mut self) -> eyre::Result<()> {
        let timestamp = std::cmp::max(
            self.last_timestamp + 1,
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );

        let res = self
            .to_engine
            .fork_choice_updated(
                self.forkchoice_state(),
                Some(self.payload_attributes_builder.build(timestamp)),
                EngineApiMessageVersion::default(),
            )
            .await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload status")
        }

        let payload_id = res.payload_id.ok_or_eyre("No payload id")?;

        // Timeout proportionnel à la taille du pool : 2 s de base + 1 s par tranche de 500 tx,
        // capé entre 2 s et 30 s.
        let pending = self.pool.pool_size().pending as u64;
        let timeout_secs = (2 + pending / 500).clamp(2, 30);

        let Some(Ok(payload)) =
            tokio::time::timeout(Duration::from_secs(timeout_secs),
                self.payload_builder.resolve_kind(payload_id, PayloadKind::WaitForPending))
                .await
                .ok()
                .flatten()
        else {
            eyre::bail!("No payload")
        };

        let block = payload.block();

        // Skip mining when block would contain zero transactions (dev --auto-mine empty tick)
        if block.body().transaction_count() == 0 {
            // nothing to mine yet
            return Ok(());
        }

        let payload = T::block_to_payload(payload.block().clone());
        let res = self.to_engine.new_payload(payload).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload")
        }

        self.last_timestamp = timestamp;
        self.last_block_hashes.push(block.hash());
        // ensure we keep at most 64 blocks
        if self.last_block_hashes.len() > 64 {
            self.last_block_hashes =
                self.last_block_hashes.split_off(self.last_block_hashes.len() - 64);
        }

        Ok(())
    }
}
