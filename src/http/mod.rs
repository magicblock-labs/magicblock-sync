//! Concurrent HTTP account snapshots with provider failover and a shared freshness floor.

use crate::rpc::{DecodeError, Error as RpcError};

mod fetcher;

pub use fetcher::{Fetcher, Snapshot};

/// HTTP snapshot failures, retaining provider context and the latest retry cause.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fetch requires between 1 and 100 keys")]
    BatchSize,
    /// Failure from a configured provider, identified by its input index.
    #[error("HTTP provider {provider}: {source}")]
    Provider {
        /// Stable endpoint index supplied to the fetcher.
        provider: usize,
        /// Underlying attempt failure.
        #[source]
        source: Box<Error>,
    },
    /// Overall budget expired; `last` retains the latest attempt failure.
    #[error("fetch deadline exhausted")]
    Deadline {
        /// Latest attempt failure, if one occurred before timeout.
        #[source]
        last: Option<Box<Error>>,
    },
    #[error("HTTP status {0}")]
    Status(reqwest::StatusCode),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
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
}

impl Error {
    /// Distinguishes transient endpoint failures from final decoding failures.
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
