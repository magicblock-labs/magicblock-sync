use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use helius_laserstream::{
    grpc::{
        subscribe_request_filter_accounts_filter::Filter, subscribe_update::UpdateOneof,
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeUpdate, SubscribeUpdateAccount,
    },
    LaserstreamConfig, LaserstreamError,
};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
};

use crate::channels::DlpSyncChannelsInit;
use crate::connect;
use crate::consts::{
    DELEGATION_PROGRAM, DELEGATION_RECORD_SIZE, MAX_PENDING_REQUESTS, MAX_PENDING_UPDATES,
    MAX_RECONNECT_ATTEMPTS, PUBKEY_LEN,
};
use crate::transaction_syncer::DlpTransactionSyncer;
use crate::types::{AccountUpdate, DlpSyncError, Pubkey, Slot};

/// Internal message types for sync requests.
pub(crate) enum SyncRequest {
    /// Subscribe to updates for a delegation record.
    Subscribe {
        /// The delegation record pubkey.
        record: Pubkey,
        /// Channel to send the current slot back to the requester.
        slot_tx: tokio::sync::oneshot::Sender<Slot>,
    },
    /// Unsubscribe from a delegation record.
    Unsubscribe(Pubkey),
}

/// The main DLP synchronization service.
///
/// Manages a connection to Laserstream and handles subscription requests
/// from multiple subscribers. Updates are broadcast via an MPSC channel.
pub struct DlpSyncer {
    /// Set of currently subscribed delegation records.
    subscriptions: HashSet<Pubkey>,
    /// The Laserstream update stream.
    stream: connect::LaserStream,
    /// Receiver for incoming subscription requests.
    requests: Receiver<SyncRequest>,
    /// Sender for broadcasting updates to subscribers.
    updates: Sender<AccountUpdate>,
    /// Current slot number.
    slot: Slot,
    /// Transaction syncer for handling undelegation events.
    transaction_syncer: DlpTransactionSyncer,
}

impl DlpSyncer {
    /// Starts a new DLP synchronization service.
    ///
    /// # Arguments
    ///
    /// * `endpoint` - The Laserstream gRPC endpoint URL.
    /// * `key` - The API key for authentication.
    ///
    /// # Returns
    ///
    /// Returns [`DlpSyncChannelsInit`] containing both request and update channels,
    /// or a [`DlpSyncError`] if the connection fails.
    ///
    /// The service is spawned onto the current tokio runtime and will run
    /// until either the stream disconnects or all channel senders are dropped.
    pub async fn start(endpoint: String, key: String) -> Result<DlpSyncChannelsInit, DlpSyncError> {
        let config = LaserstreamConfig {
            api_key: key,
            endpoint,
            channel_options: Default::default(),
            max_reconnect_attempts: Some(MAX_RECONNECT_ATTEMPTS),
            replay: true,
        };

        let (requests_tx, requests_rx) = mpsc::channel(MAX_PENDING_REQUESTS);
        let (updates_tx, updates_rx) = mpsc::channel(MAX_PENDING_UPDATES);

        let stream = Self::connect(&config).await?;
        let transaction_syncer = DlpTransactionSyncer::connect(config, updates_tx.clone()).await?;

        let syncer = Self {
            subscriptions: HashSet::new(),
            stream,
            requests: requests_rx,
            updates: updates_tx,
            slot: 0,
            transaction_syncer,
        };

        tokio::spawn(syncer.run());

        Ok(crate::channels::DlpSyncChannels {
            requests: requests_tx,
            updates: updates_rx,
        })
    }

    /// Main event loop for the synchronization service.
    ///
    /// Handles both incoming requests from subscribers and updates from the Laserstream.
    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(update) = self.stream.next() => self.handle_update(update),
                Some(request) = self.requests.recv() => self.handle_request(request),
                else => break,
            }
        }

        // Notify all subscribers that the sync has terminated.
        let _ = self.updates.send(AccountUpdate::SyncTerminated).await;
    }

    /// Handles a subscription or unsubscription request.
    fn handle_request(&mut self, request: SyncRequest) {
        match request {
            SyncRequest::Subscribe { record, slot_tx } => {
                self.subscriptions.insert(record);
                let _ = slot_tx.send(self.slot);
            }
            SyncRequest::Unsubscribe(record) => {
                self.subscriptions.remove(&record);
            }
        }
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
                tracing::warn!(%error, "error during stream processing");
                return;
            }
        };

        match update {
            Account(acc) => self.handle_account_update(acc),
            Slot(slot) => self.slot = slot.slot,
            Transaction(txn) => self.transaction_syncer.process(txn),
            _ => {}
        }
    }

    /// Handles an account (delegation record) update.
    fn handle_account_update(&self, acc: SubscribeUpdateAccount) {
        let Some(account) = acc.account else { return };

        if account.pubkey.len() != PUBKEY_LEN {
            return;
        }

        if !self.subscriptions.contains(account.pubkey.as_slice()) {
            return;
        }

        let Ok(record) = Pubkey::try_from(account.pubkey.as_slice()) else {
            return;
        };

        let update = AccountUpdate::Delegated {
            record,
            data: account.data,
            slot: acc.slot,
        };

        if let Err(error) = self.updates.try_send(update) {
            tracing::error!(%error, "failed to send delegation update");
        }
    }

    /// Establishes a connection to the Laserstream and performs health check.
    ///
    /// Subscribes to:
    /// - Account updates for delegation records (by owner and data size)
    /// - Slot updates for tracking confirmed slots
    /// - Transaction filters from DlpTransactionSyncer
    async fn connect(config: &LaserstreamConfig) -> Result<connect::LaserStream, DlpSyncError> {
        let mut accounts = HashMap::new();
        let mut slots = HashMap::new();

        // Subscribe to delegation record accounts
        let account_filter = SubscribeRequestFilterAccounts {
            owner: vec![DELEGATION_PROGRAM.into()],
            filters: vec![SubscribeRequestFilterAccountsFilter {
                filter: Some(Filter::Datasize(DELEGATION_RECORD_SIZE)),
            }],
            ..Default::default()
        };
        accounts.insert("delegations".into(), account_filter);

        // Subscribe to all slot updates
        slots.insert("slots".into(), Default::default());

        // Get transaction filters from transaction syncer
        let transactions = DlpTransactionSyncer::create_filters();

        let request = SubscribeRequest {
            accounts,
            slots,
            transactions,
            ..Default::default()
        };

        connect::connect(config, request).await
    }
}