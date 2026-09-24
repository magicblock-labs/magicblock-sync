use super::Error;
use dlp_api::discriminator::DlpDiscriminator;
use solana_pubkey::Pubkey;
use yellowstone_grpc_proto::prelude::{
    CompiledInstruction, InnerInstruction, SubscribeUpdateTransactionInfo,
};

/// Borrowed instruction fields shared by top-level and CPI ownership-return decoding.
struct Instruction<'a> {
    /// Program index into the transaction's static account keys.
    program: u32,
    /// Instruction account indices into the same key list.
    accounts: &'a [u8],
    /// DLP discriminator and instruction arguments.
    data: &'a [u8],
}

impl<'a> From<&'a CompiledInstruction> for Instruction<'a> {
    fn from(instruction: &'a CompiledInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

impl<'a> From<&'a InnerInstruction> for Instruction<'a> {
    fn from(instruction: &'a InnerInstruction) -> Self {
        Self {
            program: instruction.program_id_index,
            accounts: &instruction.accounts,
            data: &instruction.data,
        }
    }
}

/// Extracts ownership returns from a successful transaction's top-level and CPI instructions.
/// An executed CPI failure aborts the transaction, so success needs no log or stack parsing.
/// The subscription supplies successful non-vote transactions with CPI metadata.
/// Relevant program and account indices must refer to static transaction keys.
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
