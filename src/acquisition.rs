use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use dlp_api::pda::delegation_record_pda_from_delegated_account;
use engine::AccountAccessor;
use futures::future;
use solana_account::{AccountBuilder, AccountMode, StateFlags};
use solana_pubkey::Pubkey;
use solana_sdk_ids::bpf_loader_upgradeable;

use crate::{
    ata, delegation, grpc,
    http::Snapshot,
    metrics::{self, Op},
    program, AccountProperty, AccountSubscription, ChainSync, ChainSyncAccount, Result,
};

impl ChainSync {
    /// Acquires action dependencies before activating the delegations that need them.
    pub(super) async fn sync_waves(&self, mut accounts: Vec<ChainSyncAccount>) -> Result<usize> {
        let _timer = metrics::time(Op::ChainSync);
        let mut count = 0;
        accounts.sort_unstable_by_key(|account| account.pubkey);
        accounts.dedup_by_key(|account| account.pubkey);
        // Keep the initial WebSocket subscription while refetching an account with its companion.
        let mut subs = BTreeSet::new();
        let mut floor = None;
        let mut deferred = Vec::new();
        while !accounts.is_empty() {
            let mut next = BTreeMap::new();
            let mut next_floor = None;
            let mut delegations = Vec::new();
            let mut start = 0;
            while start < accounts.len() {
                // Count companion addresses toward the RPC limit of 100 accounts per request.
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
                let outcome = match self.sync_batch(batch, &subs, floor, &mut count).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.unsubscribe(subs).await;
                        return Err(error);
                    }
                };
                next_floor = next_floor.max(outcome.promotion_slot);
                delegations.extend(outcome.delegations);
                for request in batch {
                    subs.remove(&request.pubkey);
                }
                for ChainSyncAccount { pubkey, property } in outcome.promotions {
                    next.insert(pubkey, property);
                    subs.insert(pubkey);
                }
                start = end;
            }

