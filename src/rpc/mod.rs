//! Shared JSON-RPC envelopes, account encoding policy, and provider diagnostics.

use account::ENCODING;
use derive_more::Display;
use json::Value;
use serde::{Deserialize, Serialize};

/// Borrowed wire accounts and their decoded-data errors.
mod account;

pub use account::DecodeError;
pub(crate) use account::WireAccount;

/// Finality required for snapshots and every subscription contributing to the watermark.
const COMMITMENT: &str = "confirmed";
/// JSON-RPC version used for requests and response validation.
pub(crate) const VERSION: &str = "2.0";
/// Requests share a protocol version, but retain transport-specific methods and params.
#[derive(Serialize)]
pub(crate) struct Request<P> {
    /// Fixed protocol version; callers only choose the operation and its arguments.
    jsonrpc: &'static str,
    /// Correlates the acknowledgement with its originating request.
    id: u64,
    /// RPC operation selected by the transport.
    method: &'static str,
    /// Operation-specific positional arguments without an intermediate JSON tree.
    params: P,
}

impl<P> Request<P> {
    /// Wraps typed operation arguments in the common JSON-RPC envelope.
    pub(crate) fn new(id: u64, method: &'static str, params: P) -> Self {
        Self {
            jsonrpc: VERSION,
            id,
            method,
            params,
        }
    }
}

/// Both transports use the same commitment and encoding. Only HTTP supplies a slot floor.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountConfig {
    /// Fixed compressed representation understood by the account decoder.
    encoding: &'static str,
    /// Fixed finality shared with the pool's watermark observations.
    commitment: &'static str,
    /// HTTP freshness floor; omitted from subscription requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    min_context_slot: Option<u64>,
}

impl AccountConfig {
    /// Applies the common account policy with an optional HTTP freshness floor.
    pub(crate) fn new(min_context_slot: Option<u64>) -> Self {
        Self {
            encoding: ENCODING,
            commitment: COMMITMENT,
            min_context_slot,
        }
    }
}

/// Provider observation context shared by the accompanying value.
#[derive(Deserialize)]
pub(crate) struct Context {
    /// Confirmed slot at which the provider observed the response value.
    pub(crate) slot: u64,
}

/// A missing value is invalid even when T permits explicit null.
#[derive(Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct ContextValue<T> {
    /// Observation context applying to the complete payload.
    pub(crate) context: Context,
    /// Required payload; explicit null is accepted only when T permits it.
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
