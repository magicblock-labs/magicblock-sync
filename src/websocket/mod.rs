//! Streams confirmed account updates through per-provider WebSocket pools.
//!
//! Drain the event receiver while awaiting pool operations. Acknowledgement
//! confirms a subscription, not an initial snapshot. On connection loss,
//! callers restore subscriptions and reconcile missed updates.
//!

use crate::rpc::{DecodeError, Error as RpcError};
use fastwebsockets::WebSocketError;
use hyper::http;
use solana_account::OwnedAccount;
use solana_pubkey::Pubkey;
use std::io;
use tokio_rustls::rustls::pki_types::InvalidDnsNameError;
use url::Url;

mod pool;
mod session;
mod transport;

pub use pool::Pool;

/// Endpoint and capacity limits for one provider.
#[derive(Clone, Debug)]
pub struct Provider {
    /// `ws` or `wss` endpoint with a host.
    pub url: Url,
    /// Maximum sockets, including connection attempts.
    pub max_connections: usize,
    /// Maximum subscriptions per socket, including pending operations.
    pub subs_per_connection: usize,
}

/// Nonempty provider list for confirmed subscriptions.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Provider order determines connection identities.
    pub providers: Vec<Provider>,
}

/// Identity of one connection attempt; reconnects receive a new generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Connection {
    /// Index in [`Config::providers`].
    pub provider: usize,
    /// Stable pool entry reused by replacement attempts.
    index: usize,
    /// Incremented on reconnect to distinguish old and replacement sockets.
    generation: u64,
}

/// Account and connection events, ordered within each connection only.
#[derive(derive_more::Debug)]
pub enum Event {
    /// A connection can accept subscriptions.
    Connected(Connection),
    /// Confirmed update, decoded in `Uninit` mode for caller classification.
    Update {
        /// Account whose subscription produced this update.
        pubkey: Pubkey,
        /// Confirmed provider context slot, not a global order guarantee.
        slot: u64,
        /// `None` only when the provider explicitly reports absence.
        #[debug(skip)]
        account: Option<OwnedAccount>,
    },
    /// All listed subscriptions were lost; earlier queued updates precede this event.
    Dropped {
        /// Failed attempt identity; its replacement has a new generation.
        connection: Connection,
        /// Lost user subscriptions, excluding internal `Clock`.
        pubkeys: Vec<Pubkey>,
        /// Cause of loss, including event-delivery failure.
        error: Error,
    },
}

/// Subscription or WebSocket failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// All configured subscription capacity is occupied.
    #[error("all provider subscription limits are exhausted")]
    Capacity,
    /// Capacity exists, but no ready socket can admit the request.
    #[error("capacity is connecting or unavailable; wait for connection events")]
    Unavailable,
    #[error("pool or event receiver closed")]
    Closed,
    #[error("peer closed the socket")]
    Disconnected,
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("invalid provider message: {0}")]
    Protocol(&'static str),
    #[error(transparent)]
    Json(#[from] json::Error),
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error(transparent)]
    Account(#[from] DecodeError),
    #[error(transparent)]
    Socket(#[from] WebSocketError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] http::Error),
    #[error(transparent)]
    ServerName(#[from] InvalidDnsNameError),
    #[error(transparent)]
    Tls(#[from] tokio_rustls::rustls::Error),
}
