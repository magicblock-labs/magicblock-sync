use std::pin::Pin;

use futures::StreamExt;
use helius_laserstream::{
    grpc::{subscribe_update::UpdateOneof, SubscribeUpdate, SubscribeUpdateTransaction},
    solana::storage::confirmed_block::CompiledInstruction,
    LaserstreamError,
};
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::types::{AccountUpdate, Pubkey};

/// Delegation program pubkey in bytes.
const DELEGATION_PROGRAM_PUBKEY: &Pubkey = &[
    181, 183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184, 53, 6, 235, 140, 229,
    25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
];

/// Instruction discriminator for undelegate operations.
const UNDELEGATE_DISCRIMINATOR: u8 = 3;

/// Length of an instruction discriminator (Anchor programs).
const DISCRIMINATOR_LEN: usize = 8;

/// Index of the delegation record account in undelegate instruction accounts.
const DELEGATION_RECORD_ACCOUNT_INDEX: usize = 6;

/// Maximum pending account/transaction updates.
const MAX_PENDING_UPDATES: usize = 8192;

/// Stream type alias for Laserstream updates.
pub type LaserStream =
    Pin<Box<dyn futures::Stream<Item = Result<SubscribeUpdate, LaserstreamError>> + Send>>;

/// Synchronization service for delegation program transactions.
///
/// Monitors the Laserstream for undelegation transactions and broadcasts
/// updates via an MPSC channel.
pub struct DlpTransactionSyncer {
    /// The Laserstream update stream.
    stream: LaserStream,
    /// Sender for broadcasting updates to subscribers.
    updates: Sender<AccountUpdate>,
}

impl DlpTransactionSyncer {
    /// Creates a new transaction syncer.
    ///
    /// # Arguments
    ///
    /// * `stream` - The Laserstream update stream.
    ///
    /// # Returns
    ///
    /// Returns the syncer and a receiver for account updates.
    pub fn new(stream: LaserStream) -> (Self, Receiver<AccountUpdate>) {
        let (updates_tx, updates_rx) = mpsc::channel(MAX_PENDING_UPDATES);

        let syncer = Self {
            stream,
            updates: updates_tx,
        };

        (syncer, updates_rx)
    }

    /// Main event loop for the transaction synchronization service.
    ///
    /// Processes updates from the Laserstream until the stream closes.
    pub async fn run(mut self) {
        while let Some(update) = self.stream.next().await {
            self.handle_update(update);
        }

        let _ = self.updates.send(AccountUpdate::SyncTerminated).await;
    }

    /// Handles an update from the Laserstream.
    fn handle_update(&mut self, result: Result<SubscribeUpdate, LaserstreamError>) {
        use UpdateOneof::*;

        let update = match result {
            Ok(u) => match u.update_oneof {
                Some(update) => update,
                None => return,
            },
            Err(error) => {
                tracing::warn!(%error, "error during transaction stream processing");
                return;
            }
        };

        if let Transaction(txn) = update {
            self.handle_transaction_update(txn);
        }
    }

    /// Handles a transaction update, extracting undelegations.
    fn handle_transaction_update(&self, txn: SubscribeUpdateTransaction) {
        let Some(message) = txn
            .transaction
            .and_then(|t| t.transaction.zip(t.meta))
            .and_then(|(t, m)| m.err.is_none().then_some(t.message))
            .flatten()
        else {
            return;
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
            let Ok(record) = Pubkey::try_from(record_bytes.as_slice()) else {
                continue;
            };

            let update = AccountUpdate::Undelegated {
                record,
                slot: txn.slot,
            };

            if let Err(error) = self.updates.try_send(update) {
                tracing::error!(%error, "failed to send undelegation update");
            }
        }
    }
}