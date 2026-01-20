//! Real-time synchronization of Solana delegation records via Laserstream.
//!
//! This crate provides [`DlpSyncer`], an async service that subscribes to delegation program
//! events on Solana and streams account and transaction updates to subscribers.
//!
//! # Usage
//!
//! ```no_run
//! use dlp_sync::DlpSyncer;
//!
//! # async fn example() -> Result<(), dlp_sync::DlpSyncError> {
//! let channels = DlpSyncer::start(
//!     "http://localhost:8000".to_string(),
//!     "your-api-key".to_string()
//! ).await?;
//!
//! let (requester, mut updates) = channels.split();
//!
//! // Subscribe to a delegation record
//! let pubkey = [0u8; 32];
//! if let Some(slot) = requester.subscribe(pubkey).await {
//!     println!("Subscribed at slot: {}", slot);
//! }
//!
//! // Receive updates
//! while let Some(update) = updates.recv().await {
//!     match update {
//!         dlp_sync::AccountUpdate::Delegated { record, slot, .. } => {
//!             println!("Delegation at slot {}", slot);
//!         }
//!         dlp_sync::AccountUpdate::Undelegated { record, slot } => {
//!             println!("Undelegation at slot {}", slot);
//!         }
//!         dlp_sync::AccountUpdate::SyncTerminated => break,
//!     }
//! }
//! # Ok(())
//! # }
//! ```

mod channels;
mod consts;
mod syncer;
pub mod transaction_syncer;
mod types;

pub use channels::{DlpSyncChannelsInit, DlpSyncChannelsRequester};
pub use syncer::DlpSyncer;
pub use types::{AccountUpdate, DlpSyncError, Pubkey, Slot};
