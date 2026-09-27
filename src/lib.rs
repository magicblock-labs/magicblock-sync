//! Synchronizes base-chain accounts into Engine.
//!
//! [`ChainSync`] subscribes before fetching missing read-only accounts and payers.
//! Writable accounts are fetched with their delegation records but not subscribed over
//! WebSocket. Acknowledged WebSocket subscriptions gain one load-balanced gRPC copy
//! after 30 minutes. A gRPC-only account keeps trying to restore WebSocket coverage.
//! Ordinary accounts enter Engine in `Uninit` mode; executable programs enter as
//! read-only ELF accounts. A background worker applies WebSocket and gRPC events.
//! A later base-chain update can recreate an undelegated account.

/// Missing-account fetch planning and initial materialization.
mod acquisition;
/// Delegation record parsing and account conversion.
mod delegation;
/// Yellowstone subscriptions and lifecycle events.
mod grpc;
/// Confirmed HTTP account snapshots.
mod http;
/// Executable and ProgramData normalization.
mod program;
/// Shared RPC wire types and account decoding.
mod rpc;
/// Confirmed WebSocket subscription pool.
mod websocket;
/// Stream application and delayed gRPC coverage.
mod worker;

use std::{borrow::Borrow, sync::Arc, time::Duration};

use engine::Engine;
use nucleus::shutdown::{Service, ShutdownManager};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use url::Url;

use crate::http::Fetcher;
use crate::websocket::Pool;

/// WS acknowledgement age and cadence for gRPC filter and WS restoration scans.
const DUPLICATION_DELAY: Duration = Duration::from_secs(30 * 60);

pub use grpc::{Config as GrpcConfig, Error as GrpcError, StreamConfig as GrpcStreamConfig};
pub use http::Error as HttpError;
pub use rpc::{DecodeError, Error as RpcError};
pub use websocket::{
    Config as WebSocketConfig, Error as WebSocketError, Provider as WebSocketProvider,
};

/// Provider configuration for HTTP snapshots and live WebSocket/gRPC updates.
pub struct ChainSyncConfig {
    /// HTTP snapshot providers.
    pub http: Vec<Url>,
    /// WebSocket subscription providers.
    pub websocket: WebSocketConfig,
    /// Yellowstone update streams and their shared delegation authority.
    pub grpc: GrpcConfig,
}

/// Selects subscription and companion-fetch behavior in [`ChainSync::sync`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountProperty {
    /// Fee payer; subscribed over WebSocket during the initial fetch.
    Payer,
    /// Writable transaction account; fetched without a WebSocket subscription.
    Writable,
    /// Read-only transaction account.
    Readonly,
    /// Executable program; its ProgramData companion is fetched and subscribed over WebSocket.
    Program,
}

/// One primary account requested for synchronization. ProgramData and delegation
/// record companions are derived from its property.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncAccount {
    /// Primary account address to fetch and materialize.
    pub pubkey: Pubkey,
    /// Role used to select subscription and companion-fetch behavior.
    pub property: AccountProperty,
}

/// Acquires missing base-chain accounts for Engine and applies live provider updates.
pub struct ChainSync {
    /// Acquires missing-account leases and materializes their snapshots.
    engine: Engine,
    /// Supplies confirmed snapshots for missing accounts.
    fetcher: Fetcher,
    /// Tracks accounts before their snapshots are fetched.
    websocket: Pool,
    /// Keeps all gRPC streams alive for the synchronizer's lifetime.
    grpc: Vec<grpc::Client>,
}

