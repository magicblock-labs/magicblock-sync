use std::io;

use derive_more::{Deref, Display};
use fastwebsockets::WebSocketError;
use hyper::http;
use json::Value;
use serde::Deserialize;
use tokio_rustls::rustls::pki_types::InvalidDnsNameError;

use crate::{OwnedAccount, Pubkey, Url};

/// A provider's independent hard limits. Its index in the configuration is its identity.
#[derive(Clone, Debug)]
pub struct Provider {
    /// WebSocket endpoint; must be a `ws` or `wss` URL with a host.
    pub url: Url,
    /// Positive hard socket limit, including connecting and reconnecting attempts.
    pub max_connections: usize,
    /// Positive per-socket reservation limit, including pending and releasing subscriptions.
    /// Each provider's initial socket consumes one reservation for internal Clock tracking.
    pub subs_per_connection: usize,
}

/// Providers for a pool; all subscriptions use confirmed commitment.
/// Callers must supply at least one valid provider with positive limits.
/// Total configured subscription capacity must not exceed `usize::MAX / 4`.
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
/// One lifecycle owner releases it at most once, without retries. Copying a handle
/// does not acquire another lease; release returns after enqueue, not acknowledgement.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Deref)]
pub struct Subscription {
    /// Local admission identity, preserved through establishment and release.
    #[deref]
    pub(crate) reservation: Reservation,
    /// Provider-issued ID, scoped to the reservation's socket incarnation.
    pub(crate) remote: u64,
}

/// Events are ordered per socket, not across providers. Update slots are individual
/// observations; the pool's watermark retains the highest confirmed update slot.
#[derive(derive_more::Debug)]
pub enum Event {
    /// A socket is ready to accept user reservations, subject to internal Clock capacity.
    Connected(
        /// Incarnation whose connection attempt succeeded.
        Connection,
    ),
    /// The provider acknowledged accountSubscribe. No initial snapshot is implied.
    Established(
        /// Acknowledged coverage that can now be released by its owner.
        Subscription,
    ),
    /// Decoded account data; callers classify its default Uninit mode before materialization.
    Update {
        /// Established handle whose remote subscription produced this update.
        subscription: Subscription,
        /// Provider observation slot, not a global ordering guarantee.
        slot: u64,
        /// Decoded account, or `None` only when the provider explicitly reports absence.
        #[debug(skip)]
        account: Option<OwnedAccount>,
    },
    /// The provider acknowledged release.
    Released(
        /// Coverage whose reservation capacity has been reclaimed.
        Subscription,
    ),
    /// A subscription was rejected; its reservation has been freed.
    Rejected {
        /// Reservation freed by the failed subscribe request.
        reservation: Reservation,
        /// Provider explanation for rejecting establishment.
        error: RpcError,
    },
    /// All pending, active, and releasing reservations on this incarnation are invalid.
    /// Already queued updates precede this event. The replacement restores only Clock.
    Dropped {
        /// Failed incarnation; its replacement has a distinct generation.
        connection: Connection,
        /// All user reservations still held when consumed; internal Clock is omitted.
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

/// Admission, transport, and decoding failures, retaining provider causes where available.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested account is already maintained by internal subscriptions.
    #[error("Clock subscriptions are internally owned")]
    Clock,
    /// The batch falls outside the supported single-request key count.
    #[error("fetch requires between 1 and 100 keys")]
    BatchSize,
    /// An HTTP attempt failed on the identified configured provider.
    #[error("HTTP provider {provider}: {source}")]
    Provider {
        /// Stable index in the fetcher's endpoint list.
        provider: usize,
        /// Original attempt failure, including transport or decoding diagnostics.
        #[source]
        source: Box<Error>,
    },
    /// The overall fetch budget expired before a complete snapshot was obtained.
    #[error("fetch deadline exhausted")]
    Deadline {
        /// Most recent provider failure, if any attempt failed before exhaustion.
        #[source]
        last: Option<Box<Error>>,
    },
    /// The HTTP endpoint returned a non-success status.
    #[error("HTTP status {0}")]
    Status(
        /// Provider's response status.
        reqwest::StatusCode,
    ),
    /// HTTP request construction, transport, or response-body failure.
    #[error(transparent)]
    Request(
        /// Underlying client failure.
        #[from]
        reqwest::Error,
    ),
    /// The declared account payload is not valid base64.
    #[error("invalid account base64: {0}")]
    Base64(
        /// Decoder diagnostics.
        #[from]
        base64::DecodeError,
    ),
    /// The account owner is not a valid public key.
    #[error("invalid account owner: {0}")]
    Owner(
        /// Public-key parsing failure.
        #[from]
        solana_pubkey::ParsePubkeyError,
    ),
    /// The decoded bytes do not form a valid zstd payload.
    #[error("invalid account zstd: {0}")]
    Zstd(
        /// Decompression failure.
        #[source]
        io::Error,
    ),
    /// All configured reservation capacity is occupied.
    #[error("all provider subscription limits are exhausted")]
    Capacity,
    /// Capacity exists but no healthy socket can currently accept the reservation.
    #[error("capacity is connecting or unavailable; wait for connection events")]
    Unavailable,
    /// The event consumer is no longer available.
    #[error("event receiver closed")]
    Closed,
    /// The peer ended the WebSocket connection.
    #[error("peer closed the socket")]
    Disconnected,
    /// An operation exhausted its time budget.
    #[error("{0} timed out")]
    Timeout(
        /// Operation whose budget expired.
        &'static str,
    ),
    /// Provider data or endpoint configuration violates the transport contract.
    #[error("invalid provider message: {0}")]
    Protocol(
        /// Description of the violated contract.
        &'static str,
    ),
    /// A request could not be serialized or a response could not be parsed.
    #[error(transparent)]
    Json(
        /// JSON codec diagnostics.
        #[from]
        json::Error,
    ),
    /// The provider rejected an RPC operation.
    #[error(transparent)]
    Rpc(
        /// Provider's structured rejection.
        #[from]
        RpcError,
    ),
    /// WebSocket framing or transport failed.
    #[error(transparent)]
    Socket(
        /// WebSocket failure.
        #[from]
        WebSocketError,
    ),
    /// Socket setup or stream I/O failed.
    #[error(transparent)]
    Io(
        /// Underlying I/O failure.
        #[from]
        io::Error,
    ),
    /// The WebSocket upgrade request could not be constructed.
    #[error(transparent)]
    Http(
        /// HTTP request construction failure.
        #[from]
        http::Error,
    ),
    /// The endpoint host cannot be used as a TLS server name.
    #[error(transparent)]
    ServerName(
        /// Invalid server-name diagnostics.
        #[from]
        InvalidDnsNameError,
    ),
    /// Shared WebSocket TLS configuration could not be initialized.
    #[error(transparent)]
    Tls(
        /// TLS configuration failure.
        #[from]
        tokio_rustls::rustls::Error,
    ),
}
