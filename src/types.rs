use std::io;

use derive_more::{Deref, Display};
use fastwebsockets::WebSocketError;
use hyper::http;
use json::Value;
use serde::Deserialize;
use tokio_rustls::rustls::pki_types::InvalidDnsNameError;

use crate::{Pubkey, UiAccount, Url};

/// A provider's independent hard limits. Its index in the configuration is its identity.
#[derive(Clone, Debug)]
pub struct Provider {
    /// WebSocket endpoint; must be a `ws` or `wss` URL with a host.
    pub url: Url,
    /// Positive hard socket limit, including connecting and reconnecting attempts.
    pub max_connections: usize,
    /// Positive per-socket reservation limit, including pending and releasing subscriptions.
    pub subs_per_connection: usize,
}

/// Providers for a pool; all subscriptions use confirmed commitment.
/// Callers must supply at least one valid provider with positive limits.
/// These requirements are assumed, not checked at construction.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Nonempty ordered provider list; its indices become stable connection identities.
    pub providers: Vec<Provider>,
}

/// A socket incarnation. Replacement sockets never reuse this identity within a pool.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Connection {
    /// Index of the provider in the pool's original configuration.
    pub provider: usize,
    /// Stable pool slot reused by successive socket incarnations.
    pub(crate) index: usize,
    /// Incremented on replacement to distinguish lost coverage from the new socket.
    pub(crate) generation: u64,
}

/// An opaque reservation, unique within the pool that issued it, even after release.
/// Admission does not imply remote coverage and cannot be released before establishment.
/// Do not pass handles between pools. Copying a handle does not acquire another lease.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Reservation {
    /// Account requested by the caller; uniqueness is the caller's responsibility.
    pub(crate) account: Pubkey,
    /// Socket incarnation responsible for this reservation's coverage.
    pub(crate) connection: Connection,
    /// Pool-local sequence distinguishing successive reservations for the same account.
    pub(crate) id: u64,
}

/// Established remote coverage, releasable only within the pool that issued it.
/// Copying a handle does not acquire another lease.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deref)]
pub struct Subscription {
    /// Local admission identity, preserved through establishment and release.
    #[deref]
    pub(crate) reservation: Reservation,
    /// Provider-issued ID, scoped to the reservation's socket incarnation.
    pub(crate) remote: u64,
}

/// Events are ordered per socket, not across providers. Slots are observations, not a watermark.
#[derive(Debug)]
pub enum Event {
    /// An empty socket is ready to accept reservations.
    Connected(Connection),
    /// The provider acknowledged accountSubscribe. No initial snapshot is implied.
    Established(Subscription),
    /// Account data remains in the requested base64+zstd representation for caller-side decoding.
    Update {
        /// Established handle whose remote subscription produced this update.
        subscription: Subscription,
        /// Provider observation slot, not a global ordering guarantee.
        slot: u64,
        /// Base64+zstd wire account, or `None` when the provider explicitly reports absence.
        account: Option<UiAccount>,
    },
    /// The provider acknowledged release.
    Released(Subscription),
    /// A subscription was rejected; its reservation has been freed.
    Rejected {
        /// Reservation freed by the failed subscribe request.
        reservation: Reservation,
        /// Provider explanation for rejecting establishment.
        error: RpcError,
    },
    /// All pending, active, and releasing reservations on this incarnation are invalid.
    /// Already queued updates precede this event. The replacement starts empty.
    Dropped {
        /// Failed incarnation; its replacement has a distinct generation.
        connection: Connection,
        /// All reservations still held on this incarnation when the pool consumes the event.
        reservations: Vec<Reservation>,
        /// Failure that invalidated coverage, including undeliverable events.
        error: Error,
    },
}

/// The provider's JSON-RPC error, including optional diagnostic data.
#[derive(Debug, Deserialize, Display, derive_more::Error)]
#[display("RPC {code}: {message}")]
pub struct RpcError {
    /// Provider's JSON-RPC error code, retained without reclassification.
    pub code: i64,
    /// Provider's human-readable explanation.
    pub message: String,
    /// Optional provider-specific diagnostics preserved for the caller.
    pub data: Option<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("all provider subscription limits are exhausted")]
    Capacity,
    #[error("capacity is connecting or unavailable; wait for connection events")]
    Unavailable,
    #[error("available socket command queues are full; retry after draining events")]
    Busy,
    #[error("subscription is no longer current")]
    Stale,
    #[error("event receiver closed")]
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
    Socket(#[from] WebSocketError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Http(#[from] http::Error),
    #[error(transparent)]
    ServerName(#[from] InvalidDnsNameError),
}
