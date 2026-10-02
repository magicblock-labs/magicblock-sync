//! Synchronizes base-chain accounts into Engine.
//!
//! [`ChainSync`] subscribes before fetching missing read-only accounts and payers.
//! Writable accounts are fetched with their delegation records but not subscribed over
//! WebSocket. Acknowledged WebSocket subscriptions gain one load-balanced gRPC copy
//! after 30 minutes of gRPC tracking. An account can remain covered by gRPC alone
//! if its WebSocket subscription is lost.
//! Ordinary accounts enter Engine in `Uninit` mode; executable programs enter as
//! read-only ELF accounts. A background worker applies WebSocket and gRPC events.
//! A later base-chain update can recreate an undelegated account.
//! Engine's persisted chain slot tracks confirmed gRPC observations, not
//! completed materializations. New gRPC sessions replay from two slots behind it;
//! Yellowstone owns recovery within a session.

/// Missing-account fetch planning and initial materialization.
mod acquisition;
/// Post-delegation signer assessment, independent of activation authority.
mod aml;
/// Canonical ATA detection and eATA-backed account projection.
mod ata;
/// Delegation record parsing and account conversion.
mod delegation;
/// Yellowstone subscriptions and lifecycle events.
mod grpc;
/// Confirmed HTTP account snapshots.
mod http;
/// Private process-wide operation and transport instrumentation.
mod metrics;
/// Executable and ProgramData normalization.
mod program;
/// Shared RPC wire types and account decoding.
mod rpc;
/// Confirmed WebSocket subscription pool.
mod websocket;
/// Applies transport updates and adds delayed gRPC subscriptions.
mod worker;

use std::{borrow::Borrow, sync::Arc, time::Duration};

use engine::{Engine, EngineError};
use nucleus::shutdown::{Service, ShutdownManager};
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use url::Url;

use crate::http::Fetcher;
use crate::websocket::Pool;

/// Delay before adding a WebSocket subscription to gRPC; also the filter rebuild interval.
const DUPLICATION_DELAY: Duration = Duration::from_secs(30 * 60);

pub use aml::{Config as AmlConfig, Error as AmlError};
pub use grpc::{Config as GrpcConfig, Error as GrpcError, StreamConfig as GrpcStreamConfig};
pub use http::Error as HttpError;
pub use rpc::{DecodeError, Error as RpcError};
pub use websocket::{
    Config as WebSocketConfig, Error as WebSocketError, Provider as WebSocketProvider,
};

