use dlp_api::state::DelegationRecord;
use solana_account::{AccountBuilder, AccountMode};
use solana_pubkey::Pubkey;

/// Decodes the shared on-chain delegation-record representation.
pub(crate) fn record(data: &[u8]) -> Option<&DelegationRecord> {
    DelegationRecord::try_from_bytes_with_discriminator(data).ok()
}

/// Whether a decoded delegation record activates for this authority.
pub(crate) fn belongs_to(record: &DelegationRecord, authority: Pubkey) -> bool {
    record.authority == authority
}

/// Returns the optional post-delegation payload following the record metadata.
pub(crate) fn appended(data: &[u8]) -> Option<&[u8]> {
    data.get(DelegationRecord::size_with_discriminator()..)
}

/// Resolves an HTTP snapshot's application account and delegation record.
pub(crate) fn snapshot_record<'a>(
    account: &AccountBuilder,
    record_account: Option<&'a AccountBuilder>,
    authority: Pubkey,
) -> Option<(&'a DelegationRecord, &'a [u8])> {
    if account.read().owner() != dlp_api::id() {
        return None;
    }
    let record_account = record_account?;
    if record_account.read().owner() != dlp_api::id() {
        return None;
    }
    let data = record_account.read().data();
    let metadata = record(data)?;
    belongs_to(metadata, authority).then_some((metadata, data))
}

/// Configures the Engine representation after a caller validates the record.
pub(crate) fn account(account: AccountBuilder, owner: Pubkey, slot: u64) -> AccountBuilder {
    account.owner(owner).slot(slot).mode(AccountMode::Delegated)
}
