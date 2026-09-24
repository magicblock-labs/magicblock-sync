use super::Error;
use dlp_api::discriminator::DlpDiscriminator;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::{
    CompiledInstruction, InnerInstruction, SubscribeUpdateTransactionInfo,
};

/// Borrowed instruction shape shared by top-level and CPI decoding.
struct Instruction<'a> {
    /// Index into static transaction account keys.
    program: u32,
    /// Account indices into the same key list.
    accounts: &'a [u8],
    /// DLP discriminator and arguments.
    data: &'a [u8],
}

impl<'a> From<&'a CompiledInstruction> for Instruction<'a> {
    /// Borrows a top-level instruction without copying its payload.
    fn from(instruction: &'a CompiledInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

impl<'a> From<&'a InnerInstruction> for Instruction<'a> {
    /// Borrows a CPI instruction in the same shape as a top-level instruction.
    fn from(instruction: &'a InnerInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

/// Finds distinct ownership returns in a successful transaction and its CPIs.
pub(super) fn released(tx: &SubscribeUpdateTransactionInfo) -> Result<Vec<Pubkey>, Error> {
    let meta = tx.meta.as_ref().ok_or(Error::Protocol("missing transaction metadata"))?;
    let message = tx
        .transaction
        .as_ref()
        .and_then(|tx| tx.message.as_ref())
        .ok_or(Error::Protocol("missing transaction message"))?;
    let key = |index: usize| {
        let bytes = message
            .account_keys
            .get(index)
            .ok_or(Error::Protocol("invalid account index"))?;
        super::pubkey(bytes)
    };
    let dlp = dlp_api::id();
    let outer = message.instructions.iter().map(Instruction::from);
    let inner = meta
        .inner_instructions
        .iter()
        .flat_map(|group| &group.instructions)
        .map(Instruction::from);
    let mut released = Vec::new();
    for instruction in outer.chain(inner) {
        if key(instruction.program as usize)? != dlp {
            continue;
        }
        // The successful DLP invocation has validated the instruction; its dispatcher
        // uses the first discriminator byte and ignores the remaining bytes.
        let discriminator =
            instruction.data.first().and_then(|tag| DlpDiscriminator::try_from(*tag).ok());
        let position = match discriminator {
            Some(DlpDiscriminator::Undelegate) => 1,
            Some(DlpDiscriminator::UndelegateWithRollbackAfterTimeout) => 0,
            _ => continue,
        };
        let index = *instruction
            .accounts
            .get(position)
            .ok_or(Error::Protocol("missing undelegated account"))?;
        released.push(key(index as usize)?);
    }
    released.sort_unstable();
    released.dedup();
    Ok(released)
}
