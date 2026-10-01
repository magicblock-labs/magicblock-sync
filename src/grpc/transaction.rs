use super::{Error, Result};
use dlp_api::discriminator::DlpDiscriminator;
use smallvec::SmallVec;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::{
    CompiledInstruction, InnerInstruction, SubscribeUpdateTransactionInfo,
};

/// Borrowed instruction shape shared by top-level and CPI decoding.
struct InstructionView<'a> {
    /// Index into static transaction account keys.
    program: u32,
    /// Account indices into the same key list.
    accounts: &'a [u8],
    /// DLP discriminator and arguments.
    data: &'a [u8],
}

impl<'a> From<&'a CompiledInstruction> for InstructionView<'a> {
    fn from(instruction: &'a CompiledInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

impl<'a> From<&'a InnerInstruction> for InstructionView<'a> {
    fn from(instruction: &'a InnerInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

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
    let outer = message.instructions.iter().map(InstructionView::from);
    // Inner instructions use the same static key table as top-level instructions.
    let inner = meta
        .inner_instructions
        .iter()
        .flat_map(|group| &group.instructions)
        .map(InstructionView::from);
    let mut undelegated = SmallVec::new();
    for instruction in outer.chain(inner) {
        if key(instruction.program as usize)? != dlp {
            continue;
        }
        // The successful DLP invocation has validated the instruction; its dispatcher
        // uses the first discriminator byte and ignores the remaining bytes.
        let Some(&tag) = instruction.data.first() else { continue };
        let Ok(discriminator) = DlpDiscriminator::try_from(tag) else { continue };
        let position = match discriminator {
            DlpDiscriminator::Undelegate => 1,
            DlpDiscriminator::UndelegateWithRollbackAfterTimeout => 0,
            _ => continue,
        };
        let index = *instruction
            .accounts
            .get(position)
            .ok_or(Error::Protocol("missing undelegated account"))?;
        undelegated.push(key(index as usize)?);
    }
    undelegated.sort_unstable();
    undelegated.dedup();
    Ok(undelegated)
}
