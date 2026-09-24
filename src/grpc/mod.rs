//! Confirmed Yellowstone subscriptions for retained accounts and delegation lifecycle.
//!
//! Orchestration batches additions/removals with [`Client::update`] while keeping
//! WebSockets active. Success means the subscription request was sent, not that
//! the server has established coverage.
//!
//! Yellowstone owns automatic reconnect/replay. Its reconnects are transparent and
//! it may resume live when replay is unavailable. Orchestration owns gap reconciliation
//! and duplicate suppression; this API does not promise gapless or exactly-once delivery.
//! [`Event::Disconnected`] reports terminal failure, including event-delivery timeout.
//!
//! Discovery assumes at most one delegation per account per slot, with no same-slot
//! undelegation/redelegation or commit. Application data may resemble a delegation
//! record: only canonical PDA and slot matching resolve a new [`Delegation`].
//! Updates must be grouped by slot, including replay: both halves must arrive before
//! the stream changes slots. Pending matches are discarded on any slot change.
//! Undelegation detection supports top-level and CPI instructions using static keys;
//! lookup-table address resolution is not supported. Fetching, action execution, and
//! Engine lifecycle transitions remain caller responsibilities.

use solana_account::OwnedAccount;
use solana_pubkey::Pubkey;
use url::Url;
use yellowstone_grpc_client::{
    GeyserGrpcBuilderError, GeyserGrpcClientError, SubscribeRequestSinkError,
};
use yellowstone_grpc_proto::{
    cuckoo::{CuckooBuildError, TableFullError},
    tonic,
};

/// Client commands and task lifetime.
mod client;
/// Account and record matching within one slot.
mod delegation;
/// Subscription construction and upstream-managed recovery.
mod session;
/// Successful ownership returns from top-level and CPI instructions.
mod transaction;

pub use client::Client;
pub use delegation::Delegation;

/// Connection settings for one Yellowstone provider.
pub struct Config {
    /// HTTP(S) gRPC endpoint, without authentication embedded in the URL.
    pub endpoint: Url,
    /// Optional provider `x-token` (including Helius LaserStream API keys).
    pub token: Option<String>,
    /// Delegation authority whose new delegations are discovered.
    pub authority: Pubkey,
}

/// Ordered account and lifecycle observations from a single provider.
pub enum Event {
    /// Exact-membership-filtered account update; mode remains Uninit for caller classification.
    Update {
        /// Subscribed account.
        pubkey: Pubkey,
        /// Confirmed observation slot, also applied to the shared freshness watermark.
        slot: u64,
        /// Raw account image, including zero-lamport updates; classification belongs to the caller.
        account: OwnedAccount,
    },
    /// New delegation resolved against its canonical record in the same slot.
    /// The caller remains responsible for exactly-once activation and action execution.
    Delegated(Delegation),
    /// Successful normal undelegation or timeout rollback; never a state transition itself.
    Refetch {
        /// Distinct affected accounts from top-level and CPI instructions using static keys.
        pubkeys: Vec<Pubkey>,
        /// Minimum context slot for the caller's subsequent fetch.
        min_context_slot: u64,
    },
    /// The stream stopped permanently. Reconcile and create a new client.
    /// Already queued data precedes this event.
    Disconnected(Error),
}

/// Configuration, transport, and stream failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The client or event receiver has closed.
    #[error("gRPC client closed")]
    Closed,
    /// A peer or consumer stopped making progress.
    #[error("gRPC {0} timed out")]
    Timeout(&'static str),
    /// Provider data could not be decoded.
    #[error("gRPC protocol: {0}")]
    Protocol(&'static str),
    /// Client configuration or TLS setup failure.
    #[error(transparent)]
    Build(#[from] GeyserGrpcBuilderError),
    /// Unary RPC or stream establishment failure.
    #[error(transparent)]
    Client(#[from] GeyserGrpcClientError),
    /// Stream status retains the provider's diagnostic and status code.
    #[error(transparent)]
    Status(#[from] tonic::Status),
    /// Subscription request delivery failure.
    #[error(transparent)]
    Send(#[from] SubscribeRequestSinkError),
    /// Invalid compressed-filter capacity.
    #[error(transparent)]
    Filter(#[from] CuckooBuildError),
    /// Compressed-filter insertion could not fit; the client terminates explicitly.
    #[error(transparent)]
    Capacity(#[from] TableFullError),
}

/// Validates a provider public key at the decoding boundary.
fn pubkey(bytes: &[u8]) -> Result<Pubkey, Error> {
    bytes
        .try_into()
        .map(Pubkey::new_from_array)
        .map_err(|_| Error::Protocol("invalid public key"))
}
