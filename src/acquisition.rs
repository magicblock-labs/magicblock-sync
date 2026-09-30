use std::collections::{BTreeMap, BTreeSet};

use dlp_api::pda::delegation_record_pda_from_delegated_account;
use futures::future;
use solana_account::{AccountBuilder, StateFlags};
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use solana_sdk_ids::bpf_loader_upgradeable;

use crate::{
    ata, delegation, grpc, http::Snapshot, program, AccountProperty, AccountSubscription,
    ChainSync, Result, SyncAccount,
};

impl ChainSync {
    /// Resolves each acquisition wave before applying actions that depend on later waves.
    pub(super) async fn sync_waves(&self, mut accounts: Vec<SyncAccount>) -> Result<()> {
        let mut carried = BTreeSet::new();
        let mut floor = None;
        let mut deferred = Vec::new();
        while !accounts.is_empty() {
            accounts.sort_unstable_by_key(|account| account.pubkey);
            accounts.dedup_by_key(|account| account.pubkey);
            let mut promotions = Vec::new();
            let mut next_floor = None;
            let mut actions = Vec::new();
            let mut start = 0;
            while start < accounts.len() {
                // Companions consume RPC positions even though Engine leases only primaries.
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
                let outcome = match self.sync_batch(batch, &carried, floor).await {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.unsubscribe(carried.into_iter().chain(
                            promotions.into_iter().map(|account: SyncAccount| account.pubkey),
                        ))
                        .await;
                        return Err(error);
                    }
                };
                next_floor = next_floor.max(outcome.slot);
                promotions.extend(outcome.promotions);
                actions.extend(outcome.actions);
                for request in batch {
                    carried.remove(&request.pubkey);
                }
                start = end;
            }

            let mut next = BTreeMap::new();
            floor = next_floor;
            // Every batch lease is gone before dependencies can acquire overlapping keys.
            for account in promotions {
                let SyncAccount { pubkey, property } = account;
                next.insert(pubkey, property);
                carried.insert(pubkey);
            }
            let mut ready = Vec::new();
            for PendingDelegation { delegation, unsubscribe } in actions {
                let pubkey = delegation.pubkey;
                let (prepared, dependencies) = match self.prepare_delegation(delegation) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        self.unsubscribe(carried).await;
                        return Err(error);
                    }
                };
                for SyncAccount { pubkey, property } in dependencies {
                    // Keep the stronger companion request when actions name the same key.
                    let current = next.entry(pubkey).or_insert(property);
                    if property == AccountProperty::Program
                        || (*current == AccountProperty::Readonly
                            && property == AccountProperty::Writable)
                    {
                        *current = property;
                    }
                }
                ready.push((prepared, pubkey, unsubscribe));
            }
            // The final reverse traverses dependency waves first, but keeps peers in order.
            deferred.extend(ready.into_iter().rev());
            if next.is_empty() {
                break;
            }
            accounts = next
                .into_iter()
                .map(|(pubkey, property)| SyncAccount { pubkey, property })
                .collect();
        }
        // A dependency's own actions must finish before the action that requested it.
        for (prepared, pubkey, unsubscribe) in deferred.into_iter().rev() {
            self.materialize_delegation(prepared).await?;
            if unsubscribe {
                self.unsubscribe([pubkey]).await;
            }
        }
        Ok(())
    }

    /// Acquires missing accounts and reports follow-up work after releasing their leases.
    async fn sync_batch(
        &self,
        batch: &[SyncAccount],
        carried: &BTreeSet<Pubkey>,
        min_slot: Option<u64>,
    ) -> Result<BatchOutcome> {
        // Engine rechecks presence under ordered leases, preventing overlapping syncs from
        // fetching the same missing accounts.
        let keys: Vec<_> = batch.iter().map(|account| account.pubkey).collect();
        let accessors = self.engine.missing_accounts(&keys).await?;
        // A competing wave may have materialized a promoted key while its lease was released.
        let skipped: Vec<_> = keys
            .iter()
            .copied()
            .filter(|key| {
                carried.contains(key) && !accessors.iter().any(|accessor| accessor.pubkey() == *key)
            })
            .collect();
        if accessors.is_empty() {
            self.unsubscribe(skipped).await;
            return Ok(BatchOutcome::default());
        }
        let plan = FetchPlan::new(batch, accessors, carried);
        let (mut snapshot, prune) = self.fetch_batch(&plan, min_slot).await?;
        let projected = match self.fetch_ata_companions(&plan, &snapshot).await {
            Ok(projected) => projected,
            Err(error) => {
                self.unsubscribe(plan.subscriptions.iter().map(|sub| sub.pubkey)).await;
                return Err(error);
            }
        };
        let outcome = self.materialize_batch(plan, &mut snapshot, projected, carried).await?;
        self.unsubscribe(prune.into_iter().chain(skipped)).await;
        Ok(outcome)
    }

    /// Fetches eATA and record pairs after the first image reveals an ATA's seeds.
    /// No companion is subscribed or materialized under its raw address.
    async fn fetch_ata_companions(
        &self,
        plan: &FetchPlan<'_>,
        snapshot: &Snapshot,
    ) -> Result<Vec<Option<(AccountBuilder, Pubkey, Vec<u8>)>>> {
        let mut atas = Vec::new();
        for pending in &plan.accounts {
            let Some(base) = snapshot.accounts[pending.index].as_ref() else { continue };
            let pubkey = pending.accessor.pubkey();
            let Some(eata) = ata::companion(pubkey, base) else { continue };
            atas.push((pubkey, eata, pending.index));
        }
        let mut projected = vec![None; snapshot.accounts.len()];
        let authority = self.engine.authority();
        for chunk in atas.chunks(50) {
            let mut keys = Vec::with_capacity(chunk.len() * 2);
            for &(_, eata, _) in chunk {
                keys.push(eata);
                keys.push(delegation_record_pda_from_delegated_account(&eata));
            }
            let companions = self.fetcher.fetch(&keys, Some(snapshot.slot)).await?;
            for (&(pubkey, eata, base_index), pair) in
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
                let Some(base) = snapshot.accounts[base_index].as_ref() else {
                    continue;
                };
                let projection =
                    ata::project(pubkey, base.clone(), eata, delegated, record, authority);
                let Some(account) = projection else {
                    continue;
                };
                projected[base_index] = Some((account, metadata.owner, record.to_vec()));
            }
        }
        Ok(projected)
    }

    /// Subscribes, fetches, and normalizes planned accounts, cleaning up subscriptions on failure.
    async fn fetch_batch(
        &self,
        plan: &FetchPlan<'_>,
        min_slot: Option<u64>,
    ) -> Result<(Snapshot, Vec<Pubkey>)> {
        self.subscribe(&plan.subscriptions).await?;
        let result = async {
            let mut snapshot = self.fetcher.fetch(&plan.keys, min_slot).await?;
            let prune = plan.pruned_subscriptions(&snapshot);
            program::normalize_batch(&plan.programs, &mut snapshot.accounts, self.engine.rent())?;
            Ok((snapshot, prune))
        }
        .await;
        if result.is_err() {
            let pubkeys = plan.subscriptions.iter().map(|sub| sub.pubkey);
            self.unsubscribe(pubkeys).await;
        }
        result
    }

    /// Materializes complete snapshots and defers readonly discoveries and action targets.
    async fn materialize_batch(
        &self,
        plan: FetchPlan<'_>,
        snapshot: &mut Snapshot,
        mut projected: Vec<Option<(AccountBuilder, Pubkey, Vec<u8>)>>,
        carried: &BTreeSet<Pubkey>,
    ) -> Result<BatchOutcome> {
        let mut outcome = BatchOutcome::default();
        for pending in plan.accounts {
            let pubkey = pending.accessor.pubkey();
            let mut account = snapshot.accounts[pending.index].take().unwrap_or_default();
            if ata::is_raw_eata(pubkey, &account) {
                if pending.property != AccountProperty::Writable {
                    self.unsubscribe([pubkey]).await;
                }
                continue;
            }
            let mut projected_record = None;
            if let Some((image, source_program, record)) = projected[pending.index].take() {
                account = image;
                projected_record = Some((source_program, record));
            }
            if projected_record.is_none() && pending.property == AccountProperty::Readonly {
                let image = account.read();
                // These roles request a companion on the next wave without changing
                // the original readonly subscription's ownership.
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
                    outcome.promotions.push(SyncAccount { pubkey, property });
                    outcome.slot = outcome.slot.max(Some(image.slot()));
                    continue;
                }
            }
            let delegation = match pending.record_index {
                Some(index) if projected_record.is_none() => delegation::snapshot_record(
                    &account,
                    snapshot.accounts[index].as_ref(),
                    self.engine.authority(),
                ),
                _ => None,
            };
            let record = match (projected_record.as_ref(), delegation) {
                (Some((source_program, record)), _) => Some((*source_program, record.as_slice())),
                (None, Some((metadata, record))) => {
                    account =
                        delegation::account(account, metadata.owner, metadata.delegation_slot);
                    Some((metadata.owner, record))
                }
                (None, None) => None,
            };
            let Some((source_program, record)) = record else {
                pending.accessor.materialize(account, None).await?;
                continue;
            };
            let unsubscribe = projected_record.is_some()
                || pending.property == AccountProperty::Payer
                || carried.contains(&pubkey);
            if delegation::appended(record).is_some_and(|actions| !actions.is_empty()) {
                let delegation = grpc::Delegation {
                    pubkey,
                    account,
                    record: record.to_vec(),
                    source_program,
                };
                outcome.actions.push(PendingDelegation { delegation, unsubscribe });
            } else {
                pending.accessor.materialize(account, None).await?;
                if unsubscribe {
                    self.unsubscribe([pubkey]).await;
                }
            }
        }
        Ok(outcome)
    }

    /// Waits for every subscription request and removes successful subscriptions if any fail.
    async fn subscribe(&self, subscriptions: &[AccountSubscription]) -> Result<()> {
        // Settle every admitted request so acknowledged subscriptions can be cleaned up.
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

    /// Releases subscriptions; failed releases mean their socket or pool entry is already gone.
    pub(super) async fn unsubscribe(&self, keys: impl IntoIterator<Item = Pubkey>) {
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
    /// Whether successful materialization should remove the primary subscription.
    unsubscribe: bool,
}

/// Work discovered from one batch after its account leases have been released.
#[derive(Default)]
struct BatchOutcome {
    /// Incomplete readonly primaries needing companions.
    promotions: Vec<SyncAccount>,
    /// Highest first-wave slot among promoted primaries.
    slot: Option<u64>,
    actions: Vec<PendingDelegation>,
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

impl<'e> FetchPlan<'e> {
    /// Pairs ordered missing-account leases with requests and derives fetch inputs.
    fn new(
        batch: &[SyncAccount],
        accessors: Vec<engine::AccountAccessor<'e>>,
        carried: &BTreeSet<Pubkey>,
    ) -> Self {
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
            // A promoted key already has coverage from the discovery fetch.
            if request.property != AccountProperty::Writable && !carried.contains(&pubkey) {
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
