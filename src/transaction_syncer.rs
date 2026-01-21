//! Stateless helpers for processing undelegation transactions.

use std::collections::HashMap;

use either::Either;
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
/// Returns a Vec of record_pubkeys for each undelegation found.
pub fn process_update(txn: &SubscribeUpdateTransaction) -> impl Iterator<Item = Pubkey> + '_ {
    let Some(message) = txn
        .transaction
        .as_ref()
        .and_then(|t| t.transaction.as_ref().zip(t.meta.as_ref()))
        .and_then(|(t, m)| m.err.is_none().then_some(t.message.as_ref()).flatten())
    else {
        return Either::Left(std::iter::empty());
    };

    let accounts = &message.account_keys;

    let is_undelegate = |ix: &CompiledInstruction| {
        let program_id = accounts.get(ix.program_id_index as usize)?;
        (program_id == &DELEGATION_PROGRAM_PUBKEY).then_some(())?;

        let (discriminator, _) = ix.data.split_at_checked(DISCRIMINATOR_LEN)?;
        (discriminator[0] == UNDELEGATE_DISCRIMINATOR).then_some(())?;

        ix.accounts
            .get(DELEGATION_RECORD_ACCOUNT_INDEX)
            .and_then(|&idx| accounts.get(idx as usize))
    };

    let undelegations = message
        .instructions
        .iter()
        .filter_map(is_undelegate)
        .filter_map(|bytes| Pubkey::try_from(bytes.as_slice()).ok());
    Either::Right(undelegations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use helius_laserstream::{
        grpc::SubscribeUpdateTransactionInfo,
        solana::storage::confirmed_block::{Message, Transaction, TransactionStatusMeta},
    };

    fn create_undelegate_instruction(
        program_id_index: u32,
        record_account_index: u8,
    ) -> CompiledInstruction {
        let mut data = vec![0u8; DISCRIMINATOR_LEN];
        data[0] = UNDELEGATE_DISCRIMINATOR;
        let mut accounts = vec![0u8; DELEGATION_RECORD_ACCOUNT_INDEX + 1];
        accounts[DELEGATION_RECORD_ACCOUNT_INDEX] = record_account_index;
        CompiledInstruction {
            program_id_index,
            accounts,
            data,
        }
    }

    fn create_other_instruction(program_id_index: u32) -> CompiledInstruction {
        CompiledInstruction {
            program_id_index,
            accounts: vec![0, 1, 2],
            data: vec![0u8; DISCRIMINATOR_LEN],
        }
    }

    fn create_txn_update(
        slot: u64,
        account_keys: Vec<Vec<u8>>,
        instructions: Vec<CompiledInstruction>,
        err: Option<helius_laserstream::solana::storage::confirmed_block::TransactionError>,
    ) -> SubscribeUpdateTransaction {
        SubscribeUpdateTransaction {
            slot,
            transaction: Some(SubscribeUpdateTransactionInfo {
                signature: vec![],
                is_vote: false,
                transaction: Some(Transaction {
                    signatures: vec![],
                    message: Some(Message {
                        header: None,
                        account_keys,
                        recent_blockhash: vec![],
                        instructions,
                        versioned: false,
                        address_table_lookups: vec![],
                    }),
                }),
                meta: Some(TransactionStatusMeta {
                    err,
                    ..Default::default()
                }),
                index: 0,
            }),
        }
    }

    fn random_pubkey() -> Vec<u8> {
        (0..32).map(|i| i as u8).collect()
    }

    fn record_pubkey(seed: u8) -> Vec<u8> {
        vec![seed; 32]
    }

    #[test]
    fn test_process_update_with_undelegation_instruction() {
        let delegation_program = DELEGATION_PROGRAM_PUBKEY.to_vec();
        let record = record_pubkey(42);
        let account_keys = vec![random_pubkey(), delegation_program, record.clone()];
        let instructions = vec![create_undelegate_instruction(1, 2)];
        let txn = create_txn_update(100, account_keys, instructions, None);

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert_eq!(result.len(), 1);
        let expected_record: Pubkey = record.as_slice().try_into().unwrap();
        assert_eq!(result[0], expected_record);
    }

    #[test]
    fn test_process_update_without_undelegation_instruction() {
        let other_program = random_pubkey();
        let account_keys = vec![random_pubkey(), other_program];
        let instructions = vec![create_other_instruction(1)];
        let txn = create_txn_update(100, account_keys, instructions, None);

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert!(result.is_empty());
    }

    #[test]
    fn test_process_update_with_malformed_data() {
        let delegation_program = DELEGATION_PROGRAM_PUBKEY.to_vec();
        let account_keys = vec![random_pubkey(), delegation_program];
        let malformed_ix = CompiledInstruction {
            program_id_index: 1,
            accounts: vec![],
            data: vec![],
        };
        let txn = create_txn_update(100, account_keys, vec![malformed_ix], None);

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert!(result.is_empty());
    }

    #[test]
    fn test_process_update_with_multiple_undelegation_instructions() {
        let delegation_program = DELEGATION_PROGRAM_PUBKEY.to_vec();
        let record1 = record_pubkey(1);
        let record2 = record_pubkey(2);
        let record3 = record_pubkey(3);
        let account_keys = vec![
            random_pubkey(),
            delegation_program,
            record1.clone(),
            record2.clone(),
            record3.clone(),
        ];
        let instructions = vec![
            create_undelegate_instruction(1, 2),
            create_undelegate_instruction(1, 3),
            create_undelegate_instruction(1, 4),
        ];
        let txn = create_txn_update(200, account_keys, instructions, None);

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert_eq!(result.len(), 3);
        let expected1: Pubkey = record1.as_slice().try_into().unwrap();
        let expected2: Pubkey = record2.as_slice().try_into().unwrap();
        let expected3: Pubkey = record3.as_slice().try_into().unwrap();
        assert_eq!(result[0], expected1);
        assert_eq!(result[1], expected2);
        assert_eq!(result[2], expected3);
    }

    #[test]
    fn test_process_update_with_mixed_instructions() {
        let delegation_program = DELEGATION_PROGRAM_PUBKEY.to_vec();
        let other_program = random_pubkey();
        let record = record_pubkey(99);
        let account_keys = vec![
            random_pubkey(),
            delegation_program,
            other_program,
            record.clone(),
        ];
        let instructions = vec![
            create_other_instruction(2),
            create_undelegate_instruction(1, 3),
            create_other_instruction(2),
        ];
        let txn = create_txn_update(300, account_keys, instructions, None);

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert_eq!(result.len(), 1);
        let expected_record: Pubkey = record.as_slice().try_into().unwrap();
        assert_eq!(result[0], expected_record);
    }
}
