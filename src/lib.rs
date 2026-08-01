//! Real-time synchronization of Solana delegation records via Laserstream.
//!
//! This crate provides [`DlpSyncer`], an async service that subscribes to delegation program
//! events on Solana and streams account and transaction updates to subscribers.
//!
//! # Consistency contract
//!
//! The syncer is a fast path over the live record stream, not a store of
//! record state: it observes updates from the moment it connects and retains
//! a bounded replay window. The absence of an update for a record is never
//! evidence that the record does not exist or was undelegated — consumers
//! must fall back to fetching the record when the syncer has nothing for it.
//! An [`AccountUpdate::SyncInterrupted`] event means updates were lost;
//! delegation state cached from earlier updates must be revalidated at the
//! source. Updates replayed after a resume can repeat slots already seen, so
//! consumers must apply updates idempotently.
//!
//! # Usage
//!
//! ```no_run
//! use magicblock_sync::DlpSyncer;
//!
//! # async fn example() -> Result<(), magicblock_sync::DlpSyncError> {
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
//!         magicblock_sync::AccountUpdate::Delegated { record, slot, .. } => {
//!             println!("Delegation at slot {}", slot);
//!         }
//!         magicblock_sync::AccountUpdate::Undelegated { record, slot } => {
//!             println!("Undelegation at slot {}", slot);
//!         }
//!         magicblock_sync::AccountUpdate::SlotAdvanced(_) => {} // firehose mode only
//!         magicblock_sync::AccountUpdate::SyncInterrupted => {
//!             println!("Updates lost; revalidate cached delegation state");
//!         }
//!         magicblock_sync::AccountUpdate::SyncTerminated => break,
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Firehose mode
//!
//! [`DlpSyncer::start_firehose`] delivers **every** delegation-record update
//! on chain — no per-record subscriptions — interleaved with in-band
//! [`AccountUpdate::SlotAdvanced`] watermarks: receiving `SlotAdvanced(s)`
//! proves all record updates up to slot `s` were already delivered on the
//! channel. Delivery is lossless (a slow consumer backpressures the stream
//! instead of dropping updates), which lets a consumer maintain a mirror of
//! record state: an entry unchanged while the watermark advances past slot
//! `s` is the record's state at `s`. [`AccountUpdate::SyncInterrupted`]
//! still voids all previously delivered state.

mod channels;
mod syncer;
mod types;

pub use channels::{DlpSyncChannelsInit, DlpSyncChannelsRequester};
pub use syncer::DlpSyncer;
pub use types::{AccountUpdate, DlpSyncError, Pubkey, Slot};
