/// Confirmed coverage and redundancy scheduling for the event worker.
mod coverage;

use std::{
    collections::{hash_map::DefaultHasher, BTreeMap},
    hash::{Hash, Hasher},
    sync::Arc,
};

use dlp_api::{args::PostDelegationActions, Decrypt};
use engine::PostFinalize;
use nucleus::shutdown::{ShutdownHandle, ShutdownReason};
use solana_account::{AccountBuilder, AccountMode, StateFlags};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use tokio::{sync::mpsc::Receiver, time};
use tracing::{error, warn};

use self::coverage::{Coverage, Source};
use crate::{
    ata, delegation,
    grpc::{self, Command},
    program, websocket, AccountProperty, AccountSubscription, ChainSync, Error, Result,
    SyncAccount, DUPLICATION_DELAY,
};

impl ChainSync {
    /// Stable per-key assignment keeps a recently removed remote filter reusable.
    fn grpc_client(&self, pubkey: Pubkey) -> &grpc::Client {
        let mut hash = DefaultHasher::new();
        pubkey.hash(&mut hash);
        &self.grpc[hash.finish() as usize % self.grpc.len()]
    }

    /// Serializes source events, account application, and Engine eviction.
    pub(super) async fn run(
        self: Arc<Self>,
        mut websocket: Receiver<websocket::Event>,
        mut grpc: Receiver<grpc::Event>,
        mut shutdown: ShutdownHandle,
    ) {
        let mut coverage = Coverage::default();
        let mut tick = time::interval(DUPLICATION_DELAY);
        tick.tick().await;
        // Coverage changes and account application share this loop so buffered updates
        // cannot overtake a source removal or revive an evicted target.
        let reason = loop {
            let result = tokio::select! {
                biased;
                _ = shutdown.signalled() => break ShutdownReason::Signalled,
                Some(event) = websocket.recv() => {
                    self.on_websocket(event, &mut coverage).await
                }
                Some(event) = grpc.recv() => {
                    self.on_grpc(event, &mut coverage).await
                }
                _ = tick.tick() => {
                    for client in &self.grpc {
                        client.command(Command::Rebuild);
                    }
                    continue;
                }
            };
            if let Err(error) = result {
                break ShutdownReason::Error(Box::new(error));
            }
        };
        drop(self);
        shutdown.terminate(reason);
    }

