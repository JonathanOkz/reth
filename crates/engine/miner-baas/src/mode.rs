//! Mining trigger modes used by `Miner`.
//! 
//! This module provides an event-driven Instant mode (wakes on new pending tx)
//! and a Debounced mode (merges bursts and emits at most one trigger per period).
//! Both are production-safe and avoid busy waiting.

use alloy_primitives::TxHash;
use futures_util::{stream::Fuse, StreamExt};
use futures_util::future::{AbortHandle, Abortable};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use reth_transaction_pool::TransactionPool;

/// Mining trigger modes.
#[derive(Debug)]
pub enum MiningMode {
    /// Instant mode: build a block as soon as a valid tx reaches the pool
    Instant(Fuse<ReceiverStream<TxHash>>),
    /// Debounced mode: emits at most one trigger per period when busy.
    /// Stores abort handles for its background tasks so they can be
    /// cancelled on mode switch, avoiding orphan tasks.
    Debounced {
        /// Receives a single trigger per debounce period while load is high.
        rx: mpsc::Receiver<()>,
        /// Abort handle for the background tx-listener task started by debounced mode.
        listener_abort: AbortHandle,
        /// Abort handle for the periodic ticker task that emits debounced triggers.
        ticker_abort: AbortHandle,
    },
}

impl MiningMode {
    /// Constructor for a [`MiningMode::Instant`]
    pub fn instant<Pool: TransactionPool>(pool: Pool) -> Self {
        let rx = pool.pending_transactions_listener();
        Self::Instant(ReceiverStream::new(rx).fuse())
    }

    /// Constructor for debounced mode.
    /// The debounce period is clamped in [50ms, 500ms] for safety.
    pub fn debounced<Pool>(pool: Pool, period_ms: u64) -> Self
    where
        Pool: TransactionPool + Clone + Send + 'static,
    {
        let period_ms = period_ms.clamp(50, 500);
        let flag = Arc::new(AtomicBool::new(false));
        let (tx_alert, rx_alert) = mpsc::channel::<()>(1);

        // Listener task that sets the flag on every new tx
        let flag_l = Arc::clone(&flag);
        let mut listener = pool.pending_transactions_listener();
        let (listener_abort, listener_reg) = AbortHandle::new_pair();
        tokio::spawn(Abortable::new(async move {
            while listener.recv().await.is_some() {
                flag_l.store(true, Ordering::Relaxed);
            }
        }, listener_reg));

        // Timer task that checks the flag periodically and emits a single trigger
        let flag_t = Arc::clone(&flag);
        let (ticker_abort, ticker_reg) = AbortHandle::new_pair();
        tokio::spawn(Abortable::new(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(period_ms));
            loop {
                ticker.tick().await;
                if flag_t.swap(false, Ordering::Relaxed) {
                    let _ = tx_alert.try_send(());
                }
            }
        }, ticker_reg));

        Self::Debounced { rx: rx_alert, listener_abort, ticker_abort }
    }

    /// Wait until the next mining trigger (tx arrival or interval tick).
    pub async fn wait(&mut self) {
        match self {
            Self::Instant(rx) => {
                // If the stream finished (pool listener closed), fall back to a short sleep.
                if rx.next().await.is_none() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            Self::Debounced { rx, .. } => {
                // One tick per debounce period while busy
                let _ = rx.recv().await;
            }
        }
    }
}

impl Drop for MiningMode {
    fn drop(&mut self) {
        if let Self::Debounced { listener_abort, ticker_abort, .. } = self {
            listener_abort.abort();
            ticker_abort.abort();
        }
    }
}