/// Provider configuration for HTTP snapshots and live WebSocket/gRPC updates.
pub struct ChainSyncConfig {
    /// Checks each distinct post-delegation action signer; `None` disables assessment.
    pub aml: Option<AmlConfig>,
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
    /// Executable program; fetches and subscribes to its derived ProgramData address too.
    /// After fetching, keeps only ProgramData for Loader V3 or the program for other loaders.
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
    /// Assesses action signers without granting delegation or mutation authority.
    aml: Option<aml::Client>,
    /// Owns account leases and materialization into local state.
    engine: Engine,
    /// Supplies confirmed snapshots for missing accounts.
    fetcher: Fetcher,
    /// Subscribes to accounts before their HTTP snapshots are fetched.
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
    Engine(#[from] EngineError),
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
    #[error("delegation signer assessment failed: {0}")]
    Aml(#[from] AmlError),
}

/// Result of a synchronization operation.
pub type Result<T> = std::result::Result<T, Error>;

impl ChainSync {
    /// Sets up HTTP, WebSocket, and gRPC providers and starts applying updates.
    /// Takes Engine's sole cache-eviction receiver to remove cached mirrors and their subscriptions.
    /// The worker and transports join Engine's coordinated shutdown.
    pub fn new(
        engine: Engine,
        config: ChainSyncConfig,
        shutdown: &mut ShutdownManager,
    ) -> Result<Arc<Self>> {
        metrics::init();
        if config.grpc.streams.is_empty() {
            return Err(Error::NoGrpcStreams);
        }
        let aml = config.aml.map(aml::Client::new).transpose()?;
        let (websocket, websocket_rx) = Pool::new(config.websocket, shutdown);
        let fetcher = Fetcher::new(config.http, engine.clone())?;
        let (events, grpc_rx) = mpsc::channel(8192);
        let authority = config.grpc.authority;
        let mut grpc = Vec::with_capacity(config.grpc.streams.len());
        for (id, stream) in config.grpc.streams.into_iter().enumerate() {
            let client = grpc::Client::new(
                id,
                stream,
                authority,
                engine.clone(),
                events.clone(),
                shutdown,
            )?;
            grpc.push(client);
        }
        let sync = Arc::new(Self {
            aml,
            engine,
            fetcher,
            websocket,
            grpc,
        });
        let evictions = sync.engine.accounts().subscribe_evictions().map_err(EngineError::from)?;
        let shutdown = shutdown.handle(Service::ChainSyncWorker);
        tokio::spawn(Self::run(
            Arc::clone(&sync),
            websocket_rx,
            grpc_rx,
            evictions,
            shutdown,
        ));
        Ok(sync)
    }

    /// Fetches and materializes requested accounts that are missing from Engine.
    ///
    /// Read-only accounts, programs, and payers are subscribed over WebSocket
    /// before fetching. Writable accounts are fetched without WebSocket subscriptions.
    /// Programs include ProgramData; writable accounts and payers include their
    /// derived delegation record. Delegation records are fetched but not subscribed.
    /// After fetching, Loader V3 keeps the ProgramData subscription; other loaders
    /// keep the program subscription. A payer's WebSocket subscription is removed
    /// when its initial snapshot resolves as delegated here.
    /// Read-only DLP-owned accounts and executable Loader V3 programs are refetched
    /// with their derived companions before materialization; a read-only subscription
    /// is also removed when its account resolves as delegated here.
    /// Canonical token ATAs are resolved with their eATA and delegation record in
    /// a second HTTP fetch after the ATA layout reveals its owner and mint. A local
    /// eATA delegation projects onto the ATA; raw eATAs are not materialized.
    /// Acknowledged WebSocket subscriptions gain one gRPC copy after 30 minutes of tracking.
    /// A gRPC-only subscription remains covered without a WebSocket copy.
    ///
    /// An account is resolved as delegated only when its primary and delegation-record
    /// snapshots are DLP-owned and the record names this Engine's authority.
    /// Records with the default authority instead produce confined `Magic` accounts.
    /// Other snapshots retain their fetched owner and mode. HTTP `null` becomes a default account.
    ///
    /// Writable and program pubkeys must be unique; repeated payer and read-only
    /// requests are collapsed. Requested accounts must not overlap a program's
    /// derived ProgramData address.
    pub async fn sync<I>(&self, requests: I) -> Result<()>
    where
        I: IntoIterator,
        I::Item: Borrow<SyncAccount>,
    {
        let accounts = requests.into_iter().map(|account| *account.borrow()).collect();
        self.sync_waves(accounts).await
    }
}

/// Remote address watched by a transport and optional Loader V3 program to update.
///
/// Ordinary subscriptions watch and update the same address. Loader V3 subscriptions
/// watch ProgramData but materialize its normalized ELF at the local program address.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccountSubscription {
    /// Remote account address requested from the provider.
    pubkey: Pubkey,
    /// Program address to materialize ProgramData ELF under; `None` updates `pubkey` itself.
    program: Option<Pubkey>,
}

impl AccountSubscription {
    /// Local address where this subscription's updates are materialized.
    fn local_pubkey(self) -> Pubkey {
        self.program.unwrap_or(self.pubkey)
    }

    /// Returns a ProgramData subscription whose updates belong to the local program address.
    fn program_data(pubkey: Pubkey) -> Self {
        Self {
            pubkey: get_program_data_address(&pubkey),
            program: Some(pubkey),
        }
    }

    /// Returns the subscriptions that can update this local address:
    /// the address itself and its derived ProgramData address.
    fn for_account(pubkey: Pubkey) -> [Self; 2] {
        [Self { pubkey, program: None }, Self::program_data(pubkey)]
    }
}
