use std::collections::HashMap;

use futures::StreamExt;
use helius_laserstream::{
    grpc::{
        subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestFilterTransactions,
        SubscribeUpdate, SubscribeUpdateTransaction,
    },
    solana::storage::confirmed_block::CompiledInstruction,
    LaserstreamConfig, LaserstreamError,
};
use tokio::sync::mpsc::Sender;

use crate::types::{AccountUpdate, DlpSyncError, Pubkey};
use crate::{
    connect,
    consts::{
        DELEGATION_PROGRAM, DELEGATION_PROGRAM_PUBKEY, DELEGATION_RECORD_ACCOUNT_INDEX,
        DISCRIMINATOR_LEN, UNDELEGATE_DISCRIMINATOR,
    },
};

pub use crate::connect::LaserStream;

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
    /// Creates transaction filters for undelegation monitoring.
    pub fn create_filters() -> HashMap<String, SubscribeRequestFilterTransactions> {
        let mut transactions = HashMap::new();

        // Subscribe to undelegation transactions
        let tx_filter = SubscribeRequestFilterTransactions {
            account_include: vec![DELEGATION_PROGRAM.into()],
            ..Default::default()
        };
        transactions.insert("undelegations".into(), tx_filter);

        transactions
    }

    /// Establishes a connection to the Laserstream for transaction monitoring.
    ///
    /// Creates filters for undelegation transactions and establishes the connection
    /// using the shared connect module.
    pub async fn connect(
        config: LaserstreamConfig,
        sender: Sender<AccountUpdate>,
    ) -> Result<Self, DlpSyncError> {
        let transactions = Self::create_filters();

        let request = SubscribeRequest {
            accounts: HashMap::new(),
            slots: HashMap::new(),
            transactions,
            ..Default::default()
        };

        let stream = connect::connect(&config, request).await?;

        Ok(Self {
            stream,
            updates: sender,
        })
    }

    /// Processes a single transaction update.
    ///
    /// Extracts undelegation events and sends them via the update channel.
    pub(crate) fn process(&self, txn: SubscribeUpdateTransaction) {
        self.handle_transaction_update(txn);
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