/// Failure to acquire or apply base-chain account state.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP account fetch failed: {0}")]
    Fetch(#[from] HttpError),
    #[error("WebSocket subscription failed: {0}")]
    Subscribe(#[from] WebSocketError),
    #[error("gRPC operation failed: {0}")]
    Grpc(#[from] GrpcError),
    #[error("at least one gRPC stream is required")]
    NoGrpcStreams,
    #[error("Engine account operation failed: {0}")]
    Engine(#[from] engine::EngineError),
    #[error("invalid program: {0}")]
    Program(&'static str),
    #[error("invalid ProgramData: {0}")]
    ProgramData(#[from] Box<bincode::ErrorKind>),
    #[error("invalid delegation record: {0}")]
    Record(&'static str),
    #[error("invalid delegation actions: {0}")]
    Actions(#[from] borsh::io::Error),
    #[error("delegation action decryption failed: {0}")]
    Decrypt(#[from] dlp_api::decrypt::DecryptError),
}

impl ChainSync {
    /// Sets up HTTP, WebSocket, and gRPC providers and starts applying updates.
    /// The worker is registered with Engine's coordinated shutdown manager.
    pub fn new(
        engine: Engine,
        config: ChainSyncConfig,
        shutdown: &mut ShutdownManager,
    ) -> Result<Arc<Self>, Error> {
        if config.grpc.streams.is_empty() {
            return Err(Error::NoGrpcStreams);
        }
        let (websocket, websocket_rx) = Pool::new(config.websocket);
        let slot = websocket.slot();
        let fetcher = Fetcher::new(config.http, Arc::clone(&slot))?;
        let (events, grpc_rx) = mpsc::channel(grpc::EVENT_CAPACITY);
        let authority = config.grpc.authority;
        let grpc = config
            .grpc
            .streams
            .into_iter()
            .enumerate()
            .map(|(id, stream)| {
                grpc::Client::new(id, stream, authority, Arc::clone(&slot), events.clone())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sync = Arc::new(Self { engine, fetcher, websocket, grpc });
        let shutdown = shutdown.handle(Service::ChainSync);
        tokio::spawn(Self::run(
            Arc::downgrade(&sync),
            websocket_rx,
            grpc_rx,
            shutdown,
        ));
        Ok(sync)
    }

    /// Fetches and materializes requested accounts that are missing from Engine.
    ///
    /// Read-only accounts, programs, and payers are subscribed over WebSocket
    /// before fetching. Writable accounts are fetched without WebSocket subscriptions.
    /// Programs include ProgramData; writable accounts and payers include their
    /// derived delegation record. Records are fetched only. A payer's WebSocket
    /// subscription is removed when its initial snapshot resolves as delegated here.
    /// Acknowledged WebSocket subscriptions gain one gRPC copy after 30 minutes.
    /// A gRPC-only subscription periodically retries WebSocket restoration.
    ///
    /// An account is resolved as delegated only when its primary and delegation-record
    /// snapshots are DLP-owned and the record names this Engine. Other snapshots retain
    /// their fetched owner and mode. HTTP `null` becomes a default account.
    ///
    /// Writable and program pubkeys must be unique; repeated payer and read-only
    /// requests are collapsed. Requested accounts must not overlap a program's
    /// derived ProgramData address.
    pub async fn sync<I>(&self, requests: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: Borrow<SyncAccount>,
    {
        let mut accounts: Vec<_> = requests.into_iter().map(|account| *account.borrow()).collect();
        accounts.sort_unstable_by_key(|account| account.pubkey);
        accounts.dedup_by_key(|account| account.pubkey);

        // Bound each acquisition wave so its primary keys and companions fit one RPC batch.
        let mut start = 0;
        while start < accounts.len() {
            let mut end = start;
            let mut size = 0;
            while let Some(account) = accounts.get(end) {
                let added = 1 + usize::from(account.property != AccountProperty::Readonly);
                if size + added > 100 {
                    break;
                }
                size += added;
                end += 1;
            }
            let batch = &accounts[start..end];
            start = end;
            self.sync_batch(batch).await?;
        }
        Ok(())
    }
}

/// Account subscription and optional target for ProgramData account updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccountSubscription {
    /// Address observed by the transport.
    pubkey: Pubkey,
    /// Program to update when `pubkey` is its Loader V3 ProgramData account.
    target: Option<Pubkey>,
}
