use std::io;

use derive_more::Display;
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
    pub(crate) index: usize,
    /// Incremented on reconnect to distinguish old and replacement connections.
    pub(crate) generation: u64,
}

/// Events are ordered per connection, not across connections. Solana context slots are individual
/// observations; the pool's watermark retains the highest confirmed update slot.
#[derive(derive_more::Debug)]
pub enum Event {
    /// A connection is ready to accept subscribe requests,
    /// subject to remaining subscription capacity.
    Connected(Connection),
    /// Decoded account data; callers classify its default Uninit mode before materialization.
    Update {
        /// Account whose remote subscription produced this update.
        pubkey: Pubkey,
        /// Confirmed Solana context slot reported by the provider, not a global ordering guarantee.
        slot: u64,
        /// Decoded account, or `None` only when the provider explicitly reports absence.
        #[debug(skip)]
        account: Option<OwnedAccount>,
    },
    /// All pending, active, and unsubscribing accounts on this connection have lost their subscriptions.
    /// Already queued updates precede this event. The replacement restores only Clock.
    Dropped {
        /// Failed connection identity; its replacement has a new generation.
        connection: Connection,
        /// Lost user pubkeys, already removed from the registry; internal Clock is omitted.
        pubkeys: Vec<Pubkey>,
        /// Cause of subscription loss, including event-delivery failure.
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
    Status(reqwest::StatusCode),
    /// HTTP request construction, transport, or response-body failure.
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    /// The declared account payload is not valid base64.
    #[error("invalid account base64: {0}")]
    Base64(#[from] base64::DecodeError),
    /// The account owner is not a valid public key.
    #[error("invalid account owner: {0}")]
    Owner(#[from] solana_pubkey::ParsePubkeyError),
    /// The decoded bytes do not form a valid zstd payload.
    #[error("invalid account zstd: {0}")]
    Zstd(#[source] io::Error),
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
