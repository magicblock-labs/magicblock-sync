use std::{
    collections::{HashMap, HashSet, VecDeque},
    future::Future,
    ops::{Deref, DerefMut},
    pin::Pin,
    time::Duration,
};

use futures::StreamExt;
use helius_laserstream::{
    client,
    grpc::{
        subscribe_request_filter_accounts_filter::Filter,
        subscribe_request_filter_accounts_filter_memcmp::Data as MemcmpData,
        subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
        SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeRequestFilterAccountsFilterMemcmp, SubscribeRequestFilterSlots,
        SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdate,
        SubscribeUpdateAccount, SubscribeUpdateTransaction,
    },
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

/// Discriminator of a delegation record account (`AccountDiscriminator::DelegationRecord`),
/// stored as a little-endian u64 at offset 0. Records carrying appended
/// post-delegation actions exceed the base 96-byte size, so filtering by
/// discriminator rather than datasize is required to observe them.
const DELEGATION_RECORD_DISCRIMINATOR: u64 = 100;

/// Instruction discriminator for undelegate operations (u64 LE).
const UNDELEGATE_DISCRIMINATOR: u64 = 3;

/// Instruction discriminators for delegate operations (u64 LE):
/// `Delegate` and `DelegateWithAnyValidator`, which share one account layout.
const DELEGATE_DISCRIMINATORS: [u64; 2] = [0, 19];

/// Length of an instruction discriminator.
const DISCRIMINATOR_LEN: usize = 8;

/// Index of the delegation record account in undelegate instruction accounts.
const DELEGATION_RECORD_ACCOUNT_INDEX: usize = 6;

/// Index of the delegated account in delegate instruction accounts.
const DELEGATE_DELEGATED_ACCOUNT_INDEX: usize = 1;

/// Index of the delegation record account in delegate instruction accounts.
const DELEGATE_RECORD_ACCOUNT_INDEX: usize = 4;

/// Discriminator of an `UndelegationRequest` account
/// (`AccountDiscriminator::UndelegationRequest`), little-endian u64 at
/// offset 0.
const UNDELEGATION_REQUEST_DISCRIMINATOR: u64 = 104;

/// Minimum size of an `UndelegationRequest` account:
/// discriminator + delegated account + expires-at slot.
const UNDELEGATION_REQUEST_MIN_LEN: usize = 8 + 32 + 8;

/// Maximum pending subscription/unsubscription requests.
const MAX_PENDING_REQUESTS: usize = 256;

/// Maximum pending account/transaction updates.
const MAX_PENDING_UPDATES: usize = 8192;

/// Maximum reconnection attempts to the Laserstream.
const MAX_RECONNECT_ATTEMPTS: u32 = 16;

/// Initial delay between stream re-establishment attempts.
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(1);

/// Maximum delay between stream re-establishment attempts.
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Skew bound for the stream not being slot-ordered: an update older than
/// the latest slot notification may still be in flight. Applied when
/// resuming a dropped stream (resume that many slots behind the last
/// observation) and when publishing firehose watermarks (claim only slots
/// that far behind the last observation).
const RESUME_SAFETY_MARGIN_SLOTS: u64 = 32;

/// Number of slots of recent updates retained for replay on subscribe.
const REPLAY_RETENTION_SLOTS: u64 = 256;

/// Maximum number of updates retained for replay.
const REPLAY_BUFFER_CAPACITY: usize = 4096;

/// Stream type alias for Laserstream updates.
type Laser = Pin<Box<dyn futures::Stream<Item = Result<SubscribeUpdate, LaserstreamError>> + Send>>;

pub struct LaserStream {
    stream: Laser,
    /// Kept for the stream's lifetime; `None` only in tests.
    _handle: Option<StreamHandle>,
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

/// A DLP instruction observed in the transaction stream.
enum DlpInstruction {
    Delegate {
        delegated_account: Pubkey,
        record: Pubkey,
    },
    Undelegate {
        record: Pubkey,
    },
}

/// How updates are delivered to the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryMode {
    /// Only updates for explicitly subscribed records are delivered;
    /// updates are buffered for replay-on-subscribe and dropped (with an
    /// error log) under backpressure.
    Subscribed,
    /// Every record update is delivered, plus in-band
    /// [`AccountUpdate::SlotAdvanced`] watermarks. Delivery is lossless:
    /// sends wait for channel capacity, because a silently dropped update
    /// would let a mirror consumer serve stale state forever.
    Firehose,
}

/// The main DLP synchronization service.
///
/// Manages a connection to Laserstream and handles subscription requests
/// from multiple subscribers. Updates are broadcast via an MPSC channel.
pub struct DlpSyncer {
    /// How updates are delivered to the consumer.
    mode: DeliveryMode,
    /// Set of currently subscribed delegation records.
    subscriptions: HashSet<Pubkey>,
    /// The Laserstream update stream.
    stream: LaserStream,
    /// Connection configuration, kept for stream re-establishment.
    config: LaserstreamConfig,
    /// Receiver for incoming subscription requests.
    requests: Receiver<SyncRequest>,
    /// Whether the request channel has been closed by all requesters.
    requests_closed: bool,
    /// Sender for broadcasting updates to subscribers.
    updates: Sender<AccountUpdate>,
    /// Current slot number.
    slot: Slot,
    /// Highest watermark published to firehose consumers; 0 before the first.
    watermark: Slot,
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
        let (requests_tx, requests_rx) = mpsc::channel(MAX_PENDING_REQUESTS);
        let updates_rx =
            Self::launch(endpoint, key, DeliveryMode::Subscribed, requests_rx, false).await?;

        Ok(crate::channels::DlpSyncChannels {
            requests: requests_tx,
            updates: updates_rx,
        })
    }

    /// Starts the service in firehose mode: the returned channel carries
    /// every delegation-record update on chain (no per-record subscriptions)
    /// interleaved with [`AccountUpdate::SlotAdvanced`] watermarks. Delivery
    /// is lossless — a slow consumer backpressures the stream instead of
    /// dropping updates.
    pub async fn start_firehose(
        endpoint: String,
        key: String,
    ) -> Result<Receiver<AccountUpdate>, DlpSyncError> {
        // The request sender is dropped: firehose mode has no subscriptions.
        let (_requests_tx, requests_rx) = mpsc::channel(MAX_PENDING_REQUESTS);
        Self::launch(endpoint, key, DeliveryMode::Firehose, requests_rx, true).await
    }

    async fn launch(
        endpoint: String,
        key: String,
        mode: DeliveryMode,
        requests: Receiver<SyncRequest>,
        requests_closed: bool,
    ) -> Result<Receiver<AccountUpdate>, DlpSyncError> {
        let config = LaserstreamConfig {
            api_key: key,
            endpoint,
            channel_options: Default::default(),
            max_reconnect_attempts: Some(MAX_RECONNECT_ATTEMPTS),
            replay: true,
        };

        let (updates_tx, updates_rx) = mpsc::channel(MAX_PENDING_UPDATES);

        let stream = Self::connect(config.clone(), None, mode == DeliveryMode::Firehose).await?;

        let syncer = Self {
            mode,
            subscriptions: HashSet::new(),
            stream,
            config,
            requests,
            requests_closed,
            updates: updates_tx,
            slot: 0,
            watermark: 0,
            replay: ReplayBuffer::default(),
        };

        tokio::spawn(syncer.run());

        Ok(updates_rx)
    }

    /// Main event loop for the synchronization service.
    ///
    /// Handles both incoming requests from subscribers and updates from the
    /// Laserstream. When the stream ends it is re-established indefinitely
    /// with backoff; the loop only exits once no subscriber can observe
    /// further updates (the update channel is closed).
    async fn run(mut self) {
        loop {
            if self.updates.is_closed() {
                break;
            }

            tokio::select! {
                update = self.stream.next() => match update {
                    Some(update) => self.handle_update(update).await,
                    None => {
                        if !self.reconnect().await {
                            break;
                        }
                    }
                },
                request = self.requests.recv(), if !self.requests_closed => match request {
                    Some(request) => self.handle_request(request),
                    None => self.requests_closed = true,
                },
            }
        }

        // Notify all subscribers that the sync has terminated.
        let _ = self.updates.send(AccountUpdate::SyncTerminated).await;
    }

    /// Re-establishes the Laserstream after the current stream ends.
    ///
    /// First tries to resume from behind the last observed slot so no record
    /// update is lost. If the server cannot replay that far back, falls back
    /// to a fresh subscription and emits [`AccountUpdate::SyncInterrupted`] so
    /// subscribers know continuity was lost and cached delegation state must
    /// be revalidated. Retries indefinitely with exponential backoff.
    ///
    /// Returns `false` only when the update channel is closed and no
    /// subscriber can observe further updates.
    async fn reconnect(&mut self) -> bool {
        tracing::warn!("laserstream ended; re-establishing");
        let mut delay = RECONNECT_BASE_DELAY;

        loop {
            if self.updates.is_closed() {
                return false;
            }

            // Resume behind the last slot notification: the stream is not
            // slot-ordered, so an update older than that slot may still have
            // been in flight when the stream dropped. Re-delivered updates
            // are covered by the idempotency contract.
            let resume_slot = (self.slot > 0)
                .then(|| self.slot.saturating_sub(RESUME_SAFETY_MARGIN_SLOTS).max(1));
            let connect = Self::connect(
                self.config.clone(),
                resume_slot,
                self.mode == DeliveryMode::Firehose,
            );
            match self.serve_requests_during(connect).await {
                Ok(stream) => {
                    self.stream = stream;
                    // Replay redelivers updates from the resume slot, which
                    // sits at the last published watermark: suspend violation
                    // checks until a fresh slot notification re-establishes
                    // it, so expected duplicates are not misread as skew.
                    // Consumers merge watermarks monotonically, so the brief
                    // regression is invisible to them.
                    self.watermark = 0;
                    tracing::info!(from_slot = ?resume_slot, "laserstream re-established");
                    // Without a slot watermark continuity cannot be proven:
                    // updates delivered before the first slot notification
                    // may have had successors in the disconnected interval.
                    if resume_slot.is_none() {
                        return self.send_interrupted().await;
                    }
                    return true;
                }
                Err(error) => {
                    tracing::warn!(?error, from_slot = ?resume_slot, "resume failed")
                }
            }

            // The server may no longer retain the resume slot; a fresh
            // subscription loses the updates in between, which subscribers
            // must learn about to invalidate cached state.
            if resume_slot.is_some() {
                let connect = Self::connect(
                    self.config.clone(),
                    None,
                    self.mode == DeliveryMode::Firehose,
                );
                match self.serve_requests_during(connect).await {
                    Ok(stream) => {
                        self.stream = stream;
                        self.watermark = 0;
                        tracing::warn!(
                            "laserstream re-established without replay; continuity lost"
                        );
                        return self.send_interrupted().await;
                    }
                    Err(error) => tracing::warn!(?error, "fresh reconnect failed"),
                }
            }

            self.serve_requests_during(time::sleep(delay)).await;
            delay = (delay * 2).min(RECONNECT_MAX_DELAY);
        }
    }

    /// Delivers the continuity-loss notice. The send blocks under
    /// backpressure because dropping it would leave subscribers trusting
    /// stale state. Returns `false` when the update channel is closed and
    /// no subscriber can observe further updates.
    async fn send_interrupted(&mut self) -> bool {
        // Pre-gap replay entries are no longer trustworthy: a record buffered
        // as delegated may have been undelegated during the missed interval,
        // and replaying it on a later subscribe would restore stale state.
        self.replay.clear();
        self.updates
            .send(AccountUpdate::SyncInterrupted)
            .await
            .is_ok()
    }

    /// Awaits `fut` while continuing to service subscription requests.
    /// Subscribe/unsubscribe only touch local state, so they must not stall
    /// (or fill the request channel) while the stream is being re-established.
    async fn serve_requests_during<F: Future>(&mut self, fut: F) -> F::Output {
        tokio::pin!(fut);
        loop {
            tokio::select! {
                out = &mut fut => return out,
                request = self.requests.recv(), if !self.requests_closed => match request {
                    Some(request) => self.handle_request(request),
                    None => self.requests_closed = true,
                },
            }
        }
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
    async fn handle_update(&mut self, result: Result<SubscribeUpdate, LaserstreamError>) {
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
            Account(acc) => self.handle_account_update(acc).await,
            Slot(slot) => {
                self.slot = slot.slot;
                self.replay.prune(self.slot);
                // The stream is not slot-ordered: a slot notification can
                // precede data updates from at-or-before that slot. Publish
                // the watermark behind the observed slot by the same skew
                // margin the resume logic assumes, so `SlotAdvanced(w)` only
                // claims slots whose updates have already drained.
                if self.mode == DeliveryMode::Firehose {
                    let watermark = self.slot.saturating_sub(RESUME_SAFETY_MARGIN_SLOTS);
                    if watermark > self.watermark {
                        self.watermark = watermark;
                        Self::deliver(&self.updates, AccountUpdate::SlotAdvanced(watermark)).await;
                    }
                }
            }
            Transaction(txn) => self.handle_transaction_update(txn).await,
            _ => {}
        }
    }

    /// Delivers an update losslessly: waits for channel capacity instead of
    /// dropping. Only used in firehose mode, where a dropped update would
    /// let the consumer serve stale record state forever.
    async fn deliver(updates: &Sender<AccountUpdate>, update: AccountUpdate) {
        if updates.send(update).await.is_err() {
            // Consumer gone; the run loop exits on the next iteration.
            tracing::warn!("update channel closed; dropping update");
        }
    }

    /// Detects a violation of a published watermark: an update landing at or
    /// before an already-claimed slot means the skew margin was insufficient
    /// for this delivery, so the watermark contract was broken. Restore it by
    /// voiding everything delivered so far -- consumers rebuild from live
    /// updates and fall back to fetching in the meantime.
    async fn check_watermark_violation(
        watermark: Slot,
        updates: &Sender<AccountUpdate>,
        slot: Slot,
    ) {
        if watermark > 0 && slot <= watermark {
            tracing::warn!(
                slot,
                watermark,
                "update arrived at or before the published watermark; voiding delivered state"
            );
            Self::deliver(updates, AccountUpdate::SyncInterrupted).await;
        }
    }

    /// Handles an account (delegation record) update.
    async fn handle_account_update(&mut self, acc: SubscribeUpdateAccount) {
        let Some(account) = acc.account else { return };

        if account.pubkey.len() != PUBKEY_LEN {
            return;
        }

        let Ok(record) = Pubkey::try_from(account.pubkey.as_slice()) else {
            return;
        };

        if self.mode == DeliveryMode::Firehose {
            // Two account filters feed this stream in firehose mode;
            // payloads are told apart by their account discriminator.
            let update = match parse_undelegation_request(&account.data) {
                Some((delegated_account, expires_at_slot)) => {
                    AccountUpdate::UndelegationRequested {
                        delegated_account,
                        expires_at_slot,
                        slot: acc.slot,
                    }
                }
                None => AccountUpdate::Delegated {
                    record,
                    data: account.data,
                    slot: acc.slot,
                },
            };
            Self::deliver(&self.updates, update).await;
            Self::check_watermark_violation(self.watermark, &self.updates, acc.slot).await;
            return;
        }

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
    async fn handle_transaction_update(&mut self, txn: SubscribeUpdateTransaction) {
        let Some(info) = txn.transaction else { return };
        let (Some(transaction), Some(meta)) = (info.transaction, info.meta) else {
            return;
        };
        if meta.err.is_some() {
            return;
        }
        let Some(message) = transaction.message else {
            return;
        };

        // Instruction indexes resolve against the runtime account ordering:
        // static keys, then lookup-table loaded writable, then readonly.
        let accounts: Vec<&Vec<u8>> = message
            .account_keys
            .iter()
            .chain(meta.loaded_writable_addresses.iter())
            .chain(meta.loaded_readonly_addresses.iter())
            .collect();

        let account_at = |ix_accounts: &[u8], index: usize| -> Option<Pubkey> {
            ix_accounts
                .get(index)
                .and_then(|&idx| accounts.get(idx as usize))
                .and_then(|bytes| Pubkey::try_from(bytes.as_slice()).ok())
        };

        let parse = |program_id_index: usize,
                     ix_accounts: &[u8],
                     data: &[u8]|
         -> Option<DlpInstruction> {
            let program_id = *accounts.get(program_id_index)?;
            (program_id == DELEGATION_PROGRAM_PUBKEY).then_some(())?;

            let discriminator = u64::from_le_bytes(data.get(..DISCRIMINATOR_LEN)?.try_into().ok()?);
            match discriminator {
                UNDELEGATE_DISCRIMINATOR => Some(DlpInstruction::Undelegate {
                    record: account_at(ix_accounts, DELEGATION_RECORD_ACCOUNT_INDEX)?,
                }),
                d if DELEGATE_DISCRIMINATORS.contains(&d) => Some(DlpInstruction::Delegate {
                    delegated_account: account_at(ix_accounts, DELEGATE_DELEGATED_ACCOUNT_INDEX)?,
                    record: account_at(ix_accounts, DELEGATE_RECORD_ACCOUNT_INDEX)?,
                }),
                _ => None,
            }
        };

        // Delegation and undelegation are typically CPI-invoked from the
        // owner program, so walk inner instructions as well as top-level.
        let mut observed: Vec<DlpInstruction> = Vec::new();
        for ix in &message.instructions {
            observed.extend(parse(ix.program_id_index as usize, &ix.accounts, &ix.data));
        }
        for inner in &meta.inner_instructions {
            for ix in &inner.instructions {
                observed.extend(parse(ix.program_id_index as usize, &ix.accounts, &ix.data));
            }
        }

        for op in observed {
            match op {
                DlpInstruction::Undelegate { record } => {
                    let update = AccountUpdate::Undelegated {
                        record,
                        slot: txn.slot,
                    };

                    if self.mode == DeliveryMode::Firehose {
                        Self::deliver(&self.updates, update).await;
                    } else {
                        self.replay.push(record, txn.slot, None);
                        if let Err(error) = self.updates.try_send(update) {
                            tracing::error!(
                                %error,
                                "failed to send undelegation update"
                            );
                        }
                        continue;
                    }
                }
                DlpInstruction::Delegate {
                    delegated_account,
                    record,
                } => {
                    // Only firehose consumers do discovery; subscribed-mode
                    // consumers key on record updates alone.
                    if self.mode != DeliveryMode::Firehose {
                        continue;
                    }
                    Self::deliver(
                        &self.updates,
                        AccountUpdate::DelegationObserved {
                            delegated_account,
                            record,
                            slot: txn.slot,
                        },
                    )
                    .await;
                }
            }
            Self::check_watermark_violation(self.watermark, &self.updates, txn.slot).await;
        }
    }

    /// Builds the Laserstream subscription request.
    ///
    /// Subscribes to:
    /// - Account updates for delegation records, matched by owner and the
    ///   record discriminator at offset 0. A datasize filter would miss
    ///   records that carry appended post-delegation actions, while an
    ///   owner-only filter would match every delegated account.
    /// - Transaction updates that touch the delegation program
    /// - Slot updates for tracking confirmed slots
    ///
    /// Updates are requested at confirmed commitment: record state consumed
    /// at processed level could be rolled back with a fork.
    ///
    /// `from_slot` requests server-side replay from that slot to resume
    /// after a disconnect without losing updates.
    ///
    /// Firehose subscriptions additionally stream `UndelegationRequest`
    /// accounts so delegating validators observe undelegation requests in
    /// real time.
    fn subscribe_request(from_slot: Option<Slot>, firehose: bool) -> SubscribeRequest {
        let mut accounts = HashMap::new();
        let mut slots = HashMap::new();
        let mut transactions = HashMap::new();

        let account_filter = SubscribeRequestFilterAccounts {
            owner: vec![DELEGATION_PROGRAM.into()],
            filters: vec![SubscribeRequestFilterAccountsFilter {
                filter: Some(Filter::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                    offset: 0,
                    data: Some(MemcmpData::Bytes(
                        DELEGATION_RECORD_DISCRIMINATOR.to_le_bytes().to_vec(),
                    )),
                })),
            }],
            ..Default::default()
        };
        accounts.insert("delegations".into(), account_filter);

        if firehose {
            let request_filter = SubscribeRequestFilterAccounts {
                owner: vec![DELEGATION_PROGRAM.into()],
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(Filter::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                        offset: 0,
                        data: Some(MemcmpData::Bytes(
                            UNDELEGATION_REQUEST_DISCRIMINATOR.to_le_bytes().to_vec(),
                        )),
                    })),
                }],
                ..Default::default()
            };
            accounts.insert("undelegation-requests".into(), request_filter);
        }

        let tx_filter = SubscribeRequestFilterTransactions {
            account_include: vec![DELEGATION_PROGRAM.into()],
            ..Default::default()
        };
        transactions.insert("undelegations".into(), tx_filter);

        // Only slots at the subscription's commitment level: the slot
        // watermark drives the resume point, so tracking processed slots
        // would resume ahead of confirmed updates that never arrived.
        let slot_filter = SubscribeRequestFilterSlots {
            filter_by_commitment: Some(true),
            ..Default::default()
        };
        slots.insert("slots".into(), slot_filter);

        SubscribeRequest {
            accounts,
            slots,
            transactions,
            commitment: Some(CommitmentLevel::Confirmed as i32),
            from_slot,
            ..Default::default()
        }
    }

    /// Establishes a connection to the Laserstream and performs health check.
    async fn connect(
        config: LaserstreamConfig,
        from_slot: Option<Slot>,
        firehose: bool,
    ) -> Result<LaserStream, DlpSyncError> {
        let request = Self::subscribe_request(from_slot, firehose);
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
        let first = time::timeout(Duration::from_secs(5), stream.next())
            .await
            .map_err(|_| DlpSyncError::Connection("health check timed out"))?
            .ok_or_else(|| DlpSyncError::Connection("stream closed before first update"))?
            .map_err(DlpSyncError::LaserStream)?;

        // The health-check item can be a real update (e.g. the first replayed
        // record on a resume), not just the pong — put it back in front of
        // the stream so it reaches the event loop.
        let stream = Box::pin(futures::stream::once(std::future::ready(Ok(first))).chain(stream));
        let stream = LaserStream {
            stream,
            _handle: Some(_handle),
        };

        Ok(stream)
    }
}

