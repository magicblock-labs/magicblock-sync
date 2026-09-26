use super::Error;
use ahash::AHashMap;
use dlp_api::{pda::delegation_record_pda_from_delegated_account, state::DelegationRecord};
use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::SubscribeUpdateAccountInfo;

/// Delegation matched to an application account's delegation record PDA.
pub struct Delegation {
    /// Application account's public key.
    pub pubkey: Pubkey,
    /// Account with original owner, `Delegated` mode, and matched delegation slot.
    pub account: AccountBuilder,
    /// Full record, including appended post-delegation actions.
    pub record: Vec<u8>,
}

/// DLP-owned application image awaiting its delegation record PDA update.
struct PendingAccount {
    /// Application account, not the record PDA.
    pubkey: Pubkey,
    /// Raw image whose original owner is still unresolved.
    image: SubscribeUpdateAccountInfo,
}

impl PendingAccount {
    /// Restores the original owner and matched delegation slot.
    fn resolve(self, record: PendingRecord, slot: u64) -> Delegation {
        let account = AccountBuilder::default()
            .owner(record.owner)
            .lamports(self.image.lamports)
            .data(self.image.data)
            .slot(slot)
            .mode(AccountMode::Delegated);
        Delegation {
            pubkey: self.pubkey,
            account,
            record: record.data,
        }
    }
}

/// Validated delegation record update awaiting its application account.
struct PendingRecord {
    /// Original program owner from the delegation record.
    owner: Pubkey,
    /// Full record, including appended actions.
    data: Vec<u8>,
}

/// One side of an unresolved same-slot delegation.
enum PendingDelegation {
    /// Application image arrived first.
    Account(PendingAccount),
    /// Delegation record arrived first.
    Record(PendingRecord),
    /// Record must not activate an application image in this slot.
    Ignored,
}

/// Matches application accounts with their delegation record PDA updates in one slot.
pub(super) struct Delegations {
    /// Only delegations for this validator may activate.
    authority: Pubkey,
    /// Slot shared by pending observations; replay may move backward.
    slot: u64,
    /// Unresolved halves and ignored markers keyed by record PDA.
    pending: AHashMap<Pubkey, PendingDelegation>,
}

impl Delegations {
    /// Restricts matches to this authority; the first stream update establishes
    /// the matching slot.
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

    /// Only a record for this authority and slot can activate an application
    /// image. Other records leave an ignored marker for the rest of the slot.
    pub(super) fn record(
        &mut self,
        key: Pubkey,
        account: &SubscribeUpdateAccountInfo,
        metadata: &DelegationRecord,
    ) -> Result<Option<Delegation>, Error> {
        // Keep an ignored marker so a later application update cannot form a match.
        if metadata.authority != self.authority || metadata.delegation_slot != self.slot {
            return Ok(self.observe(key, PendingDelegation::Ignored));
        }
        let record = PendingRecord {
            owner: metadata.owner,
            data: account.data.clone(),
        };
        Ok(self.observe(key, PendingDelegation::Record(record)))
    }

    /// Keys the application image by its derived record PDA so either update
    /// order can complete a same-slot match.
    pub(super) fn account(
        &mut self,
        key: Pubkey,
        account: SubscribeUpdateAccountInfo,
    ) -> Option<Delegation> {
        let record = delegation_record_pda_from_delegated_account(&key);
        let pending = PendingAccount { pubkey: key, image: account };
        self.observe(record, PendingDelegation::Account(pending))
    }

    /// Resolves opposite halves or retains the latest unmatched observation.
    fn observe(&mut self, key: Pubkey, incoming: PendingDelegation) -> Option<Delegation> {
        use PendingDelegation::*;

        let Some(current) = self.pending.remove(&key) else {
            self.pending.insert(key, incoming);
            return None;
        };
        let pending = match (current, incoming) {
            (Account(account), Record(record)) | (Record(record), Account(account)) => {
                return Some(account.resolve(record, self.slot));
            }
            (Ignored, _) | (_, Ignored) => Ignored,
            (_, pending) => pending,
        };
        self.pending.insert(key, pending);
        None
    }
}
