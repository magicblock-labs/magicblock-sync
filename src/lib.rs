//! Synchronizes base-chain accounts into Engine.
//!
//! [`ChainSync`] subscribes before fetching missing read-only accounts and payers.
//! Writable accounts are fetched with their delegation records but not subscribed over
//! WebSocket; all requested accounts are retained on the gRPC stream.
//! Ordinary accounts enter Engine in `Uninit` mode; executable programs enter as
//! read-only ELF accounts. A background worker applies WebSocket and gRPC events.
//! WebSocket recovery and subscription removal after undelegation are not reconciled
//! here. A later base-chain update can recreate a removed account.

mod delegation;
mod grpc;
mod http;
mod program;
mod rpc;
mod websocket;

use std::{
    borrow::Borrow,
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use dlp_api::{
    args::PostDelegationActions, pda::delegation_record_pda_from_delegated_account, Decrypt,
};
use engine::{Engine, PostFinalize};
use futures::future;
use solana_account::{AccountBuilder, AccountMode, StateFlags};
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use solana_sdk_ids::bpf_loader_upgradeable;
use tokio::sync::mpsc::Receiver;
use url::Url;

use crate::http::{Fetcher, Snapshot};
use crate::websocket::Pool;

pub use grpc::{Config as GrpcConfig, Error as GrpcError};
pub use http::Error as HttpError;
pub use rpc::{DecodeError, Error as RpcError};
pub use websocket::{
    Config as WebSocketConfig, Error as WebSocketError, Provider as WebSocketProvider,
};

/// Provider configuration for HTTP snapshots and live WebSocket/gRPC updates.
pub struct ChainSyncConfig {
    /// HTTP snapshot providers.
    pub http: Vec<Url>,
    /// WebSocket subscription providers.
    pub websocket: WebSocketConfig,
    /// Yellowstone update provider, including the delegation authority.
    pub grpc: GrpcConfig,
}

/// Selects subscription and companion-fetch behavior in [`ChainSync::sync`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountProperty {
    /// Fee payer; subscribed over WebSocket during the initial fetch.
    Payer,
    /// Writable transaction account; fetched without a WebSocket subscription.
    Writable,
    /// Read-only transaction account.
    Readonly,
    /// Executable program; its ProgramData companion is fetched and subscribed over WebSocket.
    Program,
}

/// One primary account requested for synchronization. ProgramData and delegation
/// record companions are derived from its property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncAccount {
    /// Primary account address to fetch and materialize.
    pub pubkey: Pubkey,
    /// Role used to select subscription and companion-fetch behavior.
    pub property: AccountProperty,
}

/// Acquires missing base-chain accounts for Engine and applies live provider updates.
pub struct ChainSync {
    /// Acquires missing-account leases and materializes their snapshots.
    engine: Engine,
    /// Supplies confirmed snapshots for missing accounts.
    fetcher: Fetcher,
    /// Tracks accounts before their snapshots are fetched.
    websocket: Pool,
    /// Keeps the gRPC stream alive for the synchronizer's lifetime.
    grpc: grpc::Client,
}

