use ahash::AHashMap;
use dlp_api::{pda::delegation_record_pda_from_delegated_account, state::DelegationRecord};
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::SubscribeUpdateAccountInfo;

use crate::delegation;

/// Delegation matched to an application account's delegation record PDA.
pub struct Delegation {
    /// Application account's public key.
    pub pubkey: Pubkey,
    /// Account with original owner, `Delegated` or confined `Magic` mode, and delegation slot.
    pub account: AccountBuilder,
    /// Full record, including appended post-delegation actions.
    pub record: Vec<u8>,
    /// Logical owner validated by the matched delegation record.
    pub source_program: Pubkey,
}

/// Matches application accounts with their delegation record PDA updates in one slot.
pub(super) struct Delegations {
    /// Delegations for this validator and authority-free confinement may activate.
    authority: Pubkey,
    /// Slot shared by pending observations; replay may move backward.
    slot: u64,
    /// Unresolved halves and ignored markers keyed by record PDA.
    pending: AHashMap<Pubkey, PendingDelegation>,
}

/// DLP-owned application account state awaiting its matching delegation-record update.
struct PendingAccount {
    /// Application account, not the record PDA.
    pubkey: Pubkey,
    /// Raw streamed account state awaiting restoration of its original owner.
    update: SubscribeUpdateAccountInfo,
}

impl PendingAccount {
    /// Builds Engine account state from the application update and its matching record.
    fn resolve(self, record: PendingRecord) -> Delegation {
        let account =
            AccountBuilder::default().lamports(self.update.lamports).data(self.update.data);
        Delegation {
            pubkey: self.pubkey,
            account: delegation::account(account, &record.metadata),
            record: record.data,
            source_program: record.metadata.owner,
        }
    }
}

/// Validated delegation record update awaiting its application account.
struct PendingRecord {
    /// Validated ownership, authority, and delegation slot.
    metadata: DelegationRecord,
    /// Full record, including appended actions.
    data: Vec<u8>,
}

/// One side of an unresolved same-slot delegation.
enum PendingDelegation {
    /// Application account update arrived first.
    Account(PendingAccount),
    /// Delegation record arrived first.
    Record(PendingRecord),
    /// Record must not activate an application account update in this slot.
    Ignored,
}

impl Delegations {
    /// Tracks delegations for one validator.
    pub(super) fn new(authority: Pubkey) -> Self {
        Self {
            authority,
            slot: 0,
            pending: AHashMap::new(),
        }
    }

    /// Discards incomplete matches whenever the stream changes slots.
    pub(super) fn set_slot(&mut self, slot: u64) {
        if self.slot != slot {
            self.pending.clear();
            self.slot = slot;
        }
    }

    /// Only a local or confined record for this slot can activate an application
    /// update. Other records leave an ignored marker for the rest of the slot.
    pub(super) fn record(
        &mut self,
        key: Pubkey,
        account: &SubscribeUpdateAccountInfo,
        metadata: &DelegationRecord,
    ) -> Option<Delegation> {
        // Keep an ignored marker so a later application update cannot form a match.
        if !delegation::belongs_to(metadata, self.authority)
            || metadata.delegation_slot != self.slot
        {
            return self.observe(key, PendingDelegation::Ignored);
        }
        let record = PendingRecord {
            metadata: *metadata,
            data: account.data.clone(),
        };
        self.observe(key, PendingDelegation::Record(record))
    }

    /// Adds an application account update to the pending same-slot delegation match.
    pub(super) fn account(
        &mut self,
        pubkey: Pubkey,
        update: SubscribeUpdateAccountInfo,
    ) -> Option<Delegation> {
        // Record updates are keyed by this account's derived delegation-record PDA.
        let record = delegation_record_pda_from_delegated_account(&pubkey);
        let pending = PendingAccount { pubkey, update };
        self.observe(record, PendingDelegation::Account(pending))
    }

    /// Completes a delegation when both matching updates are present.
    fn observe(&mut self, key: Pubkey, incoming: PendingDelegation) -> Option<Delegation> {
        use PendingDelegation::*;

        let Some(current) = self.pending.remove(&key) else {
            self.pending.insert(key, incoming);
            return None;
        };
        let pending = match (current, incoming) {
            (Account(account), Record(record)) | (Record(record), Account(account)) => {
                return Some(account.resolve(record));
            }
            // An irrelevant record blocks activation for this slot; otherwise keep
            // the newest unmatched update for a later counterpart in that same slot.
            (Ignored, _) | (_, Ignored) => Ignored,
            (_, pending) => pending,
        };
        self.pending.insert(key, pending);
        None
    }
}
