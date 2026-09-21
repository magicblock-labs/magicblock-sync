//! Base-layer account subscriptions, with bounded provider pools and explicit coverage loss.
//!
//! [`Pool::subscribe`] reserves capacity; only [`Event::Established`] confirms
//! remote coverage. Drive [`Pool::next`] continuously, including while waiting
//! for establishment or release. Only established [`Subscription`] handles can be
//! released; callers ensure at most one live subscription per account.
//! Reconnected sockets start empty: restoring coverage,
//! reconciling snapshots, and applying account state belong to the caller.

mod connection;
mod pool;
mod types;
mod websocket;

pub use pool::Pool;
pub use solana_account_decoder_client_types::UiAccount;
pub use solana_pubkey::Pubkey;
pub use types::{Config, Connection, Error, Event, Provider, Reservation, RpcError, Subscription};
pub use url::Url;
