use super::Error;
use ahash::AHashMap;
use dlp_api::{pda::delegation_record_pda_from_delegated_account, state::DelegationRecord};
use solana_account::{AccountBuilder, AccountMode, OwnedAccount};
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::SubscribeUpdateAccountInfo;

/// A new delegation resolved from its application account and canonical record.
/// The creation slot is carried by `account.slot()`.
pub struct Delegation {
    /// Delegated application account.
    pub pubkey: Pubkey,
    /// Original owner, `Delegated` mode, and creation slot are already resolved.
    pub account: OwnedAccount,
    /// Complete delegation-record bytes, retaining appended post-delegation actions.
    pub record: Vec<u8>,
    /// Transaction identity for action provenance and caller-side deduplication.
    pub signature: [u8; 64],
}

/// Application image with its public key already decoded at the stream boundary.
struct Candidate {
    /// Application account identity, not its record PDA.
    pubkey: Pubkey,
    /// Raw image awaiting the original owner from its record.
    image: SubscribeUpdateAccountInfo,
}

impl Candidate {
    /// Restores the original owner and attaches the matched creation record.
    fn resolve(self, record: Record, slot: u64) -> Delegation {
        let account = AccountBuilder::default()
            .owner(record.owner)
            .lamports(self.image.lamports)
            .data(self.image.data)
            .slot(slot)
            .mode(AccountMode::Delegated)
            .build();
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
    /// Original program owner authenticated by the canonical record PDA.
    owner: Pubkey,
    /// Transaction that created the record, retained for post-delegation actions.
    signature: [u8; 64],
    /// Full record, including appended actions.
    data: Vec<u8>,
}

/// One side of a delegation whose other account update has not arrived yet.
/// A completed delegation leaves this map immediately. Ignored records retain only
/// a marker so that a later application-account update is discarded too.
enum PendingDelegation {
    /// Application account received before its delegation record.
    Account(Candidate),
    /// Delegation record received before its application account.
    Record(Record),
    /// Another validator's delegation, or a commit after the creation slot.
    Ignored,
}

/// Resolves new delegations without depending on account/record arrival order.
/// Matching requires the canonical record PDA and the same slot. The caller's
/// at-most-one-delegation-per-account-per-slot contract makes signature matching unnecessary.
pub(super) struct Delegations {
    /// Only records naming this validator can activate an account.
    authority: Pubkey,
    /// Slot shared by all pending observations; replay may move it backward.
    slot: u64,
    /// Unfinished delegations and ignored markers, keyed by canonical record PDA.
    pending: AHashMap<Pubkey, PendingDelegation>,
}

impl Delegations {
    /// Starts discovery for one validator with no pending observations.
    pub(super) fn new(authority: Pubkey) -> Self {
        Self {
            authority,
            slot: 0,
            pending: AHashMap::new(),
        }
    }

    /// Updates are grouped by slot, including replay, so matches cannot span slot changes.
    pub(super) fn set_slot(&mut self, slot: u64) {
        if self.slot != slot {
            self.pending.clear();
            self.slot = slot;
        }
    }

    /// Processes record-shaped data without excluding its use as application data.
    /// PDA matching, not the discriminator alone, authenticates the record's identity.
    pub(super) fn record(
        &mut self,
        key: Pubkey,
        account: &SubscribeUpdateAccountInfo,
        metadata: &DelegationRecord,
    ) -> Result<Option<Delegation>, Error> {
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

    /// Matches an application account against its canonical delegation-record PDA.
    pub(super) fn account(
        &mut self,
        key: Pubkey,
        account: SubscribeUpdateAccountInfo,
    ) -> Option<Delegation> {
        let record = delegation_record_pda_from_delegated_account(&key);
        let candidate = Candidate { pubkey: key, image: account };
        self.observe(record, PendingDelegation::Account(candidate))
    }

    /// Completes matching account/record observations; ignores remain effective for the slot.
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
