#![allow(dead_code)]

pub mod http;
pub mod magic;
pub mod websocket;
pub mod yellowstone;

use std::{future::Future, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine};
use dlp_api::{pda::delegation_record_pda_from_delegated_account, state::DelegationRecord};
use engine::testkit::TestEngine;
use serde_json::{json, Value};
use solana_account::{testkit::delegated_account, AccountBuilder, StateFlags};
use solana_instruction::Instruction;
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey;
use spl_associated_token_account_interface::address::get_associated_token_address_with_program_id;
use spl_token_2022_interface::state::{Account as TokenAccount, AccountState};
use tokio::time;
use url::Url;
use yellowstone_grpc_geyser::plugin::message::{Message, MessageAccount, MessageAccountInfo};

use magicblock_chainsync::{
    ChainSync, ChainSyncAccount, ChainSyncConfig, Error, GrpcStreamConfig, Result, WebSocketConfig,
    WebSocketError, WebSocketProvider,
};

/// Bounds a scenario's event-driven handshakes, including cleanup.
pub const DEADLINE: Duration = Duration::from_secs(30);

pub const LOOPBACK: &str = "127.0.0.1:0";
/// Policy-only tests construct clients without starting a transport.
pub const UNAVAILABLE: &str = "http://127.0.0.1:1";
pub const RPC_VERSION: &str = "2.0";
pub const SERVICE_ERROR: &str = "unavailable";
/// Short real-time rebuild interval for plugin-backed coverage scenarios.
pub const DUPLICATION_DELAY: Duration = Duration::from_millis(5);
/// Initial plugin history and opening checkpoint ensure the two-slot replay lookback is available.
pub const REPLAY_SLOT: u64 = 1;
pub const START_SLOT: u64 = REPLAY_SLOT + 2;
pub const READY_SLOT: u64 = START_SLOT + 1;
pub const SNAPSHOT_SLOT: u64 = READY_SLOT + 1;
pub const DELEGATION_SLOT: u64 = 10;
pub const PROJECTION_SLOT: u64 = 15;
pub const RPC_BATCH_LIMIT: usize = 100;
pub const FIRST_SUBSCRIPTION: u64 = 1;
pub const ACCOUNT_DATA: &[u8] = &[1];
pub const V42_INITIAL: i64 = 1;
pub const ACTION_AMOUNT: i64 = 42;
pub const TOKEN_BALANCE: u64 = 9;
pub const DELEGATED_TOKEN_BALANCE: u64 = 42;
pub const TOKEN_LAMPORTS: u64 = 1000;
pub const NATIVE_RESERVE: u64 = 500;

/// Ordinary ChainSync configuration for controlled endpoints; scenarios set replay, tracking delay, and AML.
pub fn config(http: &Url, ws: Option<&websocket::Server>, grpc: &[&Url]) -> ChainSyncConfig {
    ChainSyncConfig {
        aml: None,
        http: vec![http.clone()],
        websocket: WebSocketConfig {
            providers: ws
                .into_iter()
                .map(|server| WebSocketProvider {
                    url: server.endpoint.clone(),
                    max_connections: 1,
                    subs_per_connection: 128,
                })
                .collect(),
        },
        grpc: grpc
            .iter()
            .map(|endpoint| GrpcStreamConfig {
                endpoint: (*endpoint).clone(),
                token: None,
                duplication_delay: Duration::from_secs(30 * 60),
            })
            .collect(),
    }
}

/// One provider-independent account image for snapshots, notifications, and Geyser messages.
#[derive(Clone)]
pub struct Account {
    pub key: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    pub data: Vec<u8>,
    pub executable: bool,
}

impl Account {
    /// Non-executable, rent-funded image with one byte of recognizable data.
    pub fn new(key: Pubkey) -> Self {
        Self {
            key,
            owner: Pubkey::default(),
            lamports: 1_000_000,
            data: ACCOUNT_DATA.to_vec(),
            executable: false,
        }
    }

    /// Shares an Engine testkit image across the HTTP, WebSocket, and plugin encoders.
    pub fn from_builder(key: Pubkey, image: &AccountBuilder) -> Self {
        let image = image.read();
        Self {
            key,
            owner: image.owner(),
            lamports: image.lamports(),
            data: image.data().to_vec(),
            executable: image.flags().contains(StateFlags::EXECUTABLE),
        }
    }

    /// Confirmed RPC account encoding shared by HTTP snapshots and WebSocket notifications.
    pub fn rpc(&self) -> Value {
        let compressed = zstd::stream::encode_all(self.data.as_slice(), 0).unwrap();
        json!({"owner": self.owner.to_string(), "lamports": self.lamports,
            "executable": self.executable, "data": [STANDARD.encode(compressed), "base64+zstd"]})
    }

    /// Plugin input image with a transaction signature, eligible for DLP account filters.
    pub fn geyser(&self, slot: u64) -> Message {
        Message::Account(Arc::new(MessageAccount {
            account: MessageAccountInfo {
                pubkey: self.key,
                owner: self.owner,
                lamports: self.lamports,
                data: self.data.clone().into(),
                executable: self.executable,
                rent_epoch: 0,
                write_version: slot,
                txn_signature: Some(Default::default()),
                pre_encoded: Default::default(),
            },
            slot,
            is_startup: false,
            created_at: Default::default(),
        }))
    }
}

/// Fails a stalled handshake instead of relying on sleeps or yield counts.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    time::timeout(DEADLINE, future)
        .await
        .expect("scenario exceeded handshake deadline")
}

