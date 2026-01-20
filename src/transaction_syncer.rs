//! Stateless helpers for processing undelegation transactions.

#![allow(dead_code)]

use std::collections::HashMap;

use helius_laserstream::{
    grpc::{SubscribeRequestFilterTransactions, SubscribeUpdateTransaction},
    solana::storage::confirmed_block::CompiledInstruction,
};

use crate::consts::{
    DELEGATION_PROGRAM, DELEGATION_PROGRAM_PUBKEY, DELEGATION_RECORD_ACCOUNT_INDEX,
    DISCRIMINATOR_LEN, UNDELEGATE_DISCRIMINATOR,
};
use crate::types::Pubkey;

/// Creates the subscription filter for undelegation transactions.
///
/// Returns a HashMap to be merged into the SubscribeRequest's transactions field.
pub fn create_filter() -> HashMap<String, SubscribeRequestFilterTransactions> {
    let mut transactions = HashMap::new();
    let tx_filter = SubscribeRequestFilterTransactions {
        account_include: vec![DELEGATION_PROGRAM.into()],
        ..Default::default()
    };
    transactions.insert("undelegations".into(), tx_filter);
    transactions
}

/// Processes a transaction update and extracts undelegated pubkeys.
///
/// Returns a Vec of (record_pubkey, slot) tuples for each undelegation found.
pub fn process_update(txn: &SubscribeUpdateTransaction) -> Vec<(Pubkey, u64)> {
    let mut undelegations = Vec::new();

    let Some(message) = txn
        .transaction
        .as_ref()
        .and_then(|t| t.transaction.as_ref().zip(t.meta.as_ref()))
        .and_then(|(t, m)| m.err.is_none().then_some(t.message.as_ref()).flatten())
    else {
        return undelegations;
    };

    let accounts = &message.account_keys;

    let is_undelegate = |ix: &CompiledInstruction| {
        let program_id = accounts.get(ix.program_id_index as usize)?;
        (program_id == DELEGATION_PROGRAM_PUBKEY).then_some(())?;

        let (discriminator, _) = ix.data.split_at_checked(DISCRIMINATOR_LEN)?;
        (discriminator[0] == UNDELEGATE_DISCRIMINATOR).then_some(())?;

        ix.accounts
            .get(DELEGATION_RECORD_ACCOUNT_INDEX)
            .and_then(|&idx| accounts.get(idx as usize))
    };

    for record_bytes in message.instructions.iter().filter_map(is_undelegate) {
        if let Ok(record) = Pubkey::try_from(record_bytes.as_slice()) {
            undelegations.push((record, txn.slot));
        }
    }

    undelegations
}
