use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::{Deref, DerefMut},
    pin::Pin,
    time::Duration,
};

use futures::StreamExt;
use helius_laserstream::{
    client,
    grpc::{
        subscribe_request_filter_accounts_filter::Filter, subscribe_update::UpdateOneof,
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdate,
        SubscribeUpdateAccount, SubscribeUpdateTransaction,
    },
    solana::storage::confirmed_block::CompiledInstruction,
    LaserstreamConfig, LaserstreamError, StreamHandle,
};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time,
};

use crate::channels::DlpSyncChannelsInit;
use crate::types::{AccountUpdate, DlpSyncError, Pubkey, Slot};

/// Size of a Solana public key in bytes.
const PUBKEY_LEN: usize = 32;

/// Delegation program address.
const DELEGATION_PROGRAM: &str = "DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh";

/// Delegation program pubkey in bytes.
const DELEGATION_PROGRAM_PUBKEY: &Pubkey = &[
    181, 183, 0, 225, 242, 87, 58, 192, 204, 6, 34, 1, 52, 74, 207, 151, 184, 53, 6, 235, 140, 229,
    25, 152, 204, 98, 126, 24, 147, 128, 167, 62,
];

/// Size of a delegation record account in bytes.
const DELEGATION_RECORD_SIZE: u64 = 96;

/// Instruction discriminator for undelegate operations.
const UNDELEGATE_DISCRIMINATOR: u8 = 3;

/// Length of an instruction discriminator (Anchor programs).
const DISCRIMINATOR_LEN: usize = 8;

/// Index of the delegation record account in undelegate instruction accounts.
const DELEGATION_RECORD_ACCOUNT_INDEX: usize = 6;

/// Maximum pending subscription/unsubscription requests.
const MAX_PENDING_REQUESTS: usize = 256;

/// Maximum pending account/transaction updates.
const MAX_PENDING_UPDATES: usize = 8192;

/// Maximum reconnection attempts to the Laserstream.
const MAX_RECONNECT_ATTEMPTS: u32 = 16;

/// Number of slots of recent updates retained for replay on subscribe.
const REPLAY_RETENTION_SLOTS: u64 = 256;

/// Maximum number of updates retained for replay.
const REPLAY_BUFFER_CAPACITY: usize = 4096;

/// Stream type alias for Laserstream updates.
type Laser = Pin<Box<dyn futures::Stream<Item = Result<SubscribeUpdate, LaserstreamError>> + Send>>;

pub struct LaserStream {
    stream: Laser,
    _handle: StreamHandle,
}

impl Deref for LaserStream {
    type Target = Laser;
    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}
impl DerefMut for LaserStream {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

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
    stream: LaserStream,
    /// Receiver for incoming subscription requests.
    requests: Receiver<SyncRequest>,
    /// Sender for broadcasting updates to subscribers.
    updates: Sender<AccountUpdate>,
    /// Current slot number.
    slot: Slot,
    /// Recent updates kept for replay on subscribe.
    replay: ReplayBuffer,
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

        let stream = Self::connect(config).await?;

