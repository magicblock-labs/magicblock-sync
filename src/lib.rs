//! Base-layer account snapshots and subscriptions with decoded Engine accounts.
//!
//! [`http::Fetcher`] fetches ordered snapshots at a minimum confirmed context slot.
//! [`websocket::Pool`] follows individual accounts and maintains the shared freshness
//! watermark. [`grpc::Client`] adds retained-account redundancy and delegation lifecycle
//! observations through Yellowstone.
//!
//! [`ChainSync`] currently checks whether requested accounts exist in Engine.
//! Subscription-before-fetch coordination, reconciliation, materialization, and
//! transport recovery remain outside this entry point. HTTP/WebSocket accounts
//! retain Uninit mode for caller classification; resolved gRPC delegations include
//! their original owner and Delegated mode.

pub mod grpc;
pub mod http;
pub mod rpc;
pub mod websocket;

use std::borrow::Borrow;

use engine::Engine;
use solana_pubkey::Pubkey;

/// Synchronization entry point backed by Engine.
pub struct ChainSync(Engine);

/// Account lookup failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The requested account is not in Engine.
    #[error("account {0} is missing from Engine")]
    Missing(Pubkey),
    /// Engine's account loader could not check an account.
    #[error("Engine account lookup failed: {0}")]
    AccountsDb(#[from] accountsdb::AccountsDBError),
}

impl From<Engine> for ChainSync {
    fn from(engine: Engine) -> Self {
        Self(engine)
    }
}

impl ChainSync {
    /// Checks each key against one Engine loader without fetching or materializing.
    pub fn sync<I>(&self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator,
        I::Item: Borrow<Pubkey>,
    {
        let accounts = self.0.accounts();
        let loader = accounts.loader();
        for key in keys {
            let key = *key.borrow();
            if !loader.contains(&key)? {
                return Err(Error::Missing(key));
            }
        }
        Ok(())
    }
}
