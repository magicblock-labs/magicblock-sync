//! Streams confirmed retained-account and delegation events from Yellowstone.
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
    /// Retained account builder in `Uninit` mode for caller classification.
    Update {
        /// Provider stream that delivered this retained update.
        stream: usize,
        /// Retained account identity.
        pubkey: Pubkey,
        /// Program target when this is a ProgramData subscription.
        target: Option<Pubkey>,
        /// Raw account update, including zero-lamport updates.
        account: AccountBuilder,
    },
    /// New delegation matched to the account's delegation record PDA.
    Delegated(Delegation),
    /// Accounts undelegated in a successful transaction at this slot.
    Undelegated {
        /// Distinct undelegated accounts.
        pubkeys: SmallVec<[Pubkey; 1]>,
        /// Confirmed undelegation slot.
        slot: u64,
    },
    /// A retained filter was sent on a live stream for this logical generation.
    Confirmed {
        /// Stream that sent or already retained the account filter.
        stream: usize,
        /// Exact retained account key.
        pubkey: Pubkey,
        /// Owner-issued identity of the current logical subscription.
        gen: u64,
    },
    /// The outer stream attempt ended; Yellowstone's internal reconnect does not emit this.
    Lost(usize),
}

pub(super) use client::Client;
pub(super) use client::Command;
pub(super) use delegation::Delegation;

/// Validates a provider public key at the stream boundary.
fn pubkey(bytes: &[u8]) -> Result<Pubkey, Error> {
    bytes
        .try_into()
        .map(Pubkey::new_from_array)
        .map_err(|_| Error::Protocol("invalid public key"))
}

mod client;
mod delegation;
mod session;
mod transaction;