        let syncer = Self {
            subscriptions: HashSet::new(),
            stream,
            requests: requests_rx,
            updates: updates_tx,
            slot: 0,
            replay: ReplayBuffer::default(),
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
                // Replay buffered updates for this record: an update that landed
                // just before the subscription was registered would otherwise be
                // lost, leaving subscribers with a stale initial fetch forever.
                for update in self.replay.updates_for(&record) {
                    if let Err(error) = self.updates.try_send(update) {
                        tracing::error!(%error, "failed to replay buffered update");
                    }
                }
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
            Slot(slot) => {
                self.slot = slot.slot;
                self.replay.prune(self.slot);
            }
            Transaction(txn) => self.handle_transaction_update(txn),
            _ => {}
        }
    }

    /// Handles an account (delegation record) update.
    fn handle_account_update(&mut self, acc: SubscribeUpdateAccount) {
        let Some(account) = acc.account else { return };

        if account.pubkey.len() != PUBKEY_LEN {
            return;
        }

        let Ok(record) = Pubkey::try_from(account.pubkey.as_slice()) else {
            return;
        };

        // Buffer unconditionally: the subscription for this record may register
        // moments from now, in which case the update is replayed on subscribe.
        self.replay
            .push(record, acc.slot, Some(account.data.clone()));

        if !self.subscriptions.contains(&record) {
            return;
        }

        let update = AccountUpdate::Delegated {
            record,
            data: account.data,
            slot: acc.slot,
        };

        if let Err(error) = self.updates.try_send(update) {
            tracing::error!(%error, "failed to send delegation update");
        }
    }

    /// Handles a transaction update, extracting undelegations.
    fn handle_transaction_update(&mut self, txn: SubscribeUpdateTransaction) {
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

        let records: Vec<Pubkey> = message
            .instructions
            .iter()
            .filter_map(is_undelegate)
            .filter_map(|bytes| Pubkey::try_from(bytes.as_slice()).ok())
            .collect();

        for record in records {
            self.replay.push(record, txn.slot, None);

            let update = AccountUpdate::Undelegated {
                record,
                slot: txn.slot,
            };

            if let Err(error) = self.updates.try_send(update) {
                tracing::error!(%error, "failed to send undelegation update");
            }
        }
    }

    /// Establishes a connection to the Laserstream and performs health check.
    ///
    /// Subscribes to:
    /// - Account updates for delegation records (by owner and data size)
    /// - Transaction updates that touch the delegation program
    /// - Slot updates for tracking confirmed slots
    async fn connect(config: LaserstreamConfig) -> Result<LaserStream, DlpSyncError> {
        let mut accounts = HashMap::new();
        let mut slots = HashMap::new();
        let mut transactions = HashMap::new();

        // Subscribe to delegation record accounts
        let account_filter = SubscribeRequestFilterAccounts {
            owner: vec![DELEGATION_PROGRAM.into()],
            filters: vec![SubscribeRequestFilterAccountsFilter {
                filter: Some(Filter::Datasize(DELEGATION_RECORD_SIZE)),
            }],
            ..Default::default()
        };
        accounts.insert("delegations".into(), account_filter);

        // Subscribe to undelegation transactions
        let tx_filter = SubscribeRequestFilterTransactions {
            account_include: vec![DELEGATION_PROGRAM.into()],
            ..Default::default()
        };
        transactions.insert("undelegations".into(), tx_filter);

        // Subscribe to all slot updates
        slots.insert("slots".into(), Default::default());

        let request = SubscribeRequest {
            accounts,
            slots,
            transactions,
            ..Default::default()
        };

        let (stream, _handle) = client::subscribe(config, request);
        let mut stream = Box::pin(stream);

        // Send ping to establish connection
        _handle
            .write(SubscribeRequest {
                ping: Some(SubscribeRequestPing { id: 0 }),
                ..Default::default()
            })
            .await
            .map_err(DlpSyncError::LaserStream)?;

        // Health check: wait for first update with timeout
        time::timeout(Duration::from_secs(5), stream.next())
            .await
            .map_err(|_| DlpSyncError::Connection("health check timed out"))?
            .ok_or_else(|| DlpSyncError::Connection("stream closed before first update"))?
            .map_err(DlpSyncError::LaserStream)?;
        let stream = LaserStream { stream, _handle };

        Ok(stream)
    }
}

/// A single buffered delegation record update.
struct BufferedUpdate {
    /// The delegation record pubkey.
    record: Pubkey,
    /// The slot at which the update occurred.
    slot: Slot,
    /// The record account data, or `None` for an undelegation.
    data: Option<Vec<u8>>,
}

