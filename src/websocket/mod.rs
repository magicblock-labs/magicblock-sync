//! Streams confirmed account updates through per-provider WebSocket pools.
//!
//! Drain the event receiver while awaiting pool operations. Acknowledgement
//! confirms a subscription, not an initial snapshot. On connection loss,
//! callers may retry missing coverage periodically. Each attempt uses ready capacity
//! or fails without queuing behind a reconnect.
//!

use crate::{
    rpc::{DecodeError, Error as RpcError},
    AccountSubscription,
};
use fastwebsockets::WebSocketError;
use hyper::http;
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use std::io;
use tokio_rustls::rustls::pki_types::InvalidDnsNameError;
use url::Url;

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
    /// The peer closed the socket.
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

/// Provider settings for confirmed subscriptions; an empty list has no capacity.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Provider order determines connection identities.
    pub providers: Vec<Provider>,
}

/// Identity of one connection attempt; reconnects receive a new generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Connection {
    /// Index in [`Config::providers`].
    pub(super) provider: usize,
    /// Stable pool entry reused by replacement attempts.
    index: usize,
    /// Incremented on reconnect to distinguish old and replacement sockets.
    generation: u64,
}

/// Account and connection events, ordered within each connection only.
pub(super) enum Event {
    /// Server acknowledged a user subscription at `at`.
    Acknowledged {
        sub: AccountSubscription,
        at: tokio::time::Instant,
        /// Background acknowledgements cannot start a new logical subscription.
        background: bool,
    },
    /// Caller requested intentional removal, before server acknowledgement.
    Removed(Pubkey),
    /// Confirmed update builder in `Uninit` mode for caller classification.
    Update {
        /// Observed account and optional ProgramData target.
        sub: AccountSubscription,
        /// Decoded account state, including zero-lamport updates.
        account: AccountBuilder,
    },
    /// Lost subscriptions; earlier queued updates precede this event.
    Dropped {
        /// Failed attempt identity; its replacement has a new generation.
        connection: Connection,
        /// Lost user subscriptions, excluding internal `Clock` and cancelled operations.
        pubkeys: Vec<Pubkey>,
        /// Cause of loss, including event-delivery failure.
        error: Error,
    },
}

pub(super) use pool::Pool;

mod pool;
mod session;
mod transport;
