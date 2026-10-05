use super::{Error, Result};
use dlp_api::discriminator::DlpDiscriminator;
use smallvec::SmallVec;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::SubscribeUpdateTransactionInfo;

/// Finds distinct undelegated accounts in a successful transaction and its CPIs.
pub(super) fn undelegated_accounts(
    tx: &SubscribeUpdateTransactionInfo,
) -> Result<SmallVec<[Pubkey; 1]>> {
    let meta = tx.meta.as_ref().ok_or(Error::Protocol("missing transaction metadata"))?;
    let transaction =
        tx.transaction.as_ref().ok_or(Error::Protocol("missing transaction message"))?;
    let message = transaction
        .message
        .as_ref()
        .ok_or(Error::Protocol("missing transaction message"))?;
    let key = |index: usize| {
        let bytes = message
            .account_keys
            .get(index)
            .ok_or(Error::Protocol("invalid account index"))?;
        super::pubkey(bytes)
    };
    let dlp = dlp_api::id();
    // Resolve both instruction kinds against static keys only; lookup-table keys are unsupported.
    let outer = message
        .instructions
        .iter()
        .map(|ix| (ix.program_id_index, &ix.accounts, &ix.data));
    let inner = meta
        .inner_instructions
        .iter()
        .flat_map(|group| &group.instructions)
        .map(|ix| (ix.program_id_index, &ix.accounts, &ix.data));
    let mut undelegated = SmallVec::new();
    for (program, accounts, data) in outer.chain(inner) {
        if key(program as usize)? != dlp {
            continue;
        }
        // The successful DLP invocation has validated the instruction; its dispatcher
        // uses the first discriminator byte and ignores the remaining bytes.
        let Some(&tag) = data.first() else { continue };
        let Ok(discriminator) = DlpDiscriminator::try_from(tag) else { continue };
        let position = match discriminator {
            DlpDiscriminator::Undelegate => 1,
            DlpDiscriminator::UndelegateWithRollbackAfterTimeout => 0,
            _ => continue,
        };
        let index =
            *accounts.get(position).ok_or(Error::Protocol("missing undelegated account"))?;
        undelegated.push(key(index as usize)?);
    }
    undelegated.sort_unstable();
    undelegated.dedup();
    Ok(undelegated)
}
