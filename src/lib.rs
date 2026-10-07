#![doc = include_str!("../README.md")]

mod acquisition;
mod aml;
mod ata;
mod delegation;
mod grpc;
mod http;
mod metrics;
mod program;
mod rpc;
mod websocket;
mod worker;

use std::{borrow::Borrow, sync::Arc};

use engine::{Engine, EngineError};
use nucleus::shutdown::{Service, ShutdownManager};
use solana_loader_v3_interface::get_program_data_address;
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;
use url::Url;

use crate::http::Fetcher;
use crate::websocket::Pool;

pub use aml::{Config as AmlConfig, Error as AmlError};
pub use grpc::{Error as GrpcError, StreamConfig as GrpcStreamConfig};
pub use http::Error as HttpError;
pub use rpc::{DecodeError, Error as RpcError};
pub use websocket::{
    Config as WebSocketConfig, Error as WebSocketError, Provider as WebSocketProvider,
};

/// Provider configuration for HTTP snapshots and live WebSocket/gRPC updates.
pub struct ChainSyncConfig {
    /// Checks each distinct post-delegation action signer; `None` disables assessment.
    pub aml: Option<AmlConfig>,
    /// HTTP snapshot providers on the same base chain; must be nonempty.
    pub http: Vec<Url>,
    /// WebSocket subscription providers.
    pub websocket: WebSocketConfig,
    /// Yellowstone-compatible streams for delegation events and subscription redundancy.
    /// Must contain at least one stream.
    pub grpc: Vec<GrpcStreamConfig>,
}

/// Transaction role that determines acquisition and live updates in [`ChainSync::sync`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountProperty {
    /// Fee payer; checked for delegation and subscribed unless delegated to this Engine.
    Payer,
    /// Writable transaction account; fetched without a WebSocket subscription.
    Writable,
    /// Read-only transaction account; subscribed unless resolved as delegated to this Engine.
    Readonly,
    /// Executable program; tracks Loader V3 upgrades through ProgramData,
    /// and other supported loaders through the program account itself.
    Program,
}

/// An account to acquire, with any companion state required by its transaction role.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChainSyncAccount {
    /// Address under which the account is made available in Engine.
    pub pubkey: Pubkey,
    /// Role in the transaction that needs this account.
    pub property: AccountProperty,
}

/// Acquires missing base-chain accounts for Engine and applies live provider updates.
pub struct ChainSync {
    aml: Option<aml::Client>,
    engine: Engine,
    fetcher: Fetcher,
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
    /// Starts account synchronization services under the supplied shutdown manager.
    ///
    /// Only one synchronizer may own an Engine's cache-eviction receiver; construction
    /// fails if it is already taken. Provider connections start in the background,
    /// so success does not mean they are ready for [`Self::sync`].
    /// See [`ChainSyncConfig`] for provider requirements.
    pub fn new(
        engine: Engine,
        config: ChainSyncConfig,
        shutdown: &mut ShutdownManager,
    ) -> Result<Arc<Self>> {
        metrics::init();
        let aml = config.aml.map(aml::Client::new).transpose()?;
        let (websocket, websocket_rx) = Pool::new(config.websocket, shutdown);
        let fetcher = Fetcher::new(config.http, engine.clone())?;
        let (events, grpc_rx) = mpsc::channel(8192);
        let mut grpc = Vec::with_capacity(config.grpc.len());
        for (id, stream) in config.grpc.into_iter().enumerate() {
            let client = grpc::Client::new(id, stream, engine.clone(), events.clone(), shutdown)?;
            grpc.push(client);
        }
        let chain_sync = Arc::new(Self {
            aml,
            engine,
            fetcher,
            websocket,
            grpc,
        });
        let evictions =
            chain_sync.engine.accounts().subscribe_evictions().map_err(EngineError::from)?;
        let shutdown = shutdown.handle(Service::ChainSyncWorker);
        tokio::spawn(Self::run(
            Arc::clone(&chain_sync),
            websocket_rx,
            grpc_rx,
            evictions,
            shutdown,
        ));
        Ok(chain_sync)
    }

    /// Fetches and materializes requested accounts that are missing from Engine.
    ///
    /// Resident accounts are left unchanged and gain no new subscriptions.
    /// [`AccountProperty`] determines which missing accounts receive live updates;
    /// subscriptions precede the initial snapshot. Required ProgramData, delegation
    /// records, and post-delegation action dependencies are acquired automatically.
    ///
    /// Delegation requires DLP-owned account and record snapshots with an accepted
    /// authority. This Engine's authority produces a delegated account; the default
    /// authority produces a confined `Magic` account. Ordinary snapshots enter in
    /// `Uninit` mode, executable images in `ReadOnly` mode, and HTTP `null` becomes
    /// a default account at the snapshot slot. Canonical token ATAs may project a
    /// locally delegated eATA balance; raw eATAs are not materialized.
    ///
    /// Success includes dependency acquisition and post-delegation action handling,
    /// but does not imply completion of any scheduled on-chain rescue. Batches can
    /// observe different slots, and errors do not roll back earlier materializations.
    ///
    /// Returns the number of account entries fetched over HTTP, including companions,
    /// action dependencies, and null results. Refetches in later acquisition waves
    /// count again; provider retries do not. Empty or entirely resident requests
    /// return zero. Background stream work and AML requests are excluded;
    /// errors do not expose a partial count.
    ///
    /// # Input requirements
    ///
    /// Writable and program pubkeys must be unique; repeated payer and read-only
    /// requests are collapsed. Requested accounts must not overlap a program's
    /// derived ProgramData address.
    pub async fn sync<I>(&self, requests: I) -> Result<usize>
    where
        I: IntoIterator,
        I::Item: Borrow<ChainSyncAccount>,
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
    pubkey: Pubkey,
    /// Program address to materialize ProgramData ELF under; `None` updates `pubkey` itself.
    program: Option<Pubkey>,
}

impl AccountSubscription {
    fn local_pubkey(self) -> Pubkey {
        self.program.unwrap_or(self.pubkey)
    }

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

#[cfg(test)]
extern crate self as magicblock_chainsync;

#[cfg(test)]
#[path = "../tests/transport/mod.rs"]
mod transport;
