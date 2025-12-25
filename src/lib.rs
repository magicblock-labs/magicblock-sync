use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    time::Duration,
};

use futures::{Stream, StreamExt};
use helius_laserstream::{
    LaserstreamConfig, LaserstreamError, client,
    grpc::{
        SubscribeRequest, SubscribeRequestFilterAccounts, SubscribeRequestFilterAccountsFilter,
        SubscribeRequestFilterTransactions, SubscribeRequestPing, SubscribeUpdate,
        SubscribeUpdateAccount, SubscribeUpdateTransaction,
        subscribe_request_filter_accounts_filter::Filter, subscribe_update::UpdateOneof,
    },
    solana::storage::confirmed_block::CompiledInstruction,
};
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender},
        oneshot,
    },
    time,
};

type LaserResult = Result<SubscribeUpdate, LaserstreamError>;
type LaserStream = Pin<Box<dyn Stream<Item = LaserResult> + Send>>;
type Slot = u64;

type Pubkey = [u8; PUBKEY_LEN];

const PUBKEY_LEN: usize = 32;

const DELEGATION_PROGRAM_STR: &str = "DELeGGvXpWV2fqJUhqcF5ZSYMS4JTLjteaAMARRSaeSh";
const DELEGATION_PROGRAM_PUBKEY: &Pubkey = &[
    31, 53, 226, 191, 87, 11, 233, 198, 246, 12, 164, 36, 215, 66, 119, 191, 169, 92, 87, 72, 207,
    12, 78, 46, 180, 166, 193, 92, 63, 34, 74, 41,
];
const DELEGATION_RECORD_SIZE: u64 = 96;
const UNDELEGATE_IX_DISCRIMINATOR: u8 = 3;
const DISCRIMINATOR_LEN: usize = 8;
const DELEGATION_RECORD_IX_INDEX: usize = 6;

const MAX_PENDING_REQUESTS: usize = 256;
const MAX_PENDING_UPDATES: usize = 8192;
const MAX_RECONNECT_ATTEMPTS: u32 = 16;

pub struct DlpSyncChannels<R> {
    requests: Sender<SyncRequest>,
    updates: R,
}

pub type DlpSyncChannelsInit = DlpSyncChannels<Receiver<AccountUpdate>>;
pub type DlpSyncChannelsRequester = DlpSyncChannels<()>;

