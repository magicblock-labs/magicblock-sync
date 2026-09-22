//! Base-layer account fetching and subscriptions with decoded Engine accounts.
//!
//! [`Fetcher`] returns one ordered snapshot for 1–100 keys, using [`Pool::slot`]
//! as the minimum confirmed Solana context slot. Both transports leave account mode Uninit for caller
//! classification. Companion discovery, subscription-before-fetch coordination,
//! reconciliation, and materialization remain caller responsibilities.
//!
//! [`Pool`] owns subscription routing by pubkey. Its cloneable handles provide
//! acknowledged async subscribe/unsubscribe operations; consume the separate event
//! receiver concurrently so delivery backpressure cannot stall acknowledgements.
//! Subscribe only without an existing subscription, then unsubscribe after successful subscribe.
//! Operations for the same pubkey must not overlap or be cancelled. Clock is reserved
//! for internal use. Unsubscribe tolerates subscriptions already removed by connection loss.
//! Successful subscribe confirms the server's subscription acknowledgement, not an initial snapshot. Unsubscribe
//! reclaims capacity before returning, but previously buffered updates may remain.
//! Connection loss removes affected pubkeys and reports them in [`Event::Dropped`].
//! Replacements restore only Clock; callers restore user subscriptions and reconcile snapshots.

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
pub use types::{Config, Connection, Error, Event, Provider, RpcError};
pub use url::Url;
