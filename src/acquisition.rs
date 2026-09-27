use dlp_api::pda::delegation_record_pda_from_delegated_account;
use futures::future;
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use solana_sdk_ids::bpf_loader_upgradeable;

use crate::{
    delegation, grpc, http::Snapshot, program, AccountProperty, AccountSubscription, ChainSync,
    Error, SyncAccount,
};

impl ChainSync {
    /// Acquires missing accounts and applies the fetched snapshot and any delegation actions.
    pub(super) async fn sync_batch(&self, batch: &[SyncAccount]) -> Result<(), Error> {
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
