use std::collections::BTreeMap;

use dlp_api::{args::PostDelegationActions, Decrypt};
use engine::PostFinalize;
use solana_account::{AccountBuilder, AccountMode};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use tracing::warn;

use crate::{ata, delegation, grpc, AccountProperty, ChainSync, Error, Result, SyncAccount};

impl ChainSync {
    /// Loads dependencies for delegated actions before materializing the delegated account.
    pub(super) async fn delegated(&self, delegation: grpc::Delegation) -> Result<()> {
        if ata::is_eata(&delegation.account) {
            return self.materialize_eata_delegation(delegation).await;
        }
        // Confined accounts have no commit authority: no actions, dependencies, or rescue.
        if delegation.account.read().is(AccountMode::Magic) {
            self.engine
                .account(delegation.pubkey)
                .await?
                .materialize(delegation.account, None)
                .await?;
            return Ok(());
        }
        let (prepared, dependencies) = self.prepare_delegation(delegation)?;
        self.sync(dependencies).await?;
        self.materialize_delegation(prepared).await
    }

    /// Projects a globally observed eATA delegation onto a resident canonical ATA.
    async fn materialize_eata_delegation(&self, delegation: grpc::Delegation) -> Result<()> {
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
    pub(crate) fn prepare_delegation(
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
        // Keep invalid actions with the trusted image so every acquisition path
        // can schedule rescue without trying to load their dependencies.
        let mut dependencies = BTreeMap::new();
        let actions: Result<_> = (!appended.is_empty())
            .then(|| {
                let compact: PostDelegationActions = borsh::from_slice(appended)?;
                let actions = compact.decrypt_with_keypair(self.engine.signer())?;
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
                Ok(actions)
            })
            .transpose();
        // The target must not hold its lease while these dependencies are acquired.
        dependencies.remove(&pubkey);
        let dependencies = dependencies
            .into_iter()
            .map(|(pubkey, property)| SyncAccount { pubkey, property })
            .collect();
        let prepared = PreparedDelegation {
            pubkey,
            account,
            actions,
            source_program,
        };
        Ok((prepared, dependencies))
    }

    /// Acquires only the target after its action dependencies have resolved.
    pub(crate) async fn materialize_delegation(&self, prepared: PreparedDelegation) -> Result<()> {
        let PreparedDelegation {
            pubkey,
            account,
            actions,
            source_program,
        } = prepared;
        let result = match actions {
            Ok(actions) => {
                let actions = actions.map(|actions| PostFinalize { source_program, actions });
                let accessor = self.engine.account(pubkey).await?;
                accessor.materialize(account.clone(), actions).await.map_err(Error::from)
            }
            Err(error) => Err(error),
        };
        let Err(error) = result else { return Ok(()) };
        warn!(%pubkey, %error, "delegation activation failed; scheduling undelegation");
        if let Err(rescue_error) = self.rescue_delegation(pubkey, account, source_program).await {
            warn!(%pubkey, %error, %rescue_error, "delegation rescue failed");
            return Err(error);
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
    pub(super) async fn undelegated(&self, pubkey: Pubkey, slot: u64) -> Result<()> {
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
            if ata::is_projection_of(ata, pubkey, &account) {
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
}

/// Delegated image and trusted actions ready to apply after dependency acquisition.
pub(crate) struct PreparedDelegation {
    /// Account whose lease is acquired only after action dependencies resolve.
    pubkey: Pubkey,
    /// Delegated image to materialize under that lease.
    account: AccountBuilder,
    /// Decoded actions, or a payload error that requires rescue undelegation.
    actions: Result<Option<Vec<Instruction>>>,
    /// Logical owner from the validated delegation record, including eATA projection.
    source_program: Pubkey,
}
