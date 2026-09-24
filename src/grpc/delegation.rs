use super::Error;
use ahash::AHashMap;
use dlp_api::{pda::delegation_record_pda_from_delegated_account, state::DelegationRecord};
use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::SubscribeUpdateAccountInfo;

/// Delegation resolved from an application account and its canonical record.
pub struct Delegation {
    /// Application account's public key.
    pub pubkey: Pubkey,
    /// Account with original owner, `Delegated` mode, and creation slot.
    pub account: AccountBuilder,
    /// Full record, including appended post-delegation actions.
    pub record: Vec<u8>,
    /// Creation transaction signature for action provenance and deduplication.
    pub signature: [u8; 64],
}

/// Application image awaiting its canonical delegation record.
struct Candidate {
    /// Application account, not the record PDA.
    pubkey: Pubkey,
    /// Raw image whose original owner is still unresolved.
    image: SubscribeUpdateAccountInfo,
}

impl Candidate {
    /// Restores the original owner and creation slot from the matched record.
    fn resolve(self, record: Record, slot: u64) -> Delegation {
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
            signature: record.signature,
        }
    }
}

/// Creation metadata retained until its application account arrives.
struct Record {
    /// Original program owner from the canonical record.
    owner: Pubkey,
    /// Creation transaction for action provenance.
    signature: [u8; 64],
    /// Full record, including appended actions.
    data: Vec<u8>,
}

/// One side of an unresolved same-slot delegation.
enum PendingDelegation {
    /// Application image arrived first.
    Account(Candidate),
    /// Canonical record arrived first.
    Record(Record),
    /// Record must not activate an application image in this slot.
    Ignored,
}

/// Matches application accounts with canonical records within one slot.
pub(super) struct Delegations {
    /// Only delegations for this validator may activate.
    authority: Pubkey,
    /// Slot shared by pending observations; replay may move backward.
    slot: u64,
    /// Unresolved halves and ignored markers keyed by record PDA.
    pending: AHashMap<Pubkey, PendingDelegation>,
}

impl Delegations {
    /// Starts matching with no pending observations.
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

    /// Validates a record candidate and pairs it with a pending application image.
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
        let signature = account
            .txn_signature
            .as_deref()
            .ok_or(Error::Protocol("missing delegation transaction signature"))?
            .try_into()
            .map_err(|_| Error::Protocol("invalid delegation transaction signature"))?;
        let record = Record {
            owner: metadata.owner,
            signature,
            data: account.data.clone(),
        };
        Ok(self.observe(key, PendingDelegation::Record(record)))
    }

    /// Pairs an application image with its canonical record PDA.
    pub(super) fn account(
        &mut self,
        key: Pubkey,
        account: SubscribeUpdateAccountInfo,
    ) -> Option<Delegation> {
        let record = delegation_record_pda_from_delegated_account(&key);
        let candidate = Candidate { pubkey: key, image: account };
        self.observe(record, PendingDelegation::Account(candidate))
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