    /// Applies WebSocket coverage changes and filters buffered account updates.
    async fn on_websocket(&self, event: websocket::Event, coverage: &mut Coverage) -> Result<()> {
        match event {
            websocket::Event::Acknowledged(sub) => {
                let gen = coverage.acknowledged(sub);
                self.grpc_client(sub.pubkey).command(Command::Track { sub, gen });
            }
            websocket::Event::Removed(pubkey) => {
                let target = coverage.removed(pubkey);
                self.grpc_client(pubkey).command(Command::Remove(pubkey));
                if let Some(target) = target {
                    self.evict(target).await?;
                }
            }
            websocket::Event::Update { sub, account } => {
                if coverage.ws_contains(sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        error!(source = "WS", %sub.pubkey, %error, "account update failed");
                    }
                }
            }
            websocket::Event::Dropped { pubkeys, error } => {
                warn!(lost = pubkeys.len(), %error, "WebSocket subscriptions lost");
                for pubkey in pubkeys {
                    self.lost(coverage, Source::WebSocket, pubkey).await?;
                }
            }
        }
        Ok(())
    }

    /// Drops a source and ends gRPC interest if no confirmed copy survives.
    async fn lost(&self, coverage: &mut Coverage, source: Source, pubkey: Pubkey) -> Result<()> {
        let (ended, target) = coverage.lost(source, pubkey);
        if ended {
            self.grpc_client(pubkey).command(Command::Remove(pubkey));
        }
        if let Some(target) = target {
            self.evict(target).await?;
        }
        Ok(())
    }

    /// Removes a target only when Engine still considers it non-authoritative.
    async fn evict(&self, target: Pubkey) -> Result<()> {
        if let Some(accessor) = self.engine.account(target).await?.into_eviction() {
            accessor.delete().await?;
        }
        Ok(())
    }

    /// Applies per-stream coverage, account, and delegation lifecycle events.
    async fn on_grpc(&self, event: grpc::Event, coverage: &mut Coverage) -> Result<()> {
        match event {
            grpc::Event::Confirmed { stream, pubkey, gen } => {
                coverage.confirmed(stream, pubkey, gen);
            }
            grpc::Event::Lost(stream) => {
                for pubkey in coverage.stream_keys(stream) {
                    self.lost(coverage, Source::Grpc, pubkey).await?;
                }
            }
            grpc::Event::Update { stream, pubkey, target, account } => {
                let sub = AccountSubscription { pubkey, target };
                if coverage.grpc_contains(stream, sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        error!(source = "gRPC", stream, %pubkey, %error, "account update failed");
                    }
                }
            }
            grpc::Event::Delegated(delegation) => {
                let pubkey = delegation.pubkey;
                if let Err(error) = self.delegated(delegation).await {
                    error!(%pubkey, %error, "delegation failed");
                }
            }
            grpc::Event::Undelegated { pubkeys, slot } => {
                for pubkey in pubkeys {
                    if let Err(error) = self.undelegated(pubkey, slot).await {
                        error!(%pubkey, slot, %error, "undelegation failed");
                    }
                }
            }
        }
        Ok(())
    }

    /// Loads dependencies for delegated actions before materializing the delegated account.
    pub(super) async fn delegated(&self, delegation: grpc::Delegation) -> Result<()> {
        if ata::is_eata(&delegation.account) {
            return self.project_eata(delegation).await;
        }
        let (prepared, dependencies) = self.prepare_delegation(delegation)?;
        self.sync(dependencies).await?;
        self.materialize_delegation(prepared).await
    }

    /// Routes a globally observed eATA delegation to a resident canonical ATA.
    async fn project_eata(&self, delegation: grpc::Delegation) -> Result<()> {
        let Some(candidates) = ata::candidates(delegation.pubkey, &delegation.account) else {
            return Ok(());
        };
        for ata in candidates {
            let accessor = self.engine.account(ata).await?;
            if !accessor.exists() {
                continue;
            }
            let base = self.resident_account(ata)?;
            let Some(base) = base else {
                continue;
            };
            let Some(account) = ata::project(
                ata,
                base,
                delegation.pubkey,
                &delegation.account,
                &delegation.record,
                self.engine.authority(),
            ) else {
                continue;
            };
            if accessor.observed().is_some_and(|(_, slot)| account.read().slot() < slot) {
                continue;
            }
            drop(accessor);
            let projected = grpc::Delegation {
                pubkey: ata,
                account,
                record: delegation.record,
                source_program: delegation.source_program,
            };
            let (prepared, dependencies) = self.prepare_delegation(projected)?;
            self.sync(dependencies).await?;
            self.materialize_delegation(prepared).await?;
            self.unsubscribe([ata]).await;
            return Ok(());
        }
        Ok(())
    }

    /// Reads the current Engine image after acquiring any required account lease.
    fn resident_account(&self, pubkey: Pubkey) -> Result<Option<AccountBuilder>> {
        let accounts = self.engine.accounts();
        let loader = accounts.loader();
        let account = loader
            .read(&pubkey, |account| AccountBuilder::from(account.clone()))
            .map_err(|error| engine::EngineError::State(error.into()))?;
        Ok(account)
    }

    /// Decodes action dependencies without acquiring the delegated account's lease.
    pub(super) fn prepare_delegation(
        &self,
        delegation: grpc::Delegation,
    ) -> Result<(PreparedDelegation, Vec<SyncAccount>)> {
        let grpc::Delegation {
            pubkey,
            account,
            record,
            source_program,
        } = delegation;
        let appended = delegation::appended(&record).ok_or(Error::Record("record too short"))?;
        let (actions, dependencies) = if !appended.is_empty() {
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
            // The target must not hold its lease while these dependencies are acquired.
            dependencies.remove(&pubkey);
            let dependencies = dependencies
                .into_iter()
                .map(|(pubkey, property)| SyncAccount { pubkey, property });
            (Some(actions), dependencies.collect())
        } else {
            (None, Vec::new())
        };
        let prepared = PreparedDelegation {
            pubkey,
            account,
            actions,
            source_program,
        };
        Ok((prepared, dependencies))
    }

    /// Acquires only the target after its action dependencies have resolved.
    pub(super) async fn materialize_delegation(&self, prepared: PreparedDelegation) -> Result<()> {
        let PreparedDelegation {
            pubkey,
            account,
            actions,
            source_program,
        } = prepared;
        let actions = actions.map(|actions| PostFinalize { source_program, actions });
        let accessor = self.engine.account(pubkey).await?;
        let error = match accessor.materialize(account.clone(), actions).await {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        if let Err(rescue_error) = self.rescue_delegation(pubkey, account, source_program).await {
            warn!(%pubkey, %error, %rescue_error, "delegation rescue failed");
            return Err(error.into());
        }
        Ok(())
    }

    /// Reacquires the target after failed materialization before scheduling rescue.
    async fn rescue_delegation(
        &self,
        pubkey: Pubkey,
        account: AccountBuilder,
        source_program: Pubkey,
    ) -> engine::Result<()> {
        let slot = account.read().slot();
        let accessor = self.engine.account(pubkey).await?;
        // A concurrent activation or later lifecycle transition owns this key now.
        let superseded = match accessor.observed() {
            Some((AccountMode::Magic, _)) => true,
            Some((AccountMode::Transient, observed)) if observed == slot => true,
            _ => accessor.skipped(AccountMode::Delegated, slot).is_some(),
        };
        if superseded {
            return Ok(());
        }
        let action = delegation::rescue_action(self.engine.authority(), pubkey);
        let rescue = PostFinalize {
            source_program,
            actions: vec![action],
        };
        accessor.materialize(account, Some(rescue)).await
    }

    /// Deletes the account only when the event is current and the observed mode permits
    /// removal.
    async fn undelegated(&self, pubkey: Pubkey, slot: u64) -> Result<()> {
        let accessor = self.engine.account(pubkey).await?;
        if accessor.exists() {
            return Self::undelegate_target(accessor, slot).await;
        }
        drop(accessor);
        let snapshot = self.fetcher.fetch(&[pubkey], Some(slot)).await?;
        let Some(account) = snapshot.accounts.into_iter().next().flatten() else {
            return Ok(());
        };
        let Some(candidates) = ata::candidates(pubkey, &account) else {
            return Ok(());
        };
        for ata in candidates {
            let accessor = self.engine.account(ata).await?;
            if !accessor.exists() {
                continue;
            }
            let Some(account) = self.resident_account(ata)? else {
                continue;
            };
            if ata::projected_for(ata, pubkey, &account) {
                Self::undelegate_target(accessor, slot).await?;
            }
        }
        Ok(())
    }

    /// Applies the existing undelegation lifecycle rule to one Engine target.
    async fn undelegate_target(accessor: engine::AccountAccessor<'_>, slot: u64) -> Result<()> {
        let pubkey = accessor.pubkey();
        let Some((mode, observed)) = accessor.observed() else {
            return Ok(());
        };
        if slot < observed || (mode.authoritative() && mode != AccountMode::Transient) {
            warn!(%pubkey, slot, observed, ?mode, "undelegation cannot remove account");
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
    ) -> Result<()> {
        let AccountSubscription { pubkey, target } = subscription;
        if target.is_none() && ata::is_raw_eata(pubkey, &account) {
            self.unsubscribe([pubkey]).await;
            return Ok(());
        }
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

/// Delegated image and trusted actions ready to apply after dependency acquisition.
pub(super) struct PreparedDelegation {
    pubkey: Pubkey,
    account: AccountBuilder,
    actions: Option<Vec<Instruction>>,
    /// Logical owner from the validated delegation record, including eATA projection.
    source_program: Pubkey,
}
