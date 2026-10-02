use dlp_api::state::DelegationRecord;
use magicblock_magic_program_api::{
    args::{CommitAndUndelegateArgs, CommitTypeArgs, MagicIntentBundleArgs, UndelegateTypeArgs},
    id,
    instruction::MagicBlockInstruction,
    MAGIC_CONTEXT_PUBKEY,
};
use solana_account::{AccountBuilder, AccountMode};
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;

/// Decodes the shared on-chain delegation-record representation.
pub(crate) fn record(data: &[u8]) -> Option<&DelegationRecord> {
    DelegationRecord::try_from_bytes_with_discriminator(data).ok()
}

/// Accepts this Engine's authority or the default authority used for confined accounts.
pub(crate) fn belongs_to(record: &DelegationRecord, authority: Pubkey) -> bool {
    record.authority == authority || record.authority == Pubkey::default()
}

/// Returns the optional post-delegation payload following the record metadata.
pub(crate) fn appended(data: &[u8]) -> Option<&[u8]> {
    data.get(DelegationRecord::size_with_discriminator()..)
}

/// Returns a parsed record only if both snapshots are DLP-owned and its authority is accepted.
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

/// Restores the recorded owning program and slot, then selects delegated or confined mode.
/// Requires a validated record whose authority is local or permits authority-free confinement.
pub(crate) fn account(account: AccountBuilder, record: &DelegationRecord) -> AccountBuilder {
    let account = account.owner(record.owner).slot(record.delegation_slot);
    // No authority can commit a confined account back to the base chain.
    if record.authority == Pubkey::default() {
        account.lamports(0).mode(AccountMode::Magic)
    } else {
        account.mode(AccountMode::Delegated)
    }
}

/// Builds the commit-and-undelegate rescue action for a validated delegation that failed activation.
/// MagicRoot vouches for the readonly authority signer during PostFinalize.
pub(crate) fn rescue_action(authority: Pubkey, pubkey: Pubkey) -> Instruction {
    // Authority and Magic Context occupy action indices 0 and 1.
    let commit_type = CommitTypeArgs::Standalone(vec![2]);
    let undelegate = CommitAndUndelegateArgs {
        commit_type,
        undelegate_type: UndelegateTypeArgs::Standalone,
    };
    let args = MagicIntentBundleArgs {
        commit_and_undelegate: Some(undelegate),
        ..Default::default()
    };
    let instruction = MagicBlockInstruction::ScheduleIntentBundle(args);
    let accounts = vec![
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new(MAGIC_CONTEXT_PUBKEY, false),
        AccountMeta::new(pubkey, false),
    ];
    Instruction::new_with_wincode(id(), &instruction, accounts)
}