/// Failure to acquire or apply base-chain account state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP account fetch failed: {0}")]
    Fetch(#[from] HttpError),
    #[error("WebSocket subscription failed: {0}")]
    Subscribe(#[from] WebSocketError),
    #[error("gRPC operation failed: {0}")]
    Grpc(#[from] GrpcError),
    #[error("Engine account operation failed: {0}")]
    Engine(#[from] engine::EngineError),
    #[error("invalid program: {0}")]
    Program(&'static str),
    #[error("invalid ProgramData: {0}")]
    ProgramData(#[from] Box<bincode::ErrorKind>),
    #[error("invalid delegation record: {0}")]
    Record(&'static str),
    #[error("invalid delegation actions: {0}")]
    Actions(#[from] borsh::io::Error),
    #[error("delegation action decryption failed: {0}")]
    Decrypt(#[from] dlp_api::decrypt::DecryptError),
}

impl ChainSync {
    /// Sets up HTTP, WebSocket, and gRPC providers and starts applying updates.
    /// Worker failures are logged. Dropping the last handle stops the provider
    /// tasks; the host remains responsible for Engine shutdown.
    pub fn new(engine: Engine, config: ChainSyncConfig) -> Result<Arc<Self>, Error> {
        let (websocket, websocket_rx) = Pool::new(config.websocket);
        let slot = websocket.slot();
        let fetcher = Fetcher::new(config.http, Arc::clone(&slot))?;
        let (grpc, grpc_rx) = grpc::Client::new(config.grpc, slot)?;
        let sync = Arc::new(Self { engine, fetcher, websocket, grpc });
        tokio::spawn(Self::run(Arc::downgrade(&sync), websocket_rx, grpc_rx));
        Ok(sync)
    }

    /// Fetches and materializes requested accounts that are missing from Engine.
    ///
    /// Read-only accounts, programs, and payers are subscribed over WebSocket
    /// before fetching. Writable accounts are fetched without WebSocket subscriptions.
    /// Programs include ProgramData; writable accounts and payers include their
    /// derived delegation record. Records are fetched only. A payer's WebSocket
    /// subscription is removed when its initial snapshot resolves as delegated here.
    /// Requested accounts and ProgramData targets are retained on gRPC for automatic
    /// application of later updates.
    ///
    /// An account is resolved as delegated only when its primary and delegation-record
    /// snapshots are DLP-owned and the record names this Engine. Other snapshots retain
    /// their fetched owner and mode. HTTP `null` becomes a default account.
    ///
    /// Writable and program pubkeys must be unique; repeated payer and read-only
    /// requests are collapsed. Requested accounts must not overlap a program's
    /// derived ProgramData address.
    pub async fn sync<I>(&self, requests: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: Borrow<SyncAccount>,
    {
        let mut accounts: Vec<_> = requests.into_iter().map(|account| *account.borrow()).collect();
        accounts.sort_unstable_by_key(|account| account.pubkey);
        accounts.dedup_by_key(|account| account.pubkey);

        // Bound each acquisition wave so its primary keys and companions fit one RPC batch.
        let mut start = 0;
        while start < accounts.len() {
            let mut end = start;
            let mut size = 0;
            while let Some(account) = accounts.get(end) {
                let added = 1 + usize::from(account.property != AccountProperty::Readonly);
                if size + added > 100 {
                    break;
                }
                size += added;
                end += 1;
            }
            let batch = &accounts[start..end];
            start = end;
            self.grpc.retain(grpc_subscriptions(batch)).await?;
            self.sync_batch(batch).await?;
        }
        Ok(())
    }

    /// Selects between both streams and handles events only while the synchronizer can be
    /// upgraded.
    async fn run(
        sync: Weak<Self>,
        mut websocket: Receiver<websocket::Event>,
        mut grpc: Receiver<grpc::Event>,
    ) {
        loop {
            tokio::select! {
                Some(event) = websocket.recv() => {
                    let Some(sync) = sync.upgrade() else { break; };
                    sync.on_websocket(event).await;
                },
                Some(event) = grpc.recv() => {
                    let Some(sync) = sync.upgrade() else { break; };
                    sync.on_grpc(event).await;
                },
                else => break,
            }
        }
    }

    /// Applies a WebSocket account update and logs failures without stopping the worker.
    async fn on_websocket(&self, event: websocket::Event) {
        match event {
            websocket::Event::Update { sub, account } => {
                if let Err(error) = self.apply(sub, account).await {
                    tracing::error!(source = "WS", %sub.pubkey, %error, "account update failed");
                }
            }
            websocket::Event::Dropped { connection, pubkeys, error } => {
                tracing::warn!(?connection, lost = pubkeys.len(), %error, "WebSocket subscriptions lost");
            }
        }
    }

    /// Applies gRPC account and lifecycle events, logging per-account failures.
    async fn on_grpc(&self, event: grpc::Event) {
        match event {
            grpc::Event::Update { pubkey, target, account } => {
                let sub = AccountSubscription { pubkey, target };
                if let Err(error) = self.apply(sub, account).await {
                    tracing::error!(source = "gRPC", %pubkey, %error, "account update failed");
                }
            }
            grpc::Event::Delegated(delegation) => {
                let pubkey = delegation.pubkey;
                if let Err(error) = self.delegated(delegation).await {
                    tracing::error!(%pubkey, %error, "delegation failed");
                }
            }
            grpc::Event::Undelegated { pubkeys, slot } => {
                for pubkey in pubkeys {
                    if let Err(error) = self.undelegated(pubkey, slot).await {
                        tracing::error!(%pubkey, slot, %error, "undelegation failed");
                    }
                }
            }
            grpc::Event::Disconnected(error) => tracing::warn!(%error, "gRPC disconnected"),
        }
    }

    /// Loads dependencies for delegated actions before materializing the delegated account.
    async fn delegated(&self, delegation: grpc::Delegation) -> Result<(), Error> {
        let grpc::Delegation { pubkey, account, record } = delegation;
        let appended = delegation::appended(&record).ok_or(Error::Record("record too short"))?;
        let actions = if !appended.is_empty() {
            let compact: PostDelegationActions = borsh::from_slice(appended)?;
            let actions = compact.decrypt_with_keypair(self.engine.signer())?;
            let mut dependencies = BTreeMap::new();
            for action in &actions {
                dependencies.insert(action.program_id, AccountProperty::Program);
                for meta in &action.accounts {
                    let property =
                        dependencies.entry(meta.pubkey).or_insert(AccountProperty::Readonly);
                    if meta.is_writable {
                        *property = AccountProperty::Writable;
                    }
                }
            }
            // The target is acquired after its other action dependencies.
            dependencies.remove(&pubkey);
            let dependencies = dependencies
                .into_iter()
                .map(|(pubkey, property)| SyncAccount { pubkey, property });
            self.sync(dependencies).await?;
            let source_program = account.read().owner();
            Some(PostFinalize { source_program, actions })
        } else {
            None
        };
        let accessor = self.engine.account(pubkey).await?;
        accessor.materialize(account, actions).await?;
        Ok(())
    }

    /// Deletes the account only when the event is current and the observed mode permits
    /// removal.
    async fn undelegated(&self, pubkey: Pubkey, slot: u64) -> Result<(), Error> {
        let accessor = self.engine.account(pubkey).await?;
        let Some((mode, observed)) = accessor.observed() else {
            return Ok(());
        };
        if slot < observed || (mode.authoritative() && mode != AccountMode::Transient) {
            tracing::warn!(%pubkey, slot, observed, ?mode, "undelegation cannot remove account");
            return Ok(());
        }
        accessor.delete().await?;
        Ok(())
    }

    /// Applies a streamed account update to its subscribed account or program target.
    async fn apply(
        &self,
        subscription: AccountSubscription,
        account: AccountBuilder,
    ) -> Result<(), Error> {
        let AccountSubscription { pubkey, target } = subscription;
        let accessor = self.engine.account(target.unwrap_or(pubkey)).await?;
        let account = match target {
            Some(_) => program::normalize_data(account, self.engine.rent())?,
            None if account.read().flags().contains(StateFlags::EXECUTABLE) => {
                program::normalize(account, None, self.engine.rent())?
            }
            None => account,
        };
        accessor.materialize(account, None).await?;
        Ok(())
    }

    /// Acquires missing accounts and applies the fetched snapshot and any delegation actions.
    async fn sync_batch(&self, batch: &[SyncAccount]) -> Result<(), Error> {
        // Engine rechecks presence under ordered leases, preventing overlapping syncs from
        // fetching the same missing accounts.
        let keys: Vec<_> = batch.iter().map(|account| account.pubkey).collect();
        let accessors = self.engine.missing_accounts(&keys).await?;
        if accessors.is_empty() {
            return Ok(());
        }
        let plan = FetchPlan::new(batch, accessors);
        let (mut snapshot, prune) = self.fetch_batch(&plan).await?;
        let actions = self.materialize_batch(plan, &mut snapshot).await?;
        self.apply_delegations(actions).await?;
        self.unsubscribe(prune).await;
        Ok(())
    }

    /// Subscribes, fetches, and normalizes planned accounts, cleaning up subscriptions on failure.
    async fn fetch_batch(&self, plan: &FetchPlan<'_>) -> Result<(Snapshot, Vec<Pubkey>), Error> {
        self.subscribe(&plan.subscriptions).await?;
        let result = async {
            let mut snapshot = self.fetcher.fetch(&plan.keys, None).await?;
            let prune = plan.pruned_subscriptions(&snapshot);
            program::normalize_batch(&plan.programs, &mut snapshot.accounts, self.engine.rent())?;
            Ok((snapshot, prune))
        }
        .await;
        if result.is_err() {
            let pubkeys = plan.subscriptions.iter().map(|subscription| subscription.pubkey);
            self.unsubscribe(pubkeys).await;
        }
        result
    }

    /// Materializes snapshot accounts and returns delegated accounts with deferred actions.
    async fn materialize_batch(
        &self,
        plan: FetchPlan<'_>,
        snapshot: &mut Snapshot,
    ) -> Result<Vec<PendingDelegation>, Error> {
        let mut actions = Vec::new();
        for pending in plan.accounts {
            let pubkey = pending.accessor.pubkey();
            let account = snapshot.accounts[pending.index].take().unwrap_or_default();
            let delegation = pending.record_index.and_then(|index| {
                delegation::snapshot_record(
                    &account,
                    snapshot.accounts[index].as_ref(),
                    self.engine.authority(),
                )
            });
            let Some((metadata, record)) = delegation else {
                pending.accessor.materialize(account, None).await?;
                continue;
            };
            let account = delegation::account(account, metadata.owner, metadata.delegation_slot);
            if delegation::appended(record).is_some_and(|actions| !actions.is_empty()) {
                actions.push(PendingDelegation {
                    delegation: grpc::Delegation {
                        pubkey,
                        account,
                        record: record.to_vec(),
                    },
                    payer: pending.property == AccountProperty::Payer,
                });
            } else {
                pending.accessor.materialize(account, None).await?;
                if pending.property == AccountProperty::Payer {
                    self.unsubscribe([pubkey]).await;
                }
            }
        }
        Ok(actions)
    }

    /// Applies deferred delegations after the batch's account leases have been released.
    async fn apply_delegations(&self, actions: Vec<PendingDelegation>) -> Result<(), Error> {
        for PendingDelegation { delegation, payer } in actions {
            let pubkey = delegation.pubkey;
            // Action dependencies may include another key from this batch.
            Box::pin(self.delegated(delegation)).await?;
            if payer {
                self.unsubscribe([pubkey]).await;
            }
        }
        Ok(())
    }

    /// Waits for every subscription request and removes successful subscriptions if any fail.
    async fn subscribe(&self, subscriptions: &[AccountSubscription]) -> Result<(), Error> {
        // Settle every admitted request so acknowledged subscriptions can be cleaned up.
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
            self.unsubscribe(subscribed).await;
            return Err(error.into());
        }
        Ok(())
    }

    /// Releases subscriptions; failed releases mean their socket or pool entry is already gone.
    async fn unsubscribe(&self, keys: impl IntoIterator<Item = Pubkey>) {
        let pending = keys.into_iter().map(|key| self.websocket.unsubscribe(key));
        // A failed request means the socket or pool owner already removed it.
        let _ = future::join_all(pending).await;
    }
}

/// One leased account and its positions in the ordered HTTP fetch batch.
struct PendingAccount<'engine> {
    /// Lease held through snapshot processing unless the account is deferred for actions.
    accessor: engine::AccountAccessor<'engine>,
    /// Selects companion-account and payer cleanup behavior.
    property: AccountProperty,
    /// Position of the primary account in `FetchPlan::keys`.
    index: usize,
    /// Position of the delegation record, when this account needs one.
    record_index: Option<usize>,
}

/// Delegation deferred until the batch's other account leases are released.
struct PendingDelegation {
    /// Resolved account and its full delegation record.
    delegation: grpc::Delegation,
    /// Whether successful materialization should remove the payer subscription.
    payer: bool,
}

/// Ordered fetch inputs and subscriptions derived from a batch of missing accounts.
struct FetchPlan<'engine> {
    /// Missing account leases paired with their positions and requested properties.
    accounts: Vec<PendingAccount<'engine>>,
    /// Primary and companion keys in HTTP response order.
    keys: Vec<Pubkey>,
    /// Accounts subscribed before the HTTP request.
    subscriptions: Vec<AccountSubscription>,
    /// Program and ProgramData positions used during normalization.
    programs: Vec<(usize, usize)>,
}

/// Account subscription and optional target for ProgramData account updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccountSubscription {
    /// Address observed by the transport.
    pub pubkey: Pubkey,
    /// Program to update when `pubkey` is its Loader V3 ProgramData account.
    pub target: Option<Pubkey>,
}

impl<'e> FetchPlan<'e> {
    /// Pairs ordered missing-account leases with requests and derives fetch inputs.
    fn new(batch: &[SyncAccount], accessors: Vec<engine::AccountAccessor<'e>>) -> Self {
        // Engine returns missing accessors in pubkey order, matching the sorted request batch.
        let mut accessors = accessors.into_iter().peekable();
        let mut plan = Self {
            accounts: Vec::with_capacity(accessors.len()),
            keys: Vec::with_capacity(batch.len() * 2),
            subscriptions: Vec::with_capacity(batch.len() * 2),
            programs: Vec::new(),
        };
        for request in batch {
            let Some(accessor) = accessors.next_if(|accessor| accessor.pubkey() == request.pubkey)
            else {
                continue;
            };
            let pubkey = request.pubkey;
            let index = plan.keys.len();
            plan.keys.push(pubkey);
            if request.property != AccountProperty::Writable {
                plan.subscriptions.push(AccountSubscription { pubkey, target: None });
            }
            let record_index = match request.property {
                AccountProperty::Payer | AccountProperty::Writable => {
                    let index = plan.keys.len();
                    plan.keys.push(delegation_record_pda_from_delegated_account(&pubkey));
                    Some(index)
                }
                AccountProperty::Program => {
                    let data = get_program_data_address(&pubkey);
                    let data_index = plan.keys.len();
                    plan.keys.push(data);
                    plan.subscriptions.push(AccountSubscription {
                        pubkey: data,
                        target: Some(pubkey),
                    });
                    plan.programs.push((index, data_index));
                    None
                }
                AccountProperty::Readonly => None,
            };
            plan.accounts.push(PendingAccount {
                accessor,
                property: request.property,
                index,
                record_index,
            });
        }
        plan
    }

    /// Selects the program or ProgramData subscription to release after normalization.
    fn pruned_subscriptions(&self, snapshot: &Snapshot) -> Vec<Pubkey> {
        self.programs
            .iter()
            .filter_map(|&(program_index, data_index)| {
                let program = snapshot.accounts[program_index].as_ref()?.read();
                let index = if program.owner() == bpf_loader_upgradeable::ID {
                    program_index
                } else {
                    data_index
                };
                Some(self.keys[index])
            })
            .collect()
    }
}

/// Tracks primary accounts and ProgramData targets for streamed updates.
fn grpc_subscriptions(accounts: &[SyncAccount]) -> Vec<AccountSubscription> {
    let mut subscriptions = BTreeMap::new();
    for account in accounts {
        subscriptions.insert(account.pubkey, None);
        if account.property == AccountProperty::Program {
            subscriptions.insert(
                get_program_data_address(&account.pubkey),
                Some(account.pubkey),
            );
        }
    }
    subscriptions
        .into_iter()
        .map(|(pubkey, target)| AccountSubscription { pubkey, target })
        .collect()
}
