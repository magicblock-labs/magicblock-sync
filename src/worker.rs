/// Confirmed coverage and redundancy scheduling for the event worker.
mod coverage;

use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    hash::{Hash, Hasher},
    sync::Weak,
};

use dlp_api::{args::PostDelegationActions, Decrypt};
use engine::PostFinalize;
use nucleus::shutdown::{ShutdownHandle, ShutdownReason};
use solana_account::{AccountBuilder, AccountMode, StateFlags};
use solana_pubkey::Pubkey;
use tokio::{sync::mpsc::Receiver, time};

use self::coverage::{Coverage, Source};
use crate::{
    delegation, grpc, program, websocket, AccountProperty, AccountSubscription, ChainSync, Error,
    SyncAccount, DUPLICATION_DELAY,
};

impl ChainSync {
    /// Stable per-key assignment keeps a recently removed remote filter reusable.
    fn grpc_stream(&self, pubkey: Pubkey) -> usize {
        let mut hash = DefaultHasher::new();
        pubkey.hash(&mut hash);
        hash.finish() as usize % self.grpc.len()
    }

    /// Serializes source events, account application, and Engine eviction.
    pub(super) async fn run(
        sync: Weak<Self>,
        mut websocket: Receiver<websocket::Event>,
        mut grpc: Receiver<grpc::Event>,
        mut shutdown: ShutdownHandle,
    ) {
        let mut coverage = Coverage::default();
        let mut tick = time::interval(DUPLICATION_DELAY);
        tick.tick().await;
        loop {
            let result = tokio::select! {
                _ = shutdown.signalled() => {
                    shutdown.terminate(ShutdownReason::Signalled);
                    break;
                }
                event = websocket.recv() => {
                    let Some(event) = event else {
                        shutdown.terminate(ShutdownReason::Unexpected);
                        break;
                    };
                    let Some(sync) = sync.upgrade() else { break; };
                    sync.on_websocket(event, &mut coverage).await
                }
                event = grpc.recv() => {
                    let Some(event) = event else {
                        shutdown.terminate(ShutdownReason::Unexpected);
                        break;
                    };
                    let Some(sync) = sync.upgrade() else { break; };
                    match event {
                        grpc::Event::Disconnected { stream, error } => {
                            tracing::error!(stream, %error, "gRPC stream stopped");
                            shutdown.terminate(ShutdownReason::Error(Box::new(error)));
                            break;
                        }
                        event => sync.on_grpc(event, &mut coverage).await,
                    }
                }
                _ = tick.tick() => {
                    let Some(sync) = sync.upgrade() else { break; };
                    sync.scan(&mut coverage).await
                }
            };
            if let Err(error) = result {
                if matches!(error, Error::Grpc(_)) {
                    shutdown.terminate(ShutdownReason::Error(Box::new(error)));
                    break;
                }
                tracing::error!(%error, "subscription worker failed");
            }
        }
        if !shutdown.requested() {
            shutdown.terminate(ShutdownReason::Unexpected);
        }
    }

    /// Delivers changed gRPC filters and attempts missing WebSocket copies.
    async fn scan(&self, coverage: &Coverage) -> Result<(), Error> {
        for client in &self.grpc {
            client.command(grpc::Command::Rebuild).await?;
        }
        for sub in coverage.missing_ws() {
            self.websocket.subscribe_background(sub).await?;
        }
        Ok(())
    }

    /// Ends logical gRPC interest before its next remote filter rebuild.
    async fn remove_grpc(&self, pubkey: Pubkey) -> Result<(), Error> {
        self.grpc[self.grpc_stream(pubkey)]
            .command(grpc::Command::Remove(pubkey))
            .await?;
        Ok(())
    }

    /// Applies WebSocket coverage changes and filters buffered account updates.
    async fn on_websocket(
        &self,
        event: websocket::Event,
        coverage: &mut Coverage,
    ) -> Result<(), Error> {
        match event {
            websocket::Event::Acknowledged { sub, at, background } => {
                if let Some(generation) = coverage.acknowledged(sub, background) {
                    self.grpc[self.grpc_stream(sub.pubkey)]
                        .command(grpc::Command::Track(sub, generation, at))
                        .await?;
                } else if background {
                    self.websocket.cancel_background(sub.pubkey).await?;
                }
            }
            websocket::Event::Removed(pubkey) => {
                let target = coverage.removed(pubkey);
                self.remove_grpc(pubkey).await?;
                if let Some(target) = target {
                    self.evict(target).await?;
                }
            }
            websocket::Event::Update { sub, account } => {
                if coverage.ws_contains(sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        tracing::error!(source = "WS", %sub.pubkey, %error, "account update failed");
                    }
                }
            }
            websocket::Event::Dropped { connection, pubkeys, error } => {
                tracing::warn!(?connection, lost = pubkeys.len(), %error, "WebSocket subscriptions lost");
                for pubkey in pubkeys {
                    let (ended, target) = coverage.lost(Source::WebSocket, pubkey);
                    if ended {
                        self.remove_grpc(pubkey).await?;
                        self.websocket.cancel_background(pubkey).await?;
                    }
                    if let Some(target) = target {
                        self.evict(target).await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Removes a target only when Engine still considers it non-authoritative.
    async fn evict(&self, target: Pubkey) -> Result<(), Error> {
        if let Some(accessor) = self.engine.account(target).await?.into_eviction() {
            accessor.delete().await?;
        }
        Ok(())
    }

    /// Applies per-stream coverage, account, and delegation lifecycle events.
    async fn on_grpc(&self, event: grpc::Event, coverage: &mut Coverage) -> Result<(), Error> {
        match event {
            grpc::Event::Confirmed { stream, pubkey, generation } => {
                coverage.confirmed(stream, pubkey, generation);
            }
            grpc::Event::Lost(stream) => {
                for pubkey in coverage.stream_keys(stream) {
                    let (ended, target) = coverage.lost(Source::Grpc, pubkey);
                    if ended {
                        self.remove_grpc(pubkey).await?;
                        self.websocket.cancel_background(pubkey).await?;
                    }
                    if let Some(target) = target {
                        self.evict(target).await?;
                    }
                }
            }
            grpc::Event::Update { stream, pubkey, target, account } => {
                let sub = AccountSubscription { pubkey, target };
                if coverage.grpc_contains(stream, sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        tracing::error!(source = "gRPC", stream, %pubkey, %error, "account update failed");
                    }
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
            grpc::Event::Disconnected { error, .. } => return Err(error.into()),
        }
        Ok(())
    }

    /// Loads dependencies for delegated actions before materializing the delegated account.
    pub(super) async fn delegated(&self, delegation: grpc::Delegation) -> Result<(), Error> {
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
}