/// A bounded buffer of recent delegation record updates.
///
/// The Laserstream subscription is global (filtered by owner and data size), so
/// the syncer observes every delegation record update — including ones for
/// records no subscriber has registered yet. Updates are buffered here and
/// replayed on subscribe, closing the window in which an update lands on chain
/// moments before its record's subscription is registered and would otherwise
/// be dropped by the client-side filter.
#[derive(Default)]
struct ReplayBuffer {
    updates: VecDeque<BufferedUpdate>,
}

impl ReplayBuffer {
    /// Buffers an update, evicting the oldest one when at capacity.
    fn push(&mut self, record: Pubkey, slot: Slot, data: Option<Vec<u8>>) {
        if self.updates.len() == REPLAY_BUFFER_CAPACITY {
            self.updates.pop_front();
        }
        self.updates.push_back(BufferedUpdate { record, slot, data });
    }

    /// Drops buffered updates older than the retention window.
    fn prune(&mut self, current_slot: Slot) {
        let cutoff = current_slot.saturating_sub(REPLAY_RETENTION_SLOTS);
        while self.updates.front().is_some_and(|u| u.slot < cutoff) {
            self.updates.pop_front();
        }
    }

    /// Returns the buffered updates for a record, oldest first.
    fn updates_for(&self, record: &Pubkey) -> impl Iterator<Item = AccountUpdate> + '_ {
        let record = *record;
        self.updates
            .iter()
            .filter(move |u| u.record == record)
            .map(move |u| match &u.data {
                Some(data) => AccountUpdate::Delegated {
                    record,
                    data: data.clone(),
                    slot: u.slot,
                },
                None => AccountUpdate::Undelegated {
                    record,
                    slot: u.slot,
                },
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD_A: Pubkey = [1; 32];
    const RECORD_B: Pubkey = [2; 32];

    fn slots(buffer: &ReplayBuffer, record: &Pubkey) -> Vec<Slot> {
        buffer
            .updates_for(record)
            .map(|u| match u {
                AccountUpdate::Delegated { slot, .. } => slot,
                AccountUpdate::Undelegated { slot, .. } => slot,
                AccountUpdate::SyncTerminated => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn replays_only_matching_records_in_order() {
        let mut buffer = ReplayBuffer::default();
        buffer.push(RECORD_A, 10, Some(vec![1]));
        buffer.push(RECORD_B, 11, Some(vec![2]));
        buffer.push(RECORD_A, 12, None);

        assert_eq!(slots(&buffer, &RECORD_A), vec![10, 12]);
        assert_eq!(slots(&buffer, &RECORD_B), vec![11]);
        assert!(slots(&buffer, &[3; 32]).is_empty());
    }

    #[test]
    fn replays_undelegations_as_undelegated() {
        let mut buffer = ReplayBuffer::default();
        buffer.push(RECORD_A, 10, None);

        let updates: Vec<_> = buffer.updates_for(&RECORD_A).collect();
        assert!(matches!(
            updates.as_slice(),
            [AccountUpdate::Undelegated { slot: 10, .. }]
        ));
    }

    #[test]
    fn prunes_updates_outside_retention_window() {
        let mut buffer = ReplayBuffer::default();
        buffer.push(RECORD_A, 10, Some(vec![]));
        buffer.push(RECORD_A, 20, Some(vec![]));

        buffer.prune(REPLAY_RETENTION_SLOTS + 15);
        assert_eq!(slots(&buffer, &RECORD_A), vec![20]);

        buffer.prune(REPLAY_RETENTION_SLOTS + 25);
        assert!(slots(&buffer, &RECORD_A).is_empty());
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut buffer = ReplayBuffer::default();
        for slot in 0..=REPLAY_BUFFER_CAPACITY as u64 {
            buffer.push(RECORD_A, slot, Some(vec![]));
        }

        assert_eq!(buffer.updates.len(), REPLAY_BUFFER_CAPACITY);
        assert_eq!(buffer.updates.front().unwrap().slot, 1);
    }
}
