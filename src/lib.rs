//! Synchronizes base-chain accounts into Engine.
//!
//! [`ChainSync`] subscribes before fetching missing accounts. Ordinary accounts
//! enter Engine in `Uninit` mode; executable programs enter as read-only ELF
//! accounts. Callers handle WebSocket updates and connection recovery.

pub mod grpc;
pub mod http;
mod program;
pub mod rpc;
pub mod websocket;

use std::borrow::Borrow;

use engine::Engine;
use futures::future;
use solana_account::AccountBuilder;
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;

use crate::http::Fetcher;
use crate::websocket::Pool;

/// Role of an account passed to [`ChainSync::sync`]. Only [`Self::Program`] adds a companion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountProperty {
    /// Transaction fee payer.
    Payer,
    /// Writable transaction account.
    Writable,
    /// Read-only transaction account.
    Readonly,
    /// Executable program; its derived ProgramData address is also subscribed and fetched.
    Program,
}

/// Pubkey and role of an account requested for synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncAccount {
    /// Account address.
    pub pubkey: Pubkey,
    /// Role used to select program handling.
    pub property: AccountProperty,
}

/// Acquires missing base-chain accounts for Engine with live WebSocket subscriptions.
pub struct ChainSync {
    /// Holds account leases through materialization.
    engine: Engine,
    /// Supplies confirmed snapshots for missing accounts.
    fetcher: Fetcher,
    /// Tracks accounts before their snapshots are fetched.
    websocket: Pool,
}

/// Failure to acquire or materialize a requested account.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Engine account lookup failed: {0}")]
    AccountsDb(#[from] accountsdb::AccountsDBError),
    #[error("HTTP account fetch failed: {0}")]
    Fetch(#[from] http::Error),
    #[error("WebSocket subscription failed: {0}")]
    Subscribe(#[from] websocket::Error),
    #[error("Engine account operation failed: {0}")]
    Engine(#[from] engine::EngineError),
    #[error("invalid program: {0}")]
    Program(&'static str),
    #[error("invalid ProgramData: {0}")]
    ProgramData(#[from] Box<bincode::ErrorKind>),
}

impl ChainSync {
    /// Creates a synchronizer. The caller must drain WebSocket events while syncing.
    pub fn new(engine: Engine, fetcher: Fetcher, websocket: Pool) -> Self {
        Self { engine, fetcher, websocket }
    }

    /// Subscribes to and materializes accounts missing from Engine.
    ///
    /// Program requests also subscribe to and fetch their derived ProgramData
    /// address, but only materialize the normalized program. HTTP `null`
    /// becomes a default account, including for a missing program.
    ///
    /// Pubkeys requested as writable accounts or programs must occur only once.
    /// Repeated payer and read-only requests are collapsed by pubkey.
    ///
    /// Each batch waits for subscription acknowledgements before fetching. A
    /// subscription, fetch, or program-normalization failure releases that
    /// batch's subscriptions. Materialization errors return without cleanup.
    pub async fn sync<I>(&self, requests: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: Borrow<SyncAccount>,
    {
        let mut missing = {
            let accounts = self.engine.accounts();
            let loader = accounts.loader();
            let mut missing = Vec::new();
            for account in requests {
                let account = *account.borrow();
                if !loader.contains(&account.pubkey)? {
                    missing.push(account);
                }
            }
            missing
        };
        // Lock primary accounts in the same order across concurrent syncs.
        missing.sort_unstable_by_key(|account| account.pubkey);
        missing.dedup_by_key(|account| account.pubkey);

        // Companions count against getMultipleAccounts' 100-key limit.
        let mut start = 0;
        while start < missing.len() {
            let mut end = start;
            let mut size = 0;
            while let Some(account) = missing.get(end) {
                let added = 1 + usize::from(account.property == AccountProperty::Program);
                if size + added > 100 {
                    break;
                }
                size += added;
                end += 1;
            }
            let batch = &missing[start..end];
            start = end;
            self.sync_batch(batch).await?;
        }
        Ok(())
    }

    /// Acquires one bounded batch while retaining each missing account's lease.
    async fn sync_batch(&self, batch: &[SyncAccount]) -> Result<(), Error> {
        // Keep primary leases through fetch and materialization to exclude duplicate syncs.
        let mut pending = Vec::new();
        let mut programs = Vec::new();
        let mut keys = Vec::with_capacity(batch.len());
        for &account in batch {
            let accessor = self.engine.account(account.pubkey).await;
            if accessor.read(|_| ())?.is_some() {
                continue;
            }
            let index = keys.len();
            keys.push(account.pubkey);
            if account.property == AccountProperty::Program {
                let data_index = keys.len();
                keys.push(get_program_data_address(&account.pubkey));
                programs.push((index, data_index));
            }
            pending.push((accessor, index));
        }
        if pending.is_empty() {
            return Ok(());
        }

        self.subscribe(&keys).await?;

        let mut snapshot = match self.fetcher.fetch(&keys, None).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.unsubscribe(&keys).await;
                return Err(error.into());
            }
        };
        if let Err(error) = program::normalize_batch(&programs, &mut snapshot.accounts) {
            self.unsubscribe(&keys).await;
            return Err(error);
        }
        for (accessor, index) in pending {
            let account = snapshot.accounts[index]
                .take()
                .unwrap_or_else(|| AccountBuilder::default().build());
            accessor.materialize(account, None).await?;
        }
        Ok(())
    }

    /// Waits for every acknowledgement before returning a subscription failure.
    async fn subscribe(&self, keys: &[Pubkey]) -> Result<(), Error> {
        // Dropping an admitted subscribe future can leave a live subscription behind.
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
            self.unsubscribe(&subscribed).await;
            return Err(error.into());
        }
        Ok(())
    }

    /// Releases acknowledged subscriptions or waits for their socket loss.
    async fn unsubscribe(&self, keys: &[Pubkey]) {
        let pending = keys.iter().map(|&key| self.websocket.unsubscribe(key));
        // Failed unsubscriptions have already lost their socket or pool owner.
        let _ = future::join_all(pending).await;
    }
}
