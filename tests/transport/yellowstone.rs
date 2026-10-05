use std::sync::Arc;

use engine::testkit::TestEngine;
use nucleus::testkit::V42_ID;
use serde_json::json;
use solana_account::{AccountMode, AccountSharedData, ReadableAccount};
use solana_pubkey::Pubkey;
use solana_sdk_ids::sysvar::clock;
use tokio::{
    net::TcpListener,
    sync::{mpsc, Mutex, MutexGuard},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use url::Url;
use yellowstone_grpc_geyser::{
    config::ConfigGrpc,
    file_watcher::FileWatcher,
    grpc::{GrpcService, GrpcServiceResult},
    plugin::message::{
        CommitmentLevel, Message, MessageBlockMeta, MessageEntry, MessageSlot, MessageTransaction,
        MessageTransactionInfo, SlotStatus,
    },
    stream::tokio::BatchStreamUnboundedReceiver,
};
use yellowstone_plugin_proto::prelude::{
    CompiledInstruction, InnerInstruction, InnerInstructions, Message as TransactionMessage,
    Transaction, TransactionStatusMeta,
};

use super::{delegation, within, Account, LOOPBACK, REPLAY_SLOT};

const CHANNEL_CAPACITY: usize = 256;

/// Upstream plugin service; input messages exercise reconstruction/replay, broadcasts do not.
pub struct Server {
    pub endpoint: Url,
    pub service: GrpcServiceResult,
    input: mpsc::UnboundedSender<Message>,
    cancel: CancellationToken,
    tasks: TaskTracker,
    // Upstream removes process-global subscription metrics by peer IP on shutdown.
    // Loopback plugin instances must not race that cleanup.
    _guard: Arc<MutexGuard<'static, ()>>,
}

/// Serializes upstream metric cleanup, which identifies every fixture client by loopback IP.
static PLUGIN: Mutex<()> = Mutex::const_new(());

impl Server {
    /// Starts the real plugin with a valid replayable block at slot one before returning.
    pub async fn new() -> Self {
        Self::start(Arc::new(PLUGIN.lock().await), LOOPBACK).await
    }

    /// Independent IPv4/IPv6 providers; distinct peer IPs avoid upstream metric-label collisions.
    /// The shared guard excludes other plugin fixtures until both have shut down.
    pub async fn pair() -> [Self; 2] {
        let guard = Arc::new(PLUGIN.lock().await);
        [Self::start(guard.clone(), LOOPBACK).await, Self::start(guard, "[::1]:0").await]
    }

    /// Each service owns its tasks; only the process-global metric exclusion is shared.
    async fn start(guard: Arc<MutexGuard<'static, ()>>, address: &str) -> Self {
        let socket = TcpListener::bind(address).await.unwrap();
        let address = socket.local_addr().unwrap();
        drop(socket);
        let config: ConfigGrpc = serde_json::from_value(json!({
            "address": address.to_string(), "replay_stored_slots": 16,
            "channel_capacity": CHANNEL_CAPACITY, "snapshot_client_channel_capacity": CHANNEL_CAPACITY,
            "unary_disabled": true
        }))
        .unwrap();
        let cancel = CancellationToken::new();
        let tasks = TaskTracker::new();
        let (input, rx) = mpsc::unbounded_channel();
        let service = GrpcService::create(
            config,
            false,
            cancel.clone(),
            tasks.clone(),
            Arc::new(FileWatcher::new().unwrap()),
            BatchStreamUnboundedReceiver::new(rx),
        )
        .await
        .unwrap();
        let server = Self {
            endpoint: format!("http://{address}").parse().unwrap(),
            service,
            input,
            cancel,
            tasks,
            _guard: guard,
        };
        server.seed(REPLAY_SLOT, &[]).await;
        server
    }

    /// Seals valid zero-transaction blocks through the upstream reconstruction path.
    pub async fn seed(&self, slot: u64, accounts: &[Account]) {
        let mut confirmed = self.service.broadcast.subscribe(CommitmentLevel::Confirmed);
        self.input.send(Self::slot(slot, SlotStatus::FirstShredReceived)).unwrap();
        self.input.send(Self::slot(slot, SlotStatus::Completed)).unwrap();
        for account in accounts {
            self.input.send(account.geyser(slot)).unwrap();
        }
        self.input
            .send(Message::Entry(Arc::new(MessageEntry {
                slot,
                index: 0,
                num_hashes: 1,
                hash: Default::default(),
                executed_transaction_count: 0,
                starting_transaction_index: 0,
                created_at: Default::default(),
            })))
            .unwrap();
        let mut meta = MessageBlockMeta {
            block_meta: Default::default(),
            created_at: Default::default(),
        };
        meta.block_meta.slot = slot;
        meta.block_meta.entries_count = 1;
        meta.block_meta.parent_slot = slot.saturating_sub(1);
        meta.block_meta.blockhash = Pubkey::new_unique().to_string();
        meta.block_meta.parent_blockhash = Pubkey::new_unique().to_string();
        self.input.send(Message::BlockMeta(Arc::new(meta))).unwrap();
        self.input.send(Self::slot(slot, SlotStatus::Processed)).unwrap();
        self.input.send(Self::slot(slot, SlotStatus::Confirmed)).unwrap();
        within(async {
            loop {
                let batch = confirmed.recv().await.unwrap();
                if batch.iter().any(|message| {
                    matches!(message, Message::Slot(s)
                    if s.slot == slot && s.status == SlotStatus::Confirmed)
                }) {
                    break;
                }
            }
        })
        .await;
    }

    /// Consecutive parent context for the replay block's lifecycle messages.
    fn slot(slot: u64, status: SlotStatus) -> Message {
        Message::Slot(Arc::new(MessageSlot {
            slot,
            parent: Some(slot.saturating_sub(1)),
            status,
            dead_error: None,
            created_at: Default::default(),
        }))
    }

    /// Controlled confirmed delivery, deliberately bypassing reconstruction and replay storage.
    pub fn confirmed(&self, slot: u64, accounts: &[Account]) {
        self.service.broadcast.send(
            CommitmentLevel::Confirmed,
            Arc::new(accounts.iter().map(|account| account.geyser(slot)).collect()),
        );
    }

    /// A terminal protocol violation ends ChainSync's outer stream attempt rather than reconnecting internally.
    pub fn malformed_transaction(&self, slot: u64) {
        self.service.broadcast.send(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Self::transaction(
                slot,
                Transaction::default(),
                Default::default(),
            )]),
        );
    }

    /// Successful outer and CPI undelegations of the same target exercise decoding and deduplication.
    pub fn undelegated(&self, slot: u64, key: Pubkey) {
        use dlp_api::discriminator::DlpDiscriminator;
        let transaction = Transaction {
            message: Some(TransactionMessage {
                account_keys: vec![dlp_api::id().to_bytes().to_vec(), key.to_bytes().to_vec()],
                instructions: vec![CompiledInstruction {
                    program_id_index: 0,
                    accounts: vec![0, 1],
                    data: vec![DlpDiscriminator::Undelegate as u8],
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let meta = TransactionStatusMeta {
            inner_instructions: vec![InnerInstructions {
                index: 0,
                instructions: vec![InnerInstruction {
                    program_id_index: 0,
                    accounts: vec![1],
                    data: vec![DlpDiscriminator::UndelegateWithRollbackAfterTimeout as u8],
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        self.service.broadcast.send(
            CommitmentLevel::Confirmed,
            Arc::new(vec![Self::transaction(slot, transaction, meta)]),
        );
    }

    /// Supplies upstream filter keys as well as the wire transaction, including malformed scenarios.
    fn transaction(slot: u64, transaction: Transaction, meta: TransactionStatusMeta) -> Message {
        let mut transaction = MessageTransactionInfo {
            signature: Default::default(),
            is_vote: false,
            transaction,
            meta,
            index: 0,
            account_keys: Default::default(),
            pre_encoded: Default::default(),
            token_owners_all: Default::default(),
            token_owners_changed: Default::default(),
        };
        transaction.fill_account_keys().unwrap();
        transaction.account_keys.insert(dlp_api::id());
        Message::Transaction(Arc::new(MessageTransaction {
            transaction,
            slot,
            created_at: Default::default(),
        }))
    }

    /// On a ready stream, this materialization follows earlier lifecycle events through the serialized worker.
    /// Sealing the canary also establishes opening readiness via replay; preceding direct broadcasts do not replay.
    pub async fn delegation_barrier(&self, engine: &TestEngine, slot: u64) {
        let key = Pubkey::new_unique();
        let mut updates = engine.accounts().subscribe(key);
        let (account, record) = delegation(key, V42_ID, engine.authority(), slot, &[]);
        self.seed(slot, &[account, record]).await;
        let account = within(updates.recv()).await.unwrap();
        assert_eq!(account.mode(), AccountMode::Delegated);
        assert_eq!(account.slot(), slot);
    }

    /// Cancels and joins all plugin tasks before releasing the instance guard.
    pub async fn close(self) {
        self.cancel.cancel();
        drop((self.input, self.service));
        self.tasks.close();
        within(self.tasks.wait()).await;
    }
}

/// Observed materialization and the last broadcast slot, which may still have an update in flight.
pub struct Delivery {
    pub sent_slot: u64,
    pub account: AccountSharedData,
}

/// Waits for retained-account delivery, pacing retries by Clock while the filter ages.
/// Returns the last sent slot as well as the accepted image: a later retry may still be in flight.
pub async fn wait_for_retained_update(
    engine: &mut TestEngine,
    providers: &[Server],
    image: &Account,
) -> Delivery {
    let mut updates = engine.accounts().subscribe(image.key);
    let clock = Account::new(clock::ID);
    let floor = engine
        .accounts()
        .chain_slot()
        .max(engine.get_account(image.key).unwrap().slot());
    let mut slot = floor;
    within(async {
        loop {
            assert!(
                engine.get_account(image.key).is_some(),
                "covered mirror must remain present"
            );
            slot += 1;
            for provider in providers {
                provider.confirmed(slot, &[clock.clone(), image.clone()]);
            }
            loop {
                tokio::select! {
                    update = updates.recv() => {
                        let update = update.unwrap();
                        if update.slot() > floor && update.data() == image.data {
                            return Delivery { sent_slot: slot, account: update };
                        }
                    }
                    () = engine.advance(1) => {
                        if engine.accounts().chain_slot() >= slot {
                            break;
                        }
                    }
                }
            }
        }
    })
    .await
}
