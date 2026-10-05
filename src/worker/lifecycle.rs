use std::collections::BTreeMap;

use dlp_api::{args::PostDelegationActions, Decrypt};
use engine::{AccountAccessor, EngineError, PostFinalize};
use solana_account::{AccountBuilder, AccountMode};
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use tracing::{error, warn};

use super::coverage::Coverage;
use crate::{
    aml, ata, delegation, grpc,
    metrics::{self, Op},
    AccountProperty, ChainSync, ChainSyncAccount, Error, Result,
};

impl ChainSync {
    /// Activates a delegation only after retiring base-chain mirror coverage.
    pub(super) async fn delegated(
        &self,
        mut delegation: grpc::Delegation,
        coverage: &mut Coverage,
    ) -> Result<()> {
        if ata::is_eata(&delegation.account) {
            let Some(projection) = self.project_delegation(delegation).await? else {
                return Ok(());
            };
            delegation = projection;
        }
        // Confirmed ownership no longer permits mirroring, even if activation later fails.
        self.unsubscribe_account(delegation.pubkey, coverage).await?;
        if delegation.account.read().is(AccountMode::Magic) {
            // Confined accounts have no commit authority: no actions, dependencies, or rescue.
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

    /// Projects a gRPC eATA delegation onto a matching ATA already present in Engine.
    async fn project_delegation(
        &self,
        delegation: grpc::Delegation,
    ) -> Result<Option<grpc::Delegation>> {
        let Some(candidates) = ata::candidates(delegation.pubkey, &delegation.account) else {
            return Ok(None);
        };
        for ata in candidates {
            let accessor = self.engine.account(ata).await?;
            if !accessor.exists() {
                continue;
            }
            let Some(base) = self.resident_account(ata)? else {
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
            return Ok(Some(grpc::Delegation {
                pubkey: ata,
                account,
                record: delegation.record,
                source_program: delegation.source_program,
            }));
        }
        Ok(None)
    }

    /// Copies the current Engine account image; callers hold its lease when checking lifecycle state.
    fn resident_account(&self, pubkey: Pubkey) -> Result<Option<AccountBuilder>> {
        let accounts = self.engine.accounts();
        let loader = accounts.loader();
        let account = loader
            .read(&pubkey, |account| AccountBuilder::from(account.clone()))
            .map_err(|error| EngineError::State(error.into()))?;
        Ok(account)
    }

    /// Decodes and decrypts actions, returning their dependencies without taking an account lease.
    pub(crate) fn prepare_delegation(
        &self,
        delegation: grpc::Delegation,
    ) -> Result<(PreparedDelegation, Vec<ChainSyncAccount>)> {
        let grpc::Delegation {
            pubkey,
            account,
            record,
            source_program,
        } = delegation;
        let appended = delegation::appended(&record).ok_or(Error::Record("record too short"))?;
        // Retain decoding errors with the validated delegation so materialization
        // can schedule rescue instead of trying to fetch unusable action dependencies.
        let actions = if appended.is_empty() {
            Ok(None)
        } else {
            match borsh::from_slice::<PostDelegationActions>(appended) {
                Ok(actions) => actions
                    .decrypt_with_keypair(self.engine.signer())
                    .map(Some)
                    .map_err(Error::from),
                Err(error) => Err(error.into()),
            }
        };
        let mut dependencies = BTreeMap::new();
        if let Ok(Some(actions)) = &actions {
            for action in actions {
                dependencies.insert(action.program_id, AccountProperty::Program);
                for meta in &action.accounts {
                    let property =
                        dependencies.entry(meta.pubkey).or_insert(AccountProperty::Readonly);
                    if meta.is_writable {
                        *property = AccountProperty::Writable;
                    }
                }
            }
        }
        // The delegated account itself is materialized later; do not fetch it as a dependency.
        dependencies.remove(&pubkey);
        let dependencies = dependencies
            .into_iter()
            .map(|(pubkey, property)| ChainSyncAccount { pubkey, property })
            .collect();
        let prepared = PreparedDelegation {
            pubkey,
            account,
            actions,
            source_program,
        };
        Ok((prepared, dependencies))
    }

    /// Activates a delegation whose action dependencies have already been acquired.
    /// Invalid actions, rejected signers, and materialization failures enter rescue;
    /// an unavailable assessment service returns an error without rescue.
    pub(crate) async fn materialize_delegation(&self, prepared: PreparedDelegation) -> Result<()> {
        let _timer = metrics::time(Op::Delegate);
        let PreparedDelegation {
            pubkey,
            account,
            mut actions,
            source_program,
        } = prepared;
        // An unavailable AML service is not a rejection: return without scheduling rescue.
        if let (Some(aml), Ok(Some(instructions))) = (&self.aml, &actions) {
            let rejected = aml::check(aml, instructions).await?;
            if !rejected.is_empty() {
                actions = Err(aml::Error::Rejected(rejected).into());
            }
        }
        let result = match actions {
            Ok(actions) => {
                let actions = actions.map(|actions| PostFinalize { source_program, actions });
                let accessor = self.engine.account(pubkey).await?;
                accessor.materialize(account.clone(), actions).await.map_err(Error::from)
            }
            Err(error) => Err(error),
        };
        let Err(error) = result else { return Ok(()) };
        metrics::activation_failed();
        warn!(%pubkey, %error, "activation failed; rescuing");
        if let Err(rescue_error) = self.rescue_delegation(pubkey, account, source_program).await {
            metrics::rescue("failed");
            error!(%pubkey, %error, %rescue_error, "rescue failed");
            return Err(error);
        }
        Ok(())
    }

    /// Acquires the account's lease and schedules rescue unless local state already supersedes it.
    pub(super) async fn rescue_delegation(
        &self,
        pubkey: Pubkey,
        account: AccountBuilder,
        source_program: Pubkey,
    ) -> engine::Result<()> {
        let slot = account.read().slot();
        let accessor = self.engine.account(pubkey).await?;
        // Skip rescue for an already handled delegation, newer state, or confined account.
        let superseded = match accessor.observed() {
            Some((AccountMode::Magic, _)) => true,
            Some((AccountMode::Transient, observed)) if observed == slot => true,
            _ => accessor.skipped(AccountMode::Delegated, slot).is_some(),
        };
        if superseded {
            metrics::rescue("superseded");
            return Ok(());
        }
        let action = delegation::undelegation_action(self.engine.authority(), pubkey);
        let rescue = PostFinalize {
            source_program,
            actions: vec![action],
        };
        accessor.materialize(account, Some(rescue)).await?;
        metrics::rescue("scheduled");
        Ok(())
    }

    /// Schedules current local state only for a request at or after its delegation slot.
    /// The serialized worker and resulting Transient mode suppress repeated observations;
    /// confirmed undelegation, not scheduling success, permits eventual cleanup.
    pub(super) async fn undelegation_requested(
        &self,
        pubkey: Pubkey,
        slot: u64,
    ) -> engine::Result<()> {
        let accessor = self.engine.account(pubkey).await?;
        if !matches!(accessor.observed(), Some((AccountMode::Delegated, at)) if slot >= at) {
            return Ok(());
        }
        let action = delegation::undelegation_action(self.engine.authority(), pubkey);
        // Hold the lease through completion to serialize ChainSync lifecycle mutations.
        self.engine.transaction(&[action])?.execute().await??;
        drop(accessor);
        Ok(())
    }

    /// Handles undelegation by deleting an eligible local account or its projected ATAs.
    pub(super) async fn undelegated(&self, pubkey: Pubkey, slot: u64) -> Result<()> {
        let _timer = metrics::time(Op::Undelegate);
        let accessor = self.engine.account(pubkey).await?;
        if accessor.exists() {
            return Self::undelegate_account(accessor, slot).await;
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
                Self::undelegate_account(accessor, slot).await?;
            }
        }
        Ok(())
    }

    /// Deletes a present account unless the event is older or its mode is authoritative.
    /// `Transient` accounts may be deleted: their undelegation is already in progress.
    async fn undelegate_account(accessor: AccountAccessor<'_>, slot: u64) -> Result<()> {
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

/// Delegated account and action-decoding result held while action dependencies are fetched.
pub(crate) struct PreparedDelegation {
    pub(crate) pubkey: Pubkey,
    pub(super) account: AccountBuilder,
    /// Decrypted actions, or a decoding/decryption error that requires rescue undelegation.
    pub(super) actions: Result<Option<Vec<Instruction>>>,
    /// Original owning program from the validated delegation record.
    /// For an eATA projection, this is the eATA program, not the ATA's token program.
    pub(super) source_program: Pubkey,
}