/// Decodes an `UndelegationRequest` account payload:
/// discriminator, delegated account, expires-at slot.
fn parse_undelegation_request(data: &[u8]) -> Option<(Pubkey, Slot)> {
    if data.len() < UNDELEGATION_REQUEST_MIN_LEN
        || data[..DISCRIMINATOR_LEN] != UNDELEGATION_REQUEST_DISCRIMINATOR.to_le_bytes()
    {
        return None;
    }
    let delegated_account = Pubkey::try_from(&data[8..40]).ok()?;
    let expires_at_slot = u64::from_le_bytes(data[40..48].try_into().ok()?);
    Some((delegated_account, expires_at_slot))
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
    /// Drops all buffered updates.
    fn clear(&mut self) {
        self.updates.clear();
    }

    /// Buffers an update, evicting the oldest one when at capacity.
    fn push(&mut self, record: Pubkey, slot: Slot, data: Option<Vec<u8>>) {
        if self.updates.len() == REPLAY_BUFFER_CAPACITY {
            self.updates.pop_front();
        }
        self.updates
            .push_back(BufferedUpdate { record, slot, data });
    }

    /// Drops buffered updates older than the retention window.
    ///
    /// Scans the whole buffer: entries arrive in stream order, which is not
    /// guaranteed to match slot order, so expired entries may sit behind
    /// retained ones.
    fn prune(&mut self, current_slot: Slot) {
        let cutoff = current_slot.saturating_sub(REPLAY_RETENTION_SLOTS);
        self.updates.retain(|u| u.slot >= cutoff);
    }

    /// Returns the buffered updates for a record, oldest slot first.
    ///
    /// Matching entries are sorted by slot rather than replayed in arrival
    /// order, so consumers that don't merge slot-monotonically still end up
    /// with the newest state last.
    fn updates_for(&self, record: &Pubkey) -> impl Iterator<Item = AccountUpdate> + '_ {
        let record = *record;
        let mut matching: Vec<&BufferedUpdate> =
            self.updates.iter().filter(|u| u.record == record).collect();
        matching.sort_by_key(|u| u.slot);
        matching.into_iter().map(move |u| match &u.data {
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
    use helius_laserstream::solana::storage::confirmed_block::CompiledInstruction;

    use super::*;

    const RECORD_A: Pubkey = [1; 32];
    const RECORD_B: Pubkey = [2; 32];

    fn slots(buffer: &ReplayBuffer, record: &Pubkey) -> Vec<Slot> {
        buffer
            .updates_for(record)
            .map(|u| match u {
                AccountUpdate::Delegated { slot, .. } => slot,
                AccountUpdate::Undelegated { slot, .. } => slot,
                AccountUpdate::DelegationObserved { .. }
                | AccountUpdate::UndelegationRequested { .. }
                | AccountUpdate::SlotAdvanced(_)
                | AccountUpdate::SyncInterrupted
                | AccountUpdate::SyncTerminated => unreachable!(),
            })
            .collect()
    }

    fn test_syncer(mode: DeliveryMode) -> (DlpSyncer, Receiver<AccountUpdate>) {
        let (_requests_tx, requests) = mpsc::channel(1);
        let (updates_tx, updates_rx) = mpsc::channel(MAX_PENDING_UPDATES);
        let syncer = DlpSyncer {
            mode,
            subscriptions: HashSet::new(),
            stream: LaserStream {
                stream: Box::pin(futures::stream::pending()),
                _handle: None,
            },
            config: LaserstreamConfig {
                api_key: String::new(),
                endpoint: String::new(),
                channel_options: Default::default(),
                max_reconnect_attempts: Some(1),
                replay: true,
            },
            requests,
            requests_closed: true,
            updates: updates_tx,
            slot: 0,
            watermark: 0,
            replay: ReplayBuffer::default(),
        };
        (syncer, updates_rx)
    }

    fn account_update(record: Pubkey, slot: Slot, data: Vec<u8>) -> SubscribeUpdate {
        use helius_laserstream::grpc::SubscribeUpdateAccountInfo;
        SubscribeUpdate {
            update_oneof: Some(UpdateOneof::Account(SubscribeUpdateAccount {
                account: Some(SubscribeUpdateAccountInfo {
                    pubkey: record.to_vec(),
                    data,
                    ..Default::default()
                }),
                slot,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn slot_update(slot: Slot) -> SubscribeUpdate {
        use helius_laserstream::grpc::SubscribeUpdateSlot;
        SubscribeUpdate {
            update_oneof: Some(UpdateOneof::Slot(SubscribeUpdateSlot {
                slot,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn undelegate_tx_update(record: Pubkey, slot: Slot) -> SubscribeUpdate {
        use helius_laserstream::{
            grpc::{SubscribeUpdateTransaction, SubscribeUpdateTransactionInfo},
            solana::storage::confirmed_block::{Message, Transaction, TransactionStatusMeta},
        };
        let account_keys = vec![
            vec![9u8; PUBKEY_LEN],
            DELEGATION_PROGRAM_PUBKEY.to_vec(),
            record.to_vec(),
        ];
        let instruction = CompiledInstruction {
            program_id_index: 1,
            // The record sits at DELEGATION_RECORD_ACCOUNT_INDEX (6).
            accounts: vec![0, 0, 0, 0, 0, 0, 2],
            data: UNDELEGATE_DISCRIMINATOR.to_le_bytes().to_vec(),
        };
        SubscribeUpdate {
            update_oneof: Some(UpdateOneof::Transaction(SubscribeUpdateTransaction {
                transaction: Some(SubscribeUpdateTransactionInfo {
                    transaction: Some(Transaction {
                        message: Some(Message {
                            account_keys,
                            instructions: vec![instruction],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    meta: Some(TransactionStatusMeta::default()),
                    ..Default::default()
                }),
                slot,
            })),
            ..Default::default()
        }
    }

    /// Same undelegation, but the delegation program and record are loaded
    /// through an address lookup table (present only in the tx meta).
    fn undelegate_tx_update_via_lookup_table(record: Pubkey, slot: Slot) -> SubscribeUpdate {
        use helius_laserstream::{
            grpc::{SubscribeUpdateTransaction, SubscribeUpdateTransactionInfo},
            solana::storage::confirmed_block::{Message, Transaction, TransactionStatusMeta},
        };
        // Static keys hold only the payer; program and record are ALT-loaded:
        // runtime ordering is static, then writable-loaded, then readonly.
        let instruction = CompiledInstruction {
            program_id_index: 2,
            accounts: vec![0, 0, 0, 0, 0, 0, 1],
            data: UNDELEGATE_DISCRIMINATOR.to_le_bytes().to_vec(),
        };
        SubscribeUpdate {
            update_oneof: Some(UpdateOneof::Transaction(SubscribeUpdateTransaction {
                transaction: Some(SubscribeUpdateTransactionInfo {
                    transaction: Some(Transaction {
                        message: Some(Message {
                            account_keys: vec![vec![9u8; PUBKEY_LEN]],
                            instructions: vec![instruction],
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    meta: Some(TransactionStatusMeta {
                        loaded_writable_addresses: vec![record.to_vec()],
                        loaded_readonly_addresses: vec![DELEGATION_PROGRAM_PUBKEY.to_vec()],
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                slot,
            })),
            ..Default::default()
        }
    }

    /// A delegate instruction (top-level or CPI-invoked) with the standard
    /// account layout: payer, delegated account, owner, buffer, record, ...
    fn delegate_tx_update(
        delegated_account: Pubkey,
        record: Pubkey,
        slot: Slot,
        inner: bool,
        discriminator: u64,
    ) -> SubscribeUpdate {
        use helius_laserstream::{
            grpc::{SubscribeUpdateTransaction, SubscribeUpdateTransactionInfo},
            solana::storage::confirmed_block::{
                InnerInstruction, InnerInstructions, Message, Transaction, TransactionStatusMeta,
            },
        };
        let account_keys = vec![
            vec![9u8; PUBKEY_LEN],
            delegated_account.to_vec(),
            vec![7u8; PUBKEY_LEN],
            vec![6u8; PUBKEY_LEN],
            record.to_vec(),
            vec![5u8; PUBKEY_LEN],
            DELEGATION_PROGRAM_PUBKEY.to_vec(),
        ];
        let (program_id_index, ix_accounts, data) = (
            6u32,
            vec![0u8, 1, 2, 3, 4, 5],
            discriminator.to_le_bytes().to_vec(),
        );
        let (instructions, inner_instructions) = if inner {
            (
                vec![],
                vec![InnerInstructions {
                    index: 0,
                    instructions: vec![InnerInstruction {
                        program_id_index,
                        accounts: ix_accounts,
                        data,
                        ..Default::default()
                    }],
                }],
            )
        } else {
            (
                vec![CompiledInstruction {
                    program_id_index,
                    accounts: ix_accounts,
                    data,
                }],
                vec![],
            )
        };
        SubscribeUpdate {
            update_oneof: Some(UpdateOneof::Transaction(SubscribeUpdateTransaction {
                transaction: Some(SubscribeUpdateTransactionInfo {
                    transaction: Some(Transaction {
                        message: Some(Message {
                            account_keys,
                            instructions,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    meta: Some(TransactionStatusMeta {
                        inner_instructions,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                slot,
            })),
            ..Default::default()
        }
    }

    fn undelegation_request_update(
        request_pda: Pubkey,
        delegated_account: Pubkey,
        expires_at_slot: Slot,
        slot: Slot,
    ) -> SubscribeUpdate {
        let mut data = UNDELEGATION_REQUEST_DISCRIMINATOR.to_le_bytes().to_vec();
        data.extend_from_slice(&delegated_account);
        data.extend_from_slice(&expires_at_slot.to_le_bytes());
        account_update(request_pda, slot, data)
    }

    #[tokio::test]
    async fn firehose_observes_delegations_from_top_and_inner_instructions() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        // Top-level Delegate, inner-ix Delegate, inner-ix
        // DelegateWithAnyValidator must all be observed.
        for (inner, discriminator) in [(false, 0), (true, 0), (true, 19)] {
            syncer
                .handle_update(Ok(delegate_tx_update(
                    RECORD_A,
                    RECORD_B,
                    7,
                    inner,
                    discriminator,
                )))
                .await;
            assert!(
                matches!(
                    updates.try_recv(),
                    Ok(AccountUpdate::DelegationObserved {
                        delegated_account,
                        record,
                        slot: 7,
                    }) if delegated_account == RECORD_A && record == RECORD_B
                ),
                "inner={inner} discriminator={discriminator}"
            );
        }

        // Unknown discriminators are not observed.
        syncer
            .handle_update(Ok(delegate_tx_update(RECORD_A, RECORD_B, 7, false, 5)))
            .await;
        assert!(updates.try_recv().is_err());
    }

    #[tokio::test]
    async fn firehose_delivers_undelegation_requests() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        syncer
            .handle_update(Ok(undelegation_request_update(RECORD_B, RECORD_A, 250, 9)))
            .await;

        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::UndelegationRequested {
                delegated_account,
                expires_at_slot: 250,
                slot: 9,
            }) if delegated_account == RECORD_A
        ));
    }

    #[tokio::test]
    async fn subscribed_mode_ignores_delegate_observations() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Subscribed);

        syncer
            .handle_update(Ok(delegate_tx_update(RECORD_A, RECORD_B, 7, true, 0)))
            .await;

        assert!(
            updates.try_recv().is_err(),
            "subscribed consumers key on record updates alone"
        );
    }

    #[tokio::test]
    async fn firehose_delivers_all_records_and_inband_watermarks() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        syncer
            .handle_update(Ok(account_update(RECORD_A, 7, vec![1, 2, 3])))
            .await;
        syncer
            .handle_update(Ok(undelegate_tx_update(RECORD_B, 8)))
            .await;
        syncer.handle_update(Ok(slot_update(100))).await;

        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::Delegated { record, slot: 7, ref data }) if record == RECORD_A && data == &[1, 2, 3]
        ));
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::Undelegated { record, slot: 8 }) if record == RECORD_B
        ));
        // The watermark trails the observed slot by the skew margin: the
        // stream is not slot-ordered, so a bare slot notification cannot
        // prove that updates at or before it have all been delivered.
        let expected = 100 - RESUME_SAFETY_MARGIN_SLOTS;
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::SlotAdvanced(w)) if w == expected
        ));
        assert!(
            syncer.replay.updates.is_empty(),
            "firehose mode must not buffer for replay"
        );
    }

    #[tokio::test]
    async fn firehose_voids_state_when_an_update_lands_behind_the_watermark() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        syncer.handle_update(Ok(slot_update(100))).await;
        let watermark = 100 - RESUME_SAFETY_MARGIN_SLOTS;
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::SlotAdvanced(w)) if w == watermark
        ));

        // A late update at a slot the watermark already claimed breaks the
        // contract: it must be delivered AND all prior state voided.
        syncer
            .handle_update(Ok(account_update(RECORD_A, watermark, vec![1])))
            .await;
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::Delegated { slot, .. }) if slot == watermark
        ));
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::SyncInterrupted)
        ));

        // Updates ahead of the watermark do not trigger a violation.
        syncer
            .handle_update(Ok(account_update(RECORD_A, watermark + 1, vec![2])))
            .await;
        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::Delegated { .. })
        ));
        assert!(updates.try_recv().is_err());
    }

    #[tokio::test]
    async fn firehose_detects_undelegations_behind_lookup_tables() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        syncer
            .handle_update(Ok(undelegate_tx_update_via_lookup_table(RECORD_B, 8)))
            .await;

        assert!(matches!(
            updates.try_recv(),
            Ok(AccountUpdate::Undelegated { record, slot: 8 }) if record == RECORD_B
        ));
    }

    #[tokio::test]
    async fn firehose_emits_no_watermark_within_the_skew_margin() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Firehose);

        syncer.handle_update(Ok(slot_update(9))).await;

        assert!(
            updates.try_recv().is_err(),
            "slots inside the skew margin prove nothing"
        );
        assert_eq!(syncer.slot, 9);
    }

    #[tokio::test]
    async fn subscribed_mode_filters_and_emits_no_watermarks() {
        let (mut syncer, mut updates) = test_syncer(DeliveryMode::Subscribed);

        syncer
            .handle_update(Ok(account_update(RECORD_A, 7, vec![1])))
            .await;
        syncer.handle_update(Ok(slot_update(9))).await;

        assert!(
            updates.try_recv().is_err(),
            "unsubscribed record updates and slot updates must not be delivered"
        );
        assert_eq!(slots(&syncer.replay, &RECORD_A), vec![7]);
        assert_eq!(syncer.slot, 9);
    }

    #[test]
    fn subscribe_request_filters_records_by_discriminator_at_confirmed() {
        let request = DlpSyncer::subscribe_request(Some(42), false);

        assert_eq!(request.commitment, Some(CommitmentLevel::Confirmed as i32));
        assert_eq!(request.from_slot, Some(42));
        assert_eq!(DlpSyncer::subscribe_request(None, false).from_slot, None);
        assert!(
            !request.accounts.contains_key("undelegation-requests"),
            "subscribed mode must not stream undelegation requests"
        );

        let firehose = DlpSyncer::subscribe_request(None, true);
        let requests_filter = &firehose.accounts["undelegation-requests"];
        let [filter] = requests_filter.filters.as_slice() else {
            panic!("expected exactly one undelegation-request filter");
        };
        let Some(Filter::Memcmp(memcmp)) = &filter.filter else {
            panic!("expected a memcmp filter, got {:?}", filter.filter);
        };
        assert_eq!(memcmp.offset, 0);
        assert_eq!(
            memcmp.data,
            Some(MemcmpData::Bytes(104u64.to_le_bytes().to_vec()))
        );

        let accounts = &request.accounts["delegations"];
        assert_eq!(accounts.owner, vec![DELEGATION_PROGRAM.to_string()]);
        let [filter] = accounts.filters.as_slice() else {
            panic!("expected exactly one account filter");
        };
        let Some(Filter::Memcmp(memcmp)) = &filter.filter else {
            panic!("expected a memcmp filter, got {:?}", filter.filter);
        };
        assert_eq!(memcmp.offset, 0);
        assert_eq!(
            memcmp.data,
            Some(MemcmpData::Bytes(100u64.to_le_bytes().to_vec()))
        );

        let transactions = &request.transactions["undelegations"];
        assert_eq!(
            transactions.account_include,
            vec![DELEGATION_PROGRAM.to_string()]
        );
        assert_eq!(
            request.slots["slots"].filter_by_commitment,
            Some(true),
            "slot watermark must track the subscription commitment level"
        );
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
    fn replays_out_of_order_arrivals_sorted_by_slot() {
        let mut buffer = ReplayBuffer::default();
        buffer.push(RECORD_A, 12, Some(vec![]));
        buffer.push(RECORD_A, 10, None);

        assert_eq!(slots(&buffer, &RECORD_A), vec![10, 12]);
    }

    #[test]
    fn prunes_expired_updates_behind_retained_ones() {
        let mut buffer = ReplayBuffer::default();
        buffer.push(RECORD_A, 20, Some(vec![]));
        buffer.push(RECORD_A, 10, Some(vec![]));

        buffer.prune(REPLAY_RETENTION_SLOTS + 15);
        assert_eq!(slots(&buffer, &RECORD_A), vec![20]);
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