            floor = next_floor;
            // Batch leases are already released, so dependency fetches can acquire overlapping keys.
            let mut ready = Vec::new();
            for PendingDelegation { delegation, unsubscribe } in delegations {
                let (prepared, dependencies) = match self.prepare_delegation(delegation) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.unsubscribe(subs).await;
                        return Err(error);
                    }
                };
                for ChainSyncAccount { pubkey, property } in dependencies {
                    // A program needs ProgramData; a writable account needs its delegation record.
                    // Do not let a readonly request for the same key drop either companion.
                    let current = next.entry(pubkey).or_insert(property);
                    if property == AccountProperty::Program
                        || (*current == AccountProperty::Readonly
                            && property == AccountProperty::Writable)
                    {
                        *current = property;
                    }
                }
                ready.push((prepared, unsubscribe));
            }
            deferred.push(ready);
            accounts = next
                .into_iter()
                .map(|(pubkey, property)| ChainSyncAccount { pubkey, property })
                .collect();
        }
        // A dependency's own actions must finish before the action that requested it.
        for (prepared, unsubscribe) in deferred.into_iter().rev().flatten() {
            let pubkey = prepared.pubkey;
            self.materialize_delegation(prepared).await?;
            if unsubscribe {
                self.unsubscribe([pubkey]).await;
            }
        }
        Ok(count)
    }

    /// Acquires missing accounts and reports follow-up work after releasing their leases.
    async fn sync_batch(
        &self,
        batch: &[ChainSyncAccount],
        carried_subscriptions: &BTreeSet<Pubkey>,
        min_slot: Option<u64>,
        count: &mut usize,
    ) -> Result<BatchOutcome> {
        // Engine rechecks presence under ordered leases, preventing overlapping syncs from
        // fetching the same missing accounts.
        let keys: Vec<_> = batch.iter().map(|account| account.pubkey).collect();
        let accessors = self.engine.missing_accounts(&keys).await?;
        let plan = FetchPlan::new(batch, accessors, carried_subscriptions);
        if plan.accounts.is_empty() {
            self.unsubscribe(plan.skipped).await;
            return Ok(BatchOutcome::default());
        }
        self.subscribe(&plan.subscriptions).await?;
        let fetched = async {
            let mut snapshot = self.fetcher.fetch(&plan.keys, min_slot).await?;
            *count += plan.keys.len();
            let prune = plan.subscriptions_to_remove(&snapshot);
            program::normalize_batch(&plan.programs, &mut snapshot.accounts, self.engine.rent())?;
            let projected = self.fetch_ata_companions(&plan, &snapshot, count).await?;
            Ok((snapshot, prune, projected))
        }
        .await;
        let (mut snapshot, prune, projected) = match fetched {
            Ok(fetched) => fetched,
            Err(error) => {
                self.unsubscribe(plan.subscriptions.iter().map(|sub| sub.pubkey)).await;
                return Err(error);
            }
        };
        let outcome = self
            .materialize_batch(
                plan.accounts,
                &mut snapshot,
                projected,
                carried_subscriptions,
            )
            .await?;
        self.unsubscribe(prune.into_iter().chain(plan.skipped)).await;
        Ok(outcome)
    }

    /// Resolves delegated eATA balances without subscribing to or materializing raw eATAs.
    /// Projections preserve primary-account order for matching during materialization.
    async fn fetch_ata_companions(
        &self,
        plan: &FetchPlan<'_>,
        snapshot: &Snapshot,
        count: &mut usize,
    ) -> Result<Vec<grpc::Delegation>> {
        let mut atas = Vec::new();
        for pending in &plan.accounts {
            let Some(base) = snapshot.accounts[pending.index].as_ref() else { continue };
            let pubkey = pending.accessor.pubkey();
            let Some(eata) = ata::companion(pubkey, base) else { continue };
            atas.push((pubkey, eata, base));
        }
        let mut projected = Vec::new();
        let authority = self.engine.authority();
        for chunk in atas.chunks(50) {
            let mut keys = Vec::with_capacity(chunk.len() * 2);
            for &(_, eata, _) in chunk {
                keys.push(eata);
                keys.push(delegation_record_pda_from_delegated_account(&eata));
            }
            let companions = self.fetcher.fetch(&keys, Some(snapshot.slot)).await?;
            *count += keys.len();
            for (&(pubkey, eata, base), pair) in
                chunk.iter().zip(companions.accounts.chunks_exact(2))
            {
                let Some(delegated) = pair[0].as_ref() else {
                    continue;
                };
                let Some((metadata, record)) =
                    delegation::snapshot_record(delegated, pair[1].as_ref(), authority)
                else {
                    continue;
                };
                let Some(account) =
                    ata::project(pubkey, base.clone(), eata, delegated, record, authority)
                else {
                    continue;
                };
                projected.push(grpc::Delegation {
                    pubkey,
                    account,
                    source_program: metadata.owner,
                    record: record.to_vec(),
                });
            }
        }
        Ok(projected)
    }

    /// Materializes snapshots, deferring accounts that need companions or action dependencies.
    async fn materialize_batch(
        &self,
        accounts: Vec<PendingAccount<'_>>,
        snapshot: &mut Snapshot,
        projected: Vec<grpc::Delegation>,
        carried_subscriptions: &BTreeSet<Pubkey>,
    ) -> Result<BatchOutcome> {
        let mut outcome = BatchOutcome::default();
        let mut projected = projected.into_iter().peekable();
        for pending in accounts {
            let pubkey = pending.accessor.pubkey();
            // Absence is an observation at this snapshot slot, not a slot-zero image.
            let account = snapshot.accounts[pending.index]
                .take()
                .unwrap_or_else(|| AccountBuilder::default().slot(snapshot.slot));
            if ata::is_raw_eata(pubkey, &account) {
                if pending.property != AccountProperty::Writable {
                    self.unsubscribe([pubkey]).await;
                }
                continue;
            }
            let projection = projected.next_if(|delegation| delegation.pubkey == pubkey);
            let is_projection = projection.is_some();
            let (mut account, mut record) = match projection {
                Some(delegation) => (
                    delegation.account,
                    Some((delegation.source_program, Cow::Owned(delegation.record))),
                ),
                None => (account, None),
            };
            if record.is_none() && pending.property == AccountProperty::Readonly {
                let image = account.read();
                // Refetch with the required companion while keeping the initial
                // readonly subscription active.
                let property = if image.owner() == dlp_api::id() {
                    Some(AccountProperty::Writable)
                } else if image.owner() == bpf_loader_upgradeable::ID
                    && image.flags().contains(StateFlags::EXECUTABLE)
                {
                    Some(AccountProperty::Program)
                } else {
                    None
                };
                if let Some(property) = property {
                    // Do not install an incomplete image: the next wave refetches the primary
                    // beside its newly discovered companion under a fresh Engine lease.
                    outcome.promotions.push(ChainSyncAccount { pubkey, property });
                    outcome.promotion_slot = outcome.promotion_slot.max(Some(image.slot()));
                    continue;
                }
            }
            let record_index = matches!(
                pending.property,
                AccountProperty::Payer | AccountProperty::Writable
            )
            .then_some(pending.index + 1);
            if let Some((metadata, bytes)) =
                record_index.filter(|_| record.is_none()).and_then(|index| {
                    delegation::snapshot_record(
                        &account,
                        snapshot.accounts[index].as_ref(),
                        self.engine.authority(),
                    )
                })
            {
                account = delegation::account(account, metadata);
                record = Some((metadata.owner, Cow::Borrowed(bytes)));
            }
            let unsubscribe = record.is_some()
                && (is_projection
                    || pending.property == AccountProperty::Payer
                    || carried_subscriptions.contains(&pubkey));
            let has_actions = record
                .as_ref()
                .and_then(|(_, record)| delegation::appended(record))
                .is_some_and(|actions| !actions.is_empty());
            let defer = account.read().is(AccountMode::Delegated) && has_actions;
            if let Some((source_program, record)) = record.filter(|_| defer) {
                let delegation = grpc::Delegation {
                    pubkey,
                    account,
                    record: record.into_owned(),
                    source_program,
                };
                outcome.delegations.push(PendingDelegation { delegation, unsubscribe });
                continue;
            }
            pending.accessor.materialize(account, None).await?;
            if unsubscribe {
                self.unsubscribe([pubkey]).await;
            }
        }
        Ok(outcome)
    }

    /// Waits for every subscription request; on failure, releases references acquired by this batch.
    async fn subscribe(&self, subscriptions: &[AccountSubscription]) -> Result<()> {
        // Wait for all requests so one failure cannot leave another successful request untracked.
        let requests = subscriptions.iter().map(|&sub| self.websocket.subscribe(sub));
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

    /// Releases one subscription reference per remote address, not per local destination.
    /// Disconnect and pool-shutdown errors are ignored during cleanup.
    pub(super) async fn unsubscribe(&self, keys: impl IntoIterator<Item = Pubkey>) {
        let pending = keys.into_iter().map(|key| self.websocket.unsubscribe(key));
        let _ = future::join_all(pending).await;
    }
}

