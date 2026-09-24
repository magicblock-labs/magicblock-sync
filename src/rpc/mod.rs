//! Shared JSON-RPC envelopes, account encoding policy, and provider diagnostics.

use account::ENCODING;
use derive_more::Display;
use json::Value;
use serde::{Deserialize, Serialize};

mod account;

pub use account::DecodeError;
pub(crate) use account::WireAccount;

/// Finality shared by snapshots and subscriptions.
const COMMITMENT: &str = "confirmed";
/// Protocol version required by the RPC envelope.
pub(crate) const VERSION: &str = "2.0";
/// Typed request wrapped in the common JSON-RPC envelope.
#[derive(Serialize)]
pub(crate) struct Request<P> {
    /// Fixed JSON-RPC protocol version.
    jsonrpc: &'static str,
    /// Request identity for acknowledgement matching.
    id: u64,
    /// Transport-selected RPC operation.
    method: &'static str,
    /// Operation-specific positional arguments.
    params: P,
}

impl<P> Request<P> {
    /// Uses the common protocol version with typed operation arguments.
    pub(crate) fn new(id: u64, method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: VERSION,
            id,
            method,
            params,
        }
    }
}

/// Account response policy shared by HTTP and WebSocket requests.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountConfig {
    /// Compressed representation accepted by the decoder.
    encoding: &'static str,
    /// Shared confirmed finality.
    commitment: &'static str,
    /// HTTP freshness floor, absent from subscriptions.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_context_slot: Option<u64>,
}

impl AccountConfig {
    /// Applies shared encoding and finality with an optional HTTP slot floor.
    pub(crate) fn new(min_context_slot: Option<u64>) -> Self {
        Self {
            encoding: ENCODING,
            commitment: COMMITMENT,
            min_context_slot,
        }
    }
}

/// Provider context applying to one response value.
#[derive(Deserialize)]
pub(crate) struct Context {
    /// Confirmed slot of the accompanying value.
    pub(crate) slot: u64,
}

/// Context and required value, allowing explicit null only when `T` does.
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct ContextValue<T> {
    /// Observation context for the whole value.
    pub(crate) context: Context,
    /// Required payload; null is valid only for nullable `T`.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(crate) value: T,
}

/// The provider's JSON-RPC error, including optional diagnostic data.
#[derive(Debug, Deserialize, Display, derive_more::Error)]
#[display("RPC {code}: {message}")]
pub struct Error {
    /// Provider's JSON-RPC error code, retained without reclassification.
    pub code: i64,
    /// Provider's human-readable explanation.
    pub message: String,
    /// Optional provider-specific diagnostics preserved for the caller.
    pub data: Option<Value>,
}