#[derive(Debug)]
pub enum DlpSyncError {
    Connection(&'static str),
    LaserStream(LaserstreamError),
}

enum SyncRequest {
    Subscribe {
        record: Pubkey,
        slot_tx: oneshot::Sender<Slot>,
    },
    Unsubscribe(Pubkey),
}

pub enum AccountUpdate {
    Delegated {
        record: Pubkey,
        data: Vec<u8>,
        slot: Slot,
    },
    Undelegated {
        record: Pubkey,
        slot: Slot,
    },
    SyncTerminated,
}

pub struct DlpSyncer {
    subscriptions: HashSet<Pubkey>,
    stream: LaserStream,
    requests: Receiver<SyncRequest>,
    updates: Sender<AccountUpdate>,
    slot: Slot,
}

impl DlpSyncer {
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
        let channels = DlpSyncChannels {
            requests: requests_tx,
            updates: updates_rx,
        };
        let stream = Self::connect(config.clone()).await?;
        let this = Self {
            subscriptions: Default::default(),
            stream,
            requests: requests_rx,
            updates: updates_tx,
            slot: 0,
        };
        tokio::spawn(this.run());
        Ok(channels)
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                Some(update) = self.stream.next() => self.handle_update(update),
                Some(request) = self.requests.recv() => self.handle_request(request),
                else => {
                    break
                }
            }
        }
        let _ = self.updates.send(AccountUpdate::SyncTerminated).await;
    }

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

    fn handle_update(&mut self, update: LaserResult) {
        use UpdateOneof::*;

        let update = match update {
            Ok(u) => {
                let Some(update) = u.update_oneof else {
                    return;
                };
                update
            }
            Err(error) => {
                tracing::warn!(%error, "encountered an error during stream processing");
                return;
            }
        };

        match update {
            Account(acc) => self.handle_account_update(acc),
            Slot(slot) => self.slot = slot.slot,
            Transaction(txn) => self.handle_transaction_update(txn),
            _ => (),
        }
    }

    fn handle_account_update(&self, acc: SubscribeUpdateAccount) {
        let slot = acc.slot;
        let Some(acc) = acc.account else { return };
        if !self.subscriptions.contains(acc.pubkey.as_slice()) {
            return;
        }
        if acc.pubkey.len() != PUBKEY_LEN {
            return;
        }
        let Ok(record) = Pubkey::try_from(acc.pubkey.as_slice()) else {
            return;
        };
        let data = acc.data;
        let update = AccountUpdate::Delegated { record, data, slot };
        if let Err(error) = self.updates.try_send(update) {
            tracing::error!(%error, "failed to send delegation update");
        }
    }

    fn handle_transaction_update(&self, txn: SubscribeUpdateTransaction) {
        let Some(message) = txn
            .transaction
            .and_then(|t| t.transaction.zip(t.meta))
            .and_then(|(t, m)| m.err.is_none().then(|| t.message))
            .flatten()
        else {
            return;
        };
        let accounts = message.account_keys;
        let filter = |ix: &CompiledInstruction| {
            let program = accounts.get(ix.program_id_index as usize)?;
            (program == DELEGATION_PROGRAM_PUBKEY).then_some(())?;
            let (tag, _) = ix.data.split_at_checked(DISCRIMINATOR_LEN)?;

            (tag[0] == UNDELEGATE_IX_DISCRIMINATOR).then_some(())?;
            ix.accounts
                .get(DELEGATION_RECORD_IX_INDEX)
                .and_then(|&i| accounts.get(i as usize))
        };
        for record in message.instructions.iter().filter_map(filter) {
            let Ok(record) = Pubkey::try_from(record.as_slice()) else {
                continue;
            };
            let slot = txn.slot;
            let update = AccountUpdate::Undelegated { record, slot };
            if let Err(error) = self.updates.try_send(update) {
                tracing::error!(%error, "failed to send undelegation update");
            }
        }
    }

    async fn connect(config: LaserstreamConfig) -> Result<LaserStream, DlpSyncError> {
        let mut accounts = HashMap::new();
        let mut slots = HashMap::new();
        let mut transactions = HashMap::new();

        let filter = SubscribeRequestFilterAccounts {
            owner: vec![DELEGATION_PROGRAM_STR.into()],
            filters: vec![SubscribeRequestFilterAccountsFilter {
                filter: Some(Filter::Datasize(DELEGATION_RECORD_SIZE)),
            }],
            ..Default::default()
        };
        accounts.insert("delegations".into(), filter);
        let filter = SubscribeRequestFilterTransactions {
            account_include: vec![DELEGATION_PROGRAM_STR.into()],

            ..Default::default()
        };
        transactions.insert("undelegations".into(), filter);
        slots.insert("slots".into(), Default::default());

        let request = SubscribeRequest {
            accounts,
            slots,
            transactions,
            ..Default::default()
        };
        let (stream, handle) = client::subscribe(config, request);
        let ping = SubscribeRequest {
            ping: Some(SubscribeRequestPing { id: 0 }),
            ..Default::default()
        };
        let mut stream = Box::pin(stream);

        handle
            .write(ping)
            .await
            .map_err(DlpSyncError::LaserStream)?;

        let update = time::timeout(Duration::from_secs(5), stream.next()).await;
        update
            .map_err(|_| DlpSyncError::Connection("Failed to healthcheck laser stream, timed out"))?
            .ok_or_else(|| {
                DlpSyncError::Connection("Laser stream connection failed to be established")
            })?
            .map_err(DlpSyncError::LaserStream)?;

        Ok(stream)
    }
}

impl DlpSyncChannelsRequester {
    pub async fn subscribe(&self, record: Pubkey) -> Option<Slot> {
        let (slot_tx, rx) = oneshot::channel();
        let request = SyncRequest::Subscribe { record, slot_tx };
        self.requests.send(request).await.ok()?;
        rx.await.ok()
    }

    pub async fn unsubscribe(&self, record: Pubkey) -> Option<()> {
        let request = SyncRequest::Unsubscribe(record);
        self.requests.send(request).await.ok()
    }
}

impl DlpSyncChannelsInit {
    pub fn split(self) -> (DlpSyncChannelsRequester, Receiver<AccountUpdate>) {
        let updates = self.updates;
        let requester = DlpSyncChannelsRequester {
            updates: (),
            requests: self.requests,
        };
        (requester, updates)
    }
}
