//! Base-layer account snapshots and subscriptions with decoded Engine accounts.
//!
//! [`http::Fetcher`] fetches ordered snapshots at a minimum confirmed context slot.
//! [`websocket::Pool`] follows individual accounts and maintains the shared freshness
//! watermark. [`grpc::Client`] adds retained-account redundancy and delegation lifecycle
//! observations through Yellowstone.
//!
//! [`ChainSync`] fetches accounts missing from Engine and materializes them.
//! Subscription-before-fetch coordination, reconciliation, and transport recovery
//! remain outside this entry point. HTTP/WebSocket accounts retain Uninit mode;
//! resolved gRPC delegations include their original owner and Delegated mode.

pub mod grpc;
pub mod http;
pub mod rpc;
pub mod websocket;

use std::borrow::Borrow;

use engine::Engine;
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;

use crate::http::Fetcher;

/// Synchronization entry point backed by Engine and an HTTP fetcher.
pub struct ChainSync {
    /// Owns account lookup, leases, and materialization.
    engine: Engine,
    /// Supplies snapshots for accounts absent from Engine.
    fetcher: Fetcher,
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
    /// Engine could not read or materialize an account.
    #[error("Engine account operation failed: {0}")]
    Engine(#[from] engine::EngineError),
}

impl ChainSync {
    /// Creates an entry point using the supplied Engine and HTTP fetcher.
    pub fn new(engine: Engine, fetcher: Fetcher) -> Self {
        Self { engine, fetcher }
    }

    /// Fetches missing accounts in batches of at most 100 and materializes them.
    /// An HTTP null is materialized from a default account builder. Account
    /// leases are held through fetching and materialization so concurrent syncs
    /// of the same key do not fetch or materialize it twice.
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
            let snapshot = self.fetcher.fetch(&keys, None).await?;
            for ((_, accessor), account) in pending.into_iter().zip(snapshot.accounts) {
                let account = account.unwrap_or_else(|| AccountBuilder::default().build());
                accessor.materialize(account, None).await?;
            }
        }
        Ok(())
    }
}