/// Runs both sides of a scripted exchange to completion under one deadline, even when the operation fails.
pub async fn scripted<T>(
    operation: impl Future<Output = T>,
    script: impl Future<Output = ()>,
) -> T {
    within(async { tokio::join!(operation, script).0 }).await
}

/// Application and canonical record images shared by all provider encoders.
pub fn delegation(
    key: Pubkey,
    owner: Pubkey,
    authority: Pubkey,
    slot: u64,
    actions: &[Instruction],
) -> (Account, Account) {
    use dlp_api::compact::ClearText;
    let mut account = Account::new(key);
    account.owner = dlp_api::id();
    account.data = V42_INITIAL.to_le_bytes().to_vec();
    let metadata = DelegationRecord {
        owner,
        authority,
        delegation_slot: slot,
        lamports: account.lamports,
        commit_frequency_ms: 1,
    };
    let mut record = Account::new(delegation_record_pda_from_delegated_account(&key));
    record.owner = dlp_api::id();
    record.data = encode_record(&metadata);
    if !actions.is_empty() {
        record.data.extend(borsh::to_vec(&actions.to_vec().cleartext()).unwrap());
    }
    (account, record)
}

/// Canonical record header shared by ordinary and eATA delegation fixtures.
fn encode_record(metadata: &DelegationRecord) -> Vec<u8> {
    let mut data = vec![0; DelegationRecord::size_with_discriminator()];
    metadata.to_bytes_with_discriminator(&mut data).unwrap();
    data
}

/// Canonical, DLP-owned request image accepted by the confirmed account filter.
pub fn undelegation_request(key: Pubkey) -> Account {
    use dlp_api::{
        pda::undelegation_request_pda_from_delegated_account, state::UndelegationRequest,
    };
    let request = UndelegationRequest {
        delegated_account: key,
        expires_at_slot: 1000,
    };
    let mut account = Account::new(undelegation_request_pda_from_delegated_account(&key));
    account.owner = dlp_api::id();
    account.data.resize(UndelegationRequest::size_with_discriminator(), 0);
    request.to_bytes_with_discriminator(&mut account.data).unwrap();
    account
}

/// Canonical token account and the eATA delegation that projects onto it.
pub struct Ata {
    pub ata: Pubkey,
    /// Ordinary token image, including extensions and native reserve when requested.
    pub base: AccountBuilder,
    pub eata: Pubkey,
    /// Restored eATA image; provider encoders use DLP ownership instead.
    pub delegated: AccountBuilder,
    pub record: Vec<u8>,
}

impl Ata {
    /// Builds matching canonical identities, balances, and authority metadata.
    pub fn new(program: Pubkey, native: bool, authority: Pubkey, slot: u64) -> Self {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let eata_program: Pubkey = "SPLxh1LVZzEkX99H6rqYizhytLWPZVV296zyYDPagv2".parse().unwrap();
        let (eata, bump) =
            Pubkey::find_program_address(&[owner.as_ref(), mint.as_ref()], &eata_program);
        let ata = get_associated_token_address_with_program_id(&owner, &mint, &program);
        let token = TokenAccount {
            owner,
            mint,
            amount: TOKEN_BALANCE,
            state: AccountState::Initialized,
            is_native: if native { COption::Some(NATIVE_RESERVE) } else { COption::None },
            ..Default::default()
        };
        let mut data = vec![0; TokenAccount::LEN];
        token.pack_into_slice(&mut data);
        if program == spl_token_2022_interface::id() {
            // Account type followed by an ImmutableOwner TLV (tag 7, zero-length payload).
            data.extend([2, 7, 0, 0, 0]);
        }
        let base = AccountBuilder::default().owner(program).lamports(TOKEN_LAMPORTS).data(data);
        let mut data = owner.to_bytes().to_vec();
        data.extend(mint.to_bytes());
        data.extend(DELEGATED_TOKEN_BALANCE.to_le_bytes());
        data.extend([bump, 0, 0, 0, 0, 0, 0, 0]);
        let delegated = delegated_account(TOKEN_LAMPORTS, data, eata_program).slot(slot);
        let metadata = DelegationRecord {
            owner: eata_program,
            authority,
            delegation_slot: slot,
            lamports: TOKEN_LAMPORTS,
            commit_frequency_ms: 1,
        };
        let record = encode_record(&metadata);
        Self {
            ata,
            base,
            eata,
            delegated,
            record,
        }
    }

    /// DLP-owned application and canonical record images for HTTP or the plugin.
    pub fn delegation(&self) -> [Account; 2] {
        let mut account = Account::from_builder(self.eata, &self.delegated);
        account.owner = dlp_api::id();
        let record = Account {
            owner: dlp_api::id(),
            data: self.record.clone(),
            ..Account::new(delegation_record_pda_from_delegated_account(&self.eata))
        };
        [account, record]
    }
}

/// Opens from the fixture's replayable slot one, with HTTP freshness starting at slot three.
pub fn start_chain_sync(
    engine: &mut TestEngine,
    http: &http::Server,
    ws: Option<&websocket::Server>,
    grpc: &yellowstone::Server,
) -> Arc<ChainSync> {
    engine.accounts().advance_chain_slot(START_SLOT);
    let config = config(&http.endpoint, ws, &[&grpc.endpoint]);
    let handle = engine.clone();
    ChainSync::new(handle, config, engine.shutdown()).unwrap()
}

/// Initial connection admission is retried through the public API, not timing-based readiness guesses.
pub async fn acquire(chain_sync: &ChainSync, accounts: &[ChainSyncAccount]) -> Result<()> {
    loop {
        match chain_sync.sync(accounts).await {
            Err(Error::Subscribe(WebSocketError::Unavailable)) => continue,
            result => return result,
        }
    }
}
