use tokio::sync::mpsc::Receiver;

use crate::syncer::SyncRequest;
use crate::types::{AccountUpdate, Pubkey, Slot};

/// Generic channels container for communicating with a `DlpSyncer`.
///
/// The `R` type parameter allows for different channel configurations
/// depending on usage context.
pub struct DlpSyncChannels<R> {
    pub(crate) requests: tokio::sync::mpsc::Sender<SyncRequest>,
    pub(crate) updates: R,
}

/// Initialized channel pair with both request and update sides.
pub type DlpSyncChannelsInit = DlpSyncChannels<Receiver<AccountUpdate>>;

/// Requester-only channel pair for sending subscription requests.
pub type DlpSyncChannelsRequester = DlpSyncChannels<()>;

impl DlpSyncChannelsRequester {
    /// Subscribe to updates for a delegation record.
    ///
    /// # Arguments
    ///
    /// * `record` - The pubkey of the delegation record to subscribe to.
    ///
    /// # Returns
    ///
    /// Returns the current slot number if the subscription was successful, or `None`
    /// if the sync service has terminated or the channel is closed.
    pub async fn subscribe(&self, record: Pubkey) -> Option<Slot> {
        let (slot_tx, rx) = tokio::sync::oneshot::channel();
        self.requests
            .send(SyncRequest::Subscribe { record, slot_tx })
            .await
            .ok()?;
        rx.await.ok()
    }

    /// Unsubscribe from a delegation record.
    ///
    /// # Arguments
    ///
    /// * `record` - The pubkey of the delegation record to unsubscribe from.
    ///
    /// # Returns
    ///
    /// Returns `Some(())` if the unsubscribe request was sent successfully,
    /// or `None` if the sync service has terminated or the channel is closed.
    pub async fn unsubscribe(&self, record: Pubkey) -> Option<()> {
        self.requests
            .send(SyncRequest::Unsubscribe(record))
            .await
            .ok()
    }
}

impl DlpSyncChannelsInit {
    /// Splits the initialized channels into separate requester and update receiver.
    ///
    /// # Returns
    ///
    /// A tuple of:
    /// - [`DlpSyncChannelsRequester`] for sending subscription requests
    /// - [`Receiver<AccountUpdate>`] for receiving updates
    pub fn split(self) -> (DlpSyncChannelsRequester, Receiver<AccountUpdate>) {
        let requester = DlpSyncChannelsRequester {
            requests: self.requests,
            updates: (),
        };
        (requester, self.updates)
    }
}
