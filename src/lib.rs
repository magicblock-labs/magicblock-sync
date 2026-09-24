//! Synchronizes base-chain accounts into Engine.
//!
//! [`ChainSync`] subscribes before fetching missing accounts. Ordinary accounts
//! enter Engine in `Uninit` mode; executable programs enter as read-only ELF
//! accounts. A background worker applies WebSocket and gRPC account notifications.
//! Connection recovery and delegation lifecycle are not handled.

pub mod grpc;
pub mod http;
mod program;
pub mod rpc;
pub mod websocket;

use std::borrow::Borrow;

use engine::Engine;
use futures::future;
use solana_account::{AccountBuilder, StateFlags};
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use solana_sdk_ids::bpf_loader_upgradeable;
use tokio::sync::mpsc::Receiver;

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

/// Account subscription and optional target for a ProgramData image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountSubscription {
    /// Address observed by the transport.
    pub pubkey: Pubkey,
    /// Program to update when `pubkey` is its Loader V3 ProgramData account.
    pub target: Option<Pubkey>,
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

/// Applies transport images without participating in acquisition or HTTP fetching.
struct Worker {
    engine: Engine,
    websocket: Receiver<websocket::Event>,
    grpc: Receiver<grpc::Event>,
}

/// Failure to acquire or apply a base-chain account image.
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

impl Worker {
    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(event) = self.websocket.recv() => self.websocket(event).await,
                Some(event) = self.grpc.recv() => self.grpc(event).await,
            }
        }
    }

    async fn websocket(&self, event: websocket::Event) {
        let websocket::Event::Update { sub, account } = event else {
            return;
        };
        if let Err(error) = self.apply(sub, account).await {
            tracing::error!(source = "WebSocket", pubkey = %sub.pubkey, target = ?sub.target, %error, "account update failed");
        }
    }

    async fn grpc(&self, event: grpc::Event) {
        let grpc::Event::Update { pubkey, target, account } = event else {
            return;
        };
        if let Err(error) = self.apply(AccountSubscription { pubkey, target }, account).await {
            tracing::error!(source = "gRPC", %pubkey, ?target, %error, "account update failed");
        }
    }

    /// Reconciles one remote image after taking Engine's account mutation lease.
    async fn apply(
        &self,
        subscription: AccountSubscription,
        account: AccountBuilder,
    ) -> Result<(), Error> {
        let AccountSubscription { pubkey, target } = subscription;
        let accessor = self.engine.account(target.unwrap_or(pubkey)).await;
        let account = if target.is_some() {
            program::normalize_data(account, self.engine.rent())?
        } else if account.read().flags().contains(StateFlags::EXECUTABLE) {
            program::normalize(account, None, self.engine.rent())?
        } else {
            account
        };
        accessor.materialize(account, None).await?;
        Ok(())
    }
}

impl ChainSync {
    /// Creates a synchronizer and starts its detached notification worker.
    /// It logs account failures and ignores non-account events. The receivers
    /// remain open for the worker's lifetime; shutdown coordination is left to the host.
    pub fn new(
        engine: Engine,
        fetcher: Fetcher,
        websocket: Pool,
        websocket_rx: Receiver<websocket::Event>,
        grpc: Receiver<grpc::Event>,
    ) -> Self {
        let worker = Worker {
            engine: engine.clone(),
            websocket: websocket_rx,
            grpc,
        };
        tokio::spawn(worker.run());
        Self { engine, fetcher, websocket }
    }

    /// Subscribes to and materializes accounts missing from Engine.
    ///
    /// Program requests also subscribe to and fetch their derived ProgramData
    /// address, but only materialize the normalized program. HTTP `null`
    /// becomes a default account, including for a missing program. After a
    /// successful snapshot, the subscription without ELF is released; both
    /// remain for a missing program.
    ///
    /// Pubkeys requested as writable accounts or programs must occur only once.
    /// Repeated payer and read-only requests are collapsed by pubkey.
    /// Requested primary accounts must not overlap a requested program's derived
    /// ProgramData address. Mutable access serializes acquisition waves.
    ///
    /// Each batch waits for subscription acknowledgements before fetching. A
    /// subscription, fetch, or program-normalization failure releases that
    /// batch's subscriptions. Materialization errors return without cleanup.
    pub async fn sync<I>(&mut self, requests: I) -> Result<(), Error>
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
        let mut subscriptions = Vec::with_capacity(batch.len() * 2);
        for &account in batch {
            let accessor = self.engine.account(account.pubkey).await;
            if accessor.read(|_| ())?.is_some() {
                continue;
            }
            let index = subscriptions.len();
            subscriptions.push(AccountSubscription {
                pubkey: account.pubkey,
                target: None,
            });
            if account.property == AccountProperty::Program {
                let data_index = subscriptions.len();
                subscriptions.push(AccountSubscription {
                    pubkey: get_program_data_address(&account.pubkey),
                    target: Some(account.pubkey),
                });
                programs.push((index, data_index));
            }
            pending.push((accessor, index));
        }
        if pending.is_empty() {
            return Ok(());
        }

        self.subscribe(&subscriptions).await?;
        let keys: Vec<_> = subscriptions.iter().map(|subscription| subscription.pubkey).collect();
        let mut snapshot = match self.fetcher.fetch(&keys, None).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.unsubscribe(&keys).await;
                return Err(error.into());
            }
        };
        let prune: Vec<_> = programs
            .iter()
            .filter_map(|&(index, data_index)| {
                let account = snapshot.accounts[index].as_ref()?.read();
                let idx =
                    if account.owner() == bpf_loader_upgradeable::ID { index } else { data_index };
                Some(keys[idx])
            })
            .collect();
        if let Err(error) =
            program::normalize_batch(&programs, &mut snapshot.accounts, self.engine.rent())
        {
            self.unsubscribe(&keys).await;
            return Err(error);
        }
        for (accessor, index) in pending {
            let account = snapshot.accounts[index].take().unwrap_or_default();
            accessor.materialize(account, None).await?;
        }
        self.unsubscribe(&prune).await;
        Ok(())
    }

    /// Waits for every acknowledgement before returning a subscription failure.
    async fn subscribe(&self, subscriptions: &[AccountSubscription]) -> Result<(), Error> {
        // Dropping an admitted subscribe future can leave a live subscription behind.
        let requests =
            subscriptions.iter().map(|&subscription| self.websocket.subscribe(subscription));
        let mut subscribed = Vec::with_capacity(subscriptions.len());
        let mut failure = None;
        for (subscription, result) in subscriptions.iter().zip(future::join_all(requests).await) {
            match result {
                Ok(()) => subscribed.push(subscription.pubkey),
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
