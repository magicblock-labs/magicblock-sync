//! Confirmed account subscriptions with per-provider connection pools.
//!
//! [`Pool`] routes subscriptions by pubkey and returns a separate event receiver.
//! Drain events while awaiting operations; backpressure can delay acknowledgements.
//!
//! Subscription success means the server acknowledged the request, not that an
//! initial snapshot arrived. On connection loss, [`Event::Dropped`] lists the
//! lost pubkeys. Replacement connections restore only the internal `Clock`
//! subscription; callers decide what to restore and reconcile missed updates.
//!

use crate::rpc::{DecodeError, Error as RpcError};
use fastwebsockets::WebSocketError;
use hyper::http;
use solana_account::OwnedAccount;
use solana_pubkey::Pubkey;
use std::io;
use tokio_rustls::rustls::pki_types::InvalidDnsNameError;
use url::Url;

/// Subscription routing, capacity accounting, and task ownership.
mod pool;
/// Per-socket protocol state and ordered delivery.
mod session;
/// WebSocket setup, TLS, and upgrade validation.
mod transport;

pub use pool::Pool;

/// A provider's independent hard limits. Its index in the configuration is its identity.
#[derive(Clone, Debug)]
pub struct Provider {
    /// WebSocket endpoint; must be a `ws` or `wss` URL with a host.
    pub url: Url,
    /// Positive hard socket limit, including connecting and reconnecting attempts.
    pub max_connections: usize,
    /// Positive subscription limit per connection,
    /// including pending subscribe/unsubscribe requests.
    pub subs_per_connection: usize,
}

/// Providers for a pool; all subscriptions use confirmed commitment.
/// Callers must supply at least one valid provider with positive limits.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Nonempty ordered provider list; its indices become stable connection identities.
    pub providers: Vec<Provider>,
}

/// Identifies one connection attempt by provider, pool-entry index, and generation.
/// Reconnecting reuses the pool entry but advances the generation, producing a new identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Connection {
    /// Index of the provider in the pool's original configuration.
    pub provider: usize,
    /// Stable index in the connection pool, reused on reconnect; unrelated to Solana slots.
    index: usize,
    /// Incremented on reconnect to distinguish old and replacement connections.
    generation: u64,
}

/// Events are ordered per connection, not across connections. The shared watermark
/// retains the highest confirmed context slot observed in an account update.
#[derive(derive_more::Debug)]
pub enum Event {
    /// A connection is ready to accept subscribe requests,
    /// subject to remaining subscription capacity.
    Connected(Connection),
    /// Decoded account data in `Uninit` mode for caller classification.
    Update {
        /// Account whose remote subscription produced this update.
        pubkey: Pubkey,
        /// Confirmed Solana context slot reported by the provider, not a global ordering guarantee.
        slot: u64,
        /// Decoded account, or `None` only when the provider explicitly reports absence.
        #[debug(skip)]
        account: Option<OwnedAccount>,
    },
    /// All subscriptions on this connection were lost, including pending operations.
    /// Updates already queued from this connection precede this event.
    Dropped {
        /// Failed connection identity; its replacement has a new generation.
        connection: Connection,
        /// Lost user pubkeys, already removed from the registry; internal `Clock` is omitted.
        pubkeys: Vec<Pubkey>,
        /// Cause of subscription loss, including event-delivery failure.
        error: Error,
    },
}

/// WebSocket admission, transport, and protocol failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// All configured subscription capacity is occupied.
    #[error("all provider subscription limits are exhausted")]
    Capacity,
    /// Subscription capacity exists but no healthy connection can currently accept the request.
    #[error("capacity is connecting or unavailable; wait for connection events")]
    Unavailable,
    /// The pool or event consumer is no longer available.
    #[error("pool or event receiver closed")]
    Closed,
    /// The peer ended the WebSocket connection.
    #[error("peer closed the socket")]
    Disconnected,
    /// An operation exhausted its time budget.
    #[error("{0} timed out")]
    Timeout(&'static str),
    /// Provider data or endpoint configuration violates the transport contract.
    #[error("invalid provider message: {0}")]
    Protocol(&'static str),
    /// A request could not be serialized or a response could not be parsed.
    #[error(transparent)]
    Json(#[from] json::Error),
    /// The provider rejected an RPC operation.
    #[error(transparent)]
    Rpc(#[from] RpcError),
    /// Shared account decoding failed.
    #[error(transparent)]
    Account(#[from] DecodeError),
    /// WebSocket framing or transport failed.
    #[error(transparent)]
    Socket(#[from] WebSocketError),
    /// Socket setup or stream I/O failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The WebSocket upgrade request could not be constructed.
    #[error(transparent)]
    Http(#[from] http::Error),
    /// The endpoint host cannot be used as a TLS server name.
    #[error(transparent)]
    ServerName(#[from] InvalidDnsNameError),
    /// Shared WebSocket TLS configuration could not be initialized.
    #[error(transparent)]
    Tls(#[from] tokio_rustls::rustls::Error),
}