/// One leased account and its positions in the ordered HTTP fetch batch.
struct PendingAccount<'engine> {
    /// Lease held through snapshot processing unless the account is deferred for actions.
    accessor: AccountAccessor<'engine>,
    /// Selects companion-account and payer cleanup behavior.
    property: AccountProperty,
    /// Primary position in `FetchPlan::keys`; any companion immediately follows it.
    index: usize,
}

/// Delegated account awaiting action dependencies after its initial lease is released.
struct PendingDelegation {
    delegation: grpc::Delegation,
    /// Whether to release the primary subscription reference after materialization succeeds.
    unsubscribe: bool,
}

/// Work discovered from one batch after its account leases have been released.
#[derive(Default)]
struct BatchOutcome {
    /// Readonly accounts to refetch with delegation-record or ProgramData companions.
    promotions: Vec<ChainSyncAccount>,
    /// Minimum slot for refetching: the highest snapshot slot among accounts needing companions.
    promotion_slot: Option<u64>,
    /// Delegated accounts whose action dependencies must resolve before materialization.
    delegations: Vec<PendingDelegation>,
}

/// Ordered fetch inputs and subscriptions derived from a batch of missing accounts.
struct FetchPlan<'engine> {
    accounts: Vec<PendingAccount<'engine>>,
    /// Primary and companion keys in HTTP response order.
    keys: Vec<Pubkey>,
    subscriptions: Vec<AccountSubscription>,
    /// Program and ProgramData positions used during normalization.
    programs: Vec<(usize, usize)>,
    /// Refetches skipped because Engine now has the account; their carried subscriptions need release.
    skipped: Vec<Pubkey>,
}

impl<'e> FetchPlan<'e> {
    fn new(
        batch: &[ChainSyncAccount],
        accessors: Vec<AccountAccessor<'e>>,
        carried_subscriptions: &BTreeSet<Pubkey>,
    ) -> Self {
        // Engine returns missing accessors in pubkey order, matching the sorted request batch.
        let mut accessors = accessors.into_iter().peekable();
        let len = accessors.len();
        let mut plan = Self {
            accounts: Vec::with_capacity(len),
            keys: Vec::with_capacity(len * 2),
            subscriptions: Vec::with_capacity(len * 2),
            programs: Vec::new(),
            skipped: Vec::new(),
        };
        for request in batch {
            let Some(accessor) = accessors.next_if(|accessor| accessor.pubkey() == request.pubkey)
            else {
                if carried_subscriptions.contains(&request.pubkey) {
                    plan.skipped.push(request.pubkey);
                }
                continue;
            };
            let pubkey = request.pubkey;
            let index = plan.keys.len();
            plan.keys.push(pubkey);
            // An account queued for refetch keeps its subscription from the first fetch.
            if request.property != AccountProperty::Writable
                && !carried_subscriptions.contains(&pubkey)
            {
                plan.subscriptions.push(AccountSubscription { pubkey, program: None });
            }
            // Companions immediately follow their primary, so its property determines their position.
            match request.property {
                AccountProperty::Payer | AccountProperty::Writable => {
                    plan.keys.push(delegation_record_pda_from_delegated_account(&pubkey));
                }
                AccountProperty::Program => {
                    let data = AccountSubscription::program_data(pubkey);
                    let data_index = plan.keys.len();
                    plan.keys.push(data.pubkey);
                    plan.subscriptions.push(data);
                    plan.programs.push((index, data_index));
                }
                AccountProperty::Readonly => {}
            }
            plan.accounts.push(PendingAccount {
                accessor,
                property: request.property,
                index,
            });
        }
        plan
    }

    /// Chooses the unused subscription: the program for Loader V3, ProgramData for other loaders.
    fn subscriptions_to_remove(&self, snapshot: &Snapshot) -> Vec<Pubkey> {
        let mut prune = Vec::new();
        for &(program_index, data_index) in &self.programs {
            let Some(program) = snapshot.accounts[program_index].as_ref() else { continue };
            let index = if program.read().owner() == bpf_loader_upgradeable::ID {
                program_index
            } else {
                data_index
            };
            prune.push(self.keys[index]);
        }
        prune
    }
}
