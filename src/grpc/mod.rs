//! Streams confirmed retained-account and delegation events from Yellowstone.
//!
//! Yellowstone reconnects and may replay, but does not guarantee gapless delivery.
//! Callers reconcile gaps, deduplicate events, and perform Engine lifecycle changes.
//! Delegation matching requires a canonical record in the same slot and assumes
//! at most one delegation per account per slot;
//! undelegation detection uses static transaction keys, not lookup-table addresses.

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

mod client;
mod delegation;
mod session;
mod transaction;

pub use client::Client;
pub use delegation::Delegation;

/// Settings for one Yellowstone provider.
pub struct Config {
    /// HTTP(S) endpoint without URL-embedded credentials.
    pub endpoint: Url,
    /// Optional provider `x-token`.
    pub token: Option<String>,
    /// Authority whose delegations are observed.
    pub authority: Pubkey,
}

/// Ordered account and lifecycle events from one provider.
pub enum Event {
    /// Retained-account update in `Uninit` mode for caller classification.
    Update {
        /// Retained account identity.
        pubkey: Pubkey,
        /// Confirmed observation slot shared with the freshness watermark.
        slot: u64,
        /// Raw account image, including zero-lamport updates.
        account: OwnedAccount,
    },
    /// New delegation resolved from an account and its canonical record.
    Delegated(Delegation),
    /// Accounts affected by an ownership return; fetch at or after the given slot.
    Refetch {
        /// Distinct accounts affected by the ownership return.
        pubkeys: Vec<Pubkey>,
        /// Lower bound for the caller's subsequent snapshot.
        min_context_slot: u64,
    },
    /// Terminal stream failure; queued events precede it.
    Disconnected(Error),
}

/// Configuration, transport, and stream failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("gRPC client closed")]
    Closed,
    #[error("gRPC {0} timed out")]
    Timeout(&'static str),
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

/// Validates a provider public key at the stream boundary.
fn pubkey(bytes: &[u8]) -> Result<Pubkey, Error> {
    bytes
        .try_into()
        .map(Pubkey::new_from_array)
        .map_err(|_| Error::Protocol("invalid public key"))
}
