//! Base-layer account fetching and subscriptions with decoded Engine accounts.
//!
//! [`Fetcher`] returns one ordered snapshot for 1–100 keys, using [`Pool::slot`]
//! as a freshness floor. Both transports leave account mode Uninit for caller
//! classification. Companion discovery, subscription-before-fetch coordination,
//! reconciliation, and materialization remain caller responsibilities.
//!
//! [`Pool::subscribe`] reserves capacity; only [`Event::Established`] confirms
//! remote coverage. Drive [`Pool::next`] continuously, including while waiting
//! for establishment or release. Only established [`Subscription`] handles can be
//! released; callers ensure at most one live subscription per account and release
//! each at most once, without retries. [`Pool::release`] enqueues without waiting for
//! remote acknowledgement; obsolete releases are ignored. [`Event::Released`] confirms
//! acknowledgement and frees capacity when consumed through [`Pool::next`].
//! Reconnected sockets restore only their internal Clock subscription: restoring user coverage,
//! reconciling snapshots, and applying account state belong to the caller.

/// Shared decoding from borrowed RPC account data into Engine accounts.
mod account;
/// Per-socket protocol state, deadlines, and ordered event delivery.
mod connection;
/// Concurrent HTTP snapshots with shared provider cooldowns.
mod fetcher;
/// Subscription admission, capacity accounting, and socket replacement.
mod pool;
/// Common RPC wire contracts and fixed request policy.
mod rpc;
/// Public identities, events, configuration, and error contracts.
mod types;
/// WebSocket connection setup, TLS, and upgrade validation.
mod websocket;

pub use fetcher::{Fetcher, Snapshot};
pub use pool::Pool;
pub use solana_account::OwnedAccount;
pub use solana_pubkey::Pubkey;
pub use types::{Config, Connection, Error, Event, Provider, Reservation, RpcError, Subscription};
pub use url::Url;
