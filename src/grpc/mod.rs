//! Streams confirmed updates for tracked accounts and discovers DLP delegation events.
//!
//! Yellowstone reconnects and may replay, but does not guarantee gapless delivery.
//! Consumers reconcile gaps and deduplicate events before applying lifecycle changes.
//! Delegation matching requires same-slot updates to the account and its derived
//! delegation-record PDA. It assumes at most one delegation per account per slot.
//! Undelegation detection uses static transaction keys, not lookup-table addresses.

use smallvec::SmallVec;
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use url::Url;
use yellowstone_grpc_client::{
    GeyserGrpcBuilderError, GeyserGrpcClientError, SubscribeRequestSinkError,
};
use yellowstone_grpc_proto::{
    cuckoo::{CuckooBuildError, TableFullError},
    tonic,
};

use crate::AccountSubscription;

/// Configuration, transport, and stream failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gRPC client closed")]
    Closed,
    #[error("gRPC protocol: {0}")]
    Protocol(&'static str),
    #[error(transparent)]
    Build(#[from] GeyserGrpcBuilderError),
    #[error(transparent)]
    Client(#[from] GeyserGrpcClientError),
    #[error(transparent)]
    Status(#[from] tonic::Status),
    #[error(transparent)]
    Send(#[from] SubscribeRequestSinkError),
    #[error(transparent)]
    Filter(#[from] CuckooBuildError),
    #[error(transparent)]
    Capacity(#[from] TableFullError),
}

pub(super) type Result<T> = std::result::Result<T, Error>;

/// Shared delegation authority and Yellowstone account-update streams.
pub struct Config {
    /// Authority whose delegation lifecycle every stream observes.
    pub authority: Pubkey,
    /// At least one provider stream; accounts use at most one at a time.
    pub streams: Vec<StreamConfig>,
}

/// Connection settings for one Yellowstone provider.
pub struct StreamConfig {
    /// HTTP(S) endpoint without URL-embedded credentials.
    pub endpoint: Url,
    /// Optional provider `x-token`.
    pub token: Option<String>,
}

/// Ordered account and lifecycle events from one provider.
pub(super) enum Event {
    /// Retained account update in `Uninit` mode, before Engine materialization.
    Update {
        stream: usize,
        /// Remote account address and the local account its updates belong to.
        sub: AccountSubscription,
        /// Raw account update, including zero-lamport updates.
        account: AccountBuilder,
    },
    /// New delegation matched to the account's delegation record PDA.
    Delegated(Delegation),
    /// Canonical DLP request to undelegate a local account, observed at this slot.
    UndelegationRequested { pubkey: Pubkey, slot: u64 },
    /// Accounts undelegated in a successful transaction at this slot.
    Undelegated { pubkeys: SmallVec<[Pubkey; 1]>, slot: u64 },
    /// This account was included in a sent filter for the given subscription generation.
    /// Reports a local send, not server acknowledgement or proof of gapless coverage.
    Confirmed {
        stream: usize,
        pubkey: Pubkey,
        /// Generation assigned by the coverage registry to reject stale confirmations.
        gen: u64,
    },
    /// The outer stream attempt ended; Yellowstone's internal reconnect does not emit this.
    Lost(usize),
}

pub(super) use client::Client;
pub(super) use client::Command;
pub(super) use delegation::Delegation;

/// Validates a provider public key at the stream boundary.
fn pubkey(bytes: &[u8]) -> Result<Pubkey> {
    bytes
        .try_into()
        .map(Pubkey::new_from_array)
        .map_err(|_| Error::Protocol("invalid public key"))
}

/// Shutdown-managed stream commands and client handle.
mod client;
/// Same-slot matching of delegated account images and delegation records.
mod delegation;
/// Yellowstone sessions, retained filters, and ordered event delivery.
mod session;
/// DLP lifecycle decoding from successful transactions and their CPIs.
mod transaction;
