//! Concurrent HTTP account snapshots with provider failover and a shared freshness floor.

use crate::rpc::{DecodeError, Error as RpcError};

/// Account fetching and response decoding.
mod fetcher;

pub use fetcher::{Fetcher, Snapshot};

/// HTTP snapshot failures, retaining provider context and the latest retry cause.
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
}

impl Error {
    /// Only transient endpoint failures may consume the remaining failover budget.
    fn retryable(&self) -> bool {
        match self {
            Error::Status(status) => {
                status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
            }
            Error::Rpc(RpcError { code, .. }) => matches!(code, -32005 | -32016),
            Error::Request(error) => !error.is_builder() && !error.is_decode(),
            Error::Timeout(_) => true,
            _ => false,
        }
    }
}
