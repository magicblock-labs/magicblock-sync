//! Base-layer account snapshots and subscriptions with decoded Engine accounts.
//!
//! [`http::Fetcher`] fetches ordered snapshots at a minimum confirmed context slot.
//! [`websocket::Pool`] follows individual accounts and maintains the shared freshness
//! watermark. [`grpc::Client`] adds retained-account redundancy and delegation lifecycle
//! observations through Yellowstone.
//!
//! [`ChainSync`] subscribes before fetching and materializing accounts missing
//! from Engine. Callers handle updates and transport recovery. HTTP and WebSocket
//! accounts retain `Uninit` mode; resolved gRPC delegations carry their original
//! owner and `Delegated` mode.

pub mod grpc;
pub mod http;
pub mod rpc;
pub mod websocket;

use std::borrow::Borrow;

use engine::Engine;
use futures::future;
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;

use crate::http::Fetcher;
use crate::websocket::Pool;

/// Synchronization entry point backed by Engine, WebSocket subscriptions, and HTTP snapshots.
pub struct ChainSync {
    /// Owns account lookup, leases, and materialization.
    engine: Engine,
    /// Supplies snapshots for accounts absent from Engine.
    fetcher: Fetcher,
    /// Subscribes to missing accounts before their snapshots are fetched.
    websocket: Pool,
}

/// Account synchronization failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Engine's account loader could not check an account.
    #[error("Engine account lookup failed: {0}")]
    AccountsDb(#[from] accountsdb::AccountsDBError),
    /// HTTP account fetching failed.
    #[error("HTTP account fetch failed: {0}")]
    Fetch(#[from] http::Error),
    /// WebSocket account subscription failed.
    #[error("WebSocket subscription failed: {0}")]
    Subscribe(#[from] websocket::Error),
    /// Engine could not read or materialize an account.
    #[error("Engine account operation failed: {0}")]
    Engine(#[from] engine::EngineError),
}

impl ChainSync {
    /// Creates an entry point using the supplied Engine, HTTP fetcher, and WebSocket pool.
    pub fn new(engine: Engine, fetcher: Fetcher, websocket: Pool) -> Self {
        Self { engine, fetcher, websocket }
    }

    /// Waits for each missing account's subscription acknowledgement before
    /// fetching batches of up to 100. Subscriptions within a batch run
    /// concurrently and all settle before an error is returned. HTTP `null`
    /// becomes a default account.
    /// Account leases prevent concurrent syncs from fetching and materializing
    /// the same key twice.
    ///
    /// The caller consumes WebSocket events and handles recovery. Subscriptions
    /// acknowledged before a subscription or HTTP failure are released before
    /// returning. Materialization failures are returned without cleanup.
    pub async fn sync<I>(&self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: Borrow<Pubkey>,
    {
        let mut missing = {
            let accounts = self.engine.accounts();
            let loader = accounts.loader();
            let mut missing = Vec::new();
            for key in keys {
                let key = *key.borrow();
                if !loader.contains(&key)? {
                    missing.push(key);
                }
            }
            missing
        };
        // Acquire each batch's leases in one order across concurrent calls.
        missing.sort_unstable();
        missing.dedup();

        for batch in missing.chunks(100) {
            let mut pending = Vec::new();
            for &key in batch {
                let accessor = self.engine.account(key).await;
                if accessor.read(|_| ())?.is_none() {
                    pending.push((key, accessor));
                }
            }
            let keys = pending.iter().map(|(key, _)| *key).collect::<Vec<_>>();
            if keys.is_empty() {
                continue;
            }
            let subscriptions = keys.iter().map(|&key| self.websocket.subscribe(key));
            let mut subscribed = Vec::with_capacity(keys.len());
            let mut failure = None;
            for (&key, result) in keys.iter().zip(future::join_all(subscriptions).await) {
                match result {
                    Ok(()) => subscribed.push(key),
                    Err(error) => {
                        failure.replace(error);
                    }
                }
            }
            if let Some(error) = failure {
                self.unsubscribe_all(&subscribed).await;
                return Err(error.into());
            }
            let snapshot = match self.fetcher.fetch(&keys, None).await {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.unsubscribe_all(&subscribed).await;
                    return Err(error.into());
                }
            };
            for ((_, accessor), account) in pending.into_iter().zip(snapshot.accounts) {
                let account = account.unwrap_or_else(|| AccountBuilder::default().build());
                accessor.materialize(account, None).await?;
            }
        }
        Ok(())
    }

    /// Waits for every acknowledged subscription to be released or lost with its socket.
    async fn unsubscribe_all(&self, keys: &[Pubkey]) {
        let pending = keys.iter().map(|&key| self.websocket.unsubscribe(key));
        // Unsubscribe fails only when its socket or the pool stops owning the subscription.
        let _ = future::join_all(pending).await;
    }
}
