//! Base-layer account snapshots and subscriptions with decoded Engine accounts.
//!
//! [`http::Fetcher`] fetches ordered snapshots at a minimum confirmed context slot.
//! [`websocket::Pool`] follows individual accounts and maintains the shared freshness
//! watermark. [`grpc::Client`] adds retained-account redundancy and delegation lifecycle
//! observations through Yellowstone.
//!
//! Orchestration owns subscription-before-fetch coordination, reconciliation,
//! materialization, and transport-specific recovery decisions. HTTP/WebSocket accounts
//! retain Uninit mode for caller classification; resolved gRPC delegations include
//! their original owner and Delegated mode.

pub mod grpc;
pub mod http;
pub mod rpc;
pub mod websocket;

pub use solana_account::OwnedAccount;
pub use solana_pubkey::Pubkey;
pub use url::Url;
