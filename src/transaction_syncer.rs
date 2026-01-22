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

    #[test]
    fn test_process_update_real_undelegate_transaction() {
        use helius_laserstream::solana::storage::confirmed_block::{
            InnerInstruction, InnerInstructions, MessageHeader,
        };

        let account_keys: Vec<Vec<u8>> = vec![
            vec![
                5, 62, 162, 42, 88, 56, 179, 161, 33, 20, 15, 2, 170, 67, 59, 11, 147, 146, 74,
                122, 186, 28, 135, 70, 6, 17, 193, 187, 246, 101, 67, 254,
            ],
            vec![
                2, 72, 123, 149, 154, 28, 42, 5, 203, 133, 136, 151, 147, 6, 51, 17, 210, 14, 230,
                70, 5, 92, 116, 249, 173, 225, 5, 243, 123, 218, 107, 121,
            ],
            vec![
                2, 156, 240, 31, 8, 227, 71, 24, 38, 134, 176, 55, 100, 79, 131, 141, 136, 167,
                250, 23, 143, 90, 223, 105, 3, 218, 146, 242, 186, 116, 159, 245,
            ],
            vec![
                16, 244, 61, 162, 137, 30, 7, 140, 196, 156, 106, 131, 37, 154, 52, 54, 38, 7, 200,
                134, 91, 126, 171, 131, 76, 71, 53, 113, 224, 191, 220, 131,
            ],
            vec![
                93, 185, 64, 160, 51, 203, 91, 169, 63, 16, 190, 250, 6, 127, 50, 34, 83, 69, 202,
                36, 207, 2, 26, 127, 73, 14, 64, 42, 168, 112, 243, 247,
            ],
            vec![
                95, 232, 166, 5, 73, 185, 224, 5, 43, 125, 32, 58, 120, 165, 19, 29, 204, 186, 36,
                171, 140, 16, 143, 110, 129, 187, 102, 247, 116, 214, 217, 104,
            ],
            vec![
                191, 216, 249, 0, 219, 24, 217, 241, 219, 105, 68, 147, 70, 243, 61, 204, 216, 198,
                228, 218, 222, 249, 42, 129, 249, 80, 136, 232, 65, 137, 1, 40,
            ],
            vec![
                206, 188, 216, 84, 252, 33, 46, 58, 47, 194, 4, 232, 107, 53, 162, 90, 194, 1, 254,
                180, 229, 235, 81, 166, 83, 235, 160, 220, 42, 36, 142, 24,
            ],
            vec![
                235, 210, 196, 10, 117, 2, 125, 28, 175, 161, 36, 233, 191, 80, 162, 45, 13, 87,
                204, 254, 249, 45, 4, 175, 236, 3, 249, 6, 117, 220, 24, 8,
            ],
            vec![
                244, 48, 161, 207, 180, 238, 8, 71, 42, 115, 120, 201, 130, 115, 52, 119, 188, 114,
                14, 25, 63, 36, 97, 28, 199, 173, 180, 46, 190, 13, 27, 108,
            ],
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0,
            ],
            vec![
                3, 6, 70, 111, 229, 33, 23, 50, 255, 236, 173, 186, 114, 195, 155, 231, 188, 140,
                229, 187, 197, 247, 18, 107, 44, 67, 155, 58, 64, 0, 0, 0,
            ],
            vec![
                162, 211, 1, 190, 154, 210, 30, 64, 120, 186, 237, 91, 247, 216, 155, 174, 121,
                251, 243, 100, 155, 28, 35, 33, 25, 86, 113, 10, 107, 4, 182, 135,
            ],
            vec![
                181, 183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184, 53, 6,
                235, 140, 229, 25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
            ],
            vec![
                185, 255, 31, 113, 29, 105, 39, 129, 63, 161, 34, 161, 231, 204, 45, 54, 62, 248,
                189, 69, 172, 212, 222, 101, 51, 136, 202, 69, 225, 212, 233, 238,
            ],
        ];

        let instructions = vec![
            CompiledInstruction {
                program_id_index: 11,
                accounts: vec![],
                data: vec![2, 80, 52, 3, 0],
            },
            CompiledInstruction {
                program_id_index: 11,
                accounts: vec![],
                data: vec![3, 128, 56, 1, 0, 0, 0, 0, 0],
            },
            CompiledInstruction {
                program_id_index: 11,
                accounts: vec![],
                data: vec![4, 32, 142, 85, 0],
            },
            CompiledInstruction {
                program_id_index: 13,
                accounts: vec![0, 5, 2, 7, 8, 6, 1, 14, 10],
                data: vec![
                    1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 75, 15, 0, 0, 0, 0, 0, 1,
                    16, 0, 0, 0, 255, 176, 4, 245, 188, 253, 124, 25, 208, 12, 0, 0, 0, 0, 0, 0,
                ],
            },
            CompiledInstruction {
                program_id_index: 13,
                accounts: vec![0, 5, 2, 7, 8, 6, 1, 10],
                data: vec![2, 0, 0, 0, 0, 0, 0, 0],
            },
            CompiledInstruction {
                program_id_index: 13,
                accounts: vec![0, 5, 12, 9, 2, 7, 8, 6, 3, 4, 1, 10],
                data: vec![3, 0, 0, 0, 0, 0, 0, 0],
            },
        ];

        let inner_instructions = vec![
            InnerInstructions {
                index: 3,
                instructions: vec![
                    InnerInstruction {
                        program_id_index: 10,
                        accounts: vec![0, 2],
                        data: vec![
                            0, 0, 0, 0, 0, 75, 15, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 181,
                            183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184,
                            53, 6, 235, 140, 229, 25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
                        ],
                        stack_height: Some(2),
                    },
                    InnerInstruction {
                        program_id_index: 10,
                        accounts: vec![0, 7],
                        data: vec![
                            0, 0, 0, 0, 128, 240, 22, 0, 0, 0, 0, 0, 88, 0, 0, 0, 0, 0, 0, 0, 181,
                            183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184,
                            53, 6, 235, 140, 229, 25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
                        ],
                        stack_height: Some(2),
                    },
                ],
            },
            InnerInstructions {
                index: 5,
                instructions: vec![
                    InnerInstruction {
                        program_id_index: 10,
                        accounts: vec![0, 9],
                        data: vec![
                            0, 0, 0, 0, 0, 75, 15, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 181,
                            183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184,
                            53, 6, 235, 140, 229, 25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
                        ],
                        stack_height: Some(2),
                    },
                    InnerInstruction {
                        program_id_index: 12,
                        accounts: vec![5, 9, 0, 10],
                        data: vec![
                            196, 28, 41, 206, 48, 37, 51, 167, 2, 0, 0, 0, 7, 0, 0, 0, 99, 111,
                            117, 110, 116, 101, 114, 32, 0, 0, 0, 5, 62, 162, 42, 88, 56, 179, 161,
                            33, 20, 15, 2, 170, 67, 59, 11, 147, 146, 74, 122, 186, 28, 135, 70, 6,
                            17, 193, 187, 246, 101, 67, 254,
                        ],
                        stack_height: Some(2),
                    },
                    InnerInstruction {
                        program_id_index: 10,
                        accounts: vec![0, 5],
                        data: vec![
                            0, 0, 0, 0, 0, 75, 15, 0, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 162,
                            211, 1, 190, 154, 210, 30, 64, 120, 186, 237, 91, 247, 216, 155, 174,
                            121, 251, 243, 100, 155, 28, 35, 33, 25, 86, 113, 10, 107, 4, 182, 135,
                        ],
                        stack_height: Some(3),
                    },
                    InnerInstruction {
                        program_id_index: 10,
                        accounts: vec![0, 5],
                        data: vec![2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                        stack_height: Some(2),
                    },
                ],
            },
        ];

        let txn = SubscribeUpdateTransaction {
            slot: 436772071,
            transaction: Some(SubscribeUpdateTransactionInfo {
                signature: vec![
                    22, 112, 23, 161, 121, 241, 200, 28, 204, 245, 38, 39, 199, 158, 125, 48, 148,
                    100, 155, 174, 225, 207, 30, 60, 217, 107, 53, 45, 76, 58, 239, 31, 75, 56, 18,
                    67, 52, 207, 138, 147, 112, 109, 58, 239, 63, 22, 110, 148, 194, 142, 221, 64,
                    184, 151, 80, 118, 187, 161, 254, 80, 50, 53, 236, 2,
                ],
                is_vote: false,
                transaction: Some(Transaction {
                    signatures: vec![vec![
                        22, 112, 23, 161, 121, 241, 200, 28, 204, 245, 38, 39, 199, 158, 125, 48,
                        148, 100, 155, 174, 225, 207, 30, 60, 217, 107, 53, 45, 76, 58, 239, 31,
                        75, 56, 18, 67, 52, 207, 138, 147, 112, 109, 58, 239, 63, 22, 110, 148,
                        194, 142, 221, 64, 184, 151, 80, 118, 187, 161, 254, 80, 50, 53, 236, 2,
                    ]],
                    message: Some(Message {
                        header: Some(MessageHeader {
                            num_required_signatures: 1,
                            num_readonly_signed_accounts: 0,
                            num_readonly_unsigned_accounts: 5,
                        }),
                        account_keys,
                        recent_blockhash: vec![
                            209, 223, 101, 156, 149, 73, 16, 29, 49, 253, 251, 190, 197, 204, 84,
                            130, 130, 147, 250, 9, 28, 172, 9, 188, 110, 174, 188, 249, 70, 223,
                            30, 240,
                        ],
                        instructions,
                        versioned: true,
                        address_table_lookups: vec![],
                    }),
                }),
                meta: Some(TransactionStatusMeta {
                    err: None,
                    fee: 21800,
                    pre_balances: vec![
                        11087665779,
                        492886560,
                        0,
                        117230240,
                        1988489818,
                        1002240,
                        1586880,
                        0,
                        1559040,
                        0,
                        1,
                        1,
                        1141440,
                        1141440,
                        0,
                    ],
                    post_balances: vec![
                        11087643979,
                        493156560,
                        0,
                        120076160,
                        1988519818,
                        1002240,
                        0,
                        0,
                        0,
                        0,
                        1,
                        1,
                        1141440,
                        1141440,
                        0,
                    ],
                    inner_instructions,
                    inner_instructions_none: false,
                    log_messages: vec![
                        "Program ComputeBudget111111111111111111111111111111 invoke [1]".into(),
                        "Program ComputeBudget111111111111111111111111111111 success".into(),
                        "Program DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh invoke [1]".into(),
                        "Program log: Instruction: ProcessUndelegation".into(),
                        "Program DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh success".into(),
                    ],
                    log_messages_none: false,
                    pre_token_balances: vec![],
                    post_token_balances: vec![],
                    rewards: vec![],
                    loaded_writable_addresses: vec![],
                    loaded_readonly_addresses: vec![],
                    return_data: None,
                    return_data_none: true,
                    compute_units_consumed: Some(129839),
                }),
                index: 1,
            }),
        };

        let result = process_update(&txn).collect::<Vec<Pubkey>>();

        assert_eq!(result.len(), 1, "Should detect exactly one undelegation");
        let expected_record: Pubkey = [
            191, 216, 249, 0, 219, 24, 217, 241, 219, 105, 68, 147, 70, 243, 61, 204, 216, 198,
            228, 218, 222, 249, 42, 129, 249, 80, 136, 232, 65, 137, 1, 40,
        ];
        assert_eq!(
            result[0], expected_record,
            "Record pubkey should match account at index 6"
        );
    }
}
