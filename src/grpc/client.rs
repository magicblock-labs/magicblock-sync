use super::{session::Session, Config, Error, Event};
use solana_pubkey::Pubkey;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Membership change acknowledged after request delivery.
pub(super) struct SubscriptionUpdate {
    /// Accounts to retain alongside WebSocket coverage.
    pub(super) add: Vec<Pubkey>,
    /// Accounts to stop retaining; removal wins over addition.
    pub(super) remove: Vec<Pubkey>,
    /// Signals delivery, not remote coverage.
    pub(super) reply: oneshot::Sender<()>,
}

/// Last-handle lifetime control for the provider task.
struct ClientTask {
    /// Bounded membership-update queue.
    updates: mpsc::Sender<SubscriptionUpdate>,
    /// Aborted when all client handles are dropped.
    handle: JoinHandle<()>,
}

impl Drop for ClientTask {
    /// Stops the provider task when the final handle is released.
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Cloneable control handle for one provider's event stream.
#[derive(Clone)]
pub struct Client {
    /// Shared provider-task lifetime and update sender.
    task: Arc<ClientTask>,
}

impl Client {
    /// Starts on the current Tokio runtime. Use [`crate::websocket::Pool::slot`]
    /// to share freshness with HTTP and WebSockets, not as a replay checkpoint.
    pub fn new(
        config: Config,
        slot: Arc<AtomicU64>,
    ) -> Result<(Self, mpsc::Receiver<Event>), Error> {
        let (updates, requests) = mpsc::channel(UPDATE_CAPACITY);
        let (events, receiver) = mpsc::channel(EVENT_CAPACITY);
        let session = Session::new(config, slot, events.clone())?;
        let task = tokio::spawn(async move {
            tokio::select! {
                _ = events.closed() => {},
                _ = session.run(requests) => {},
            }
        });
        let task = Arc::new(ClientTask { updates, handle: task });
        let client = Self { task };
        Ok((client, receiver))
    }

    /// Sends a membership change without waiting for remote coverage. Removal
    /// wins if a key appears in both lists. Cancelling cannot retract an admitted
    /// change. On terminal failure this returns [`Error::Closed`], with the cause
    /// in [`Event::Disconnected`].
    pub async fn update(&self, add: Vec<Pubkey>, remove: Vec<Pubkey>) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.task
            .updates
            .send(SubscriptionUpdate { add, remove, reply })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)
    }
}

/// Maximum queued membership changes.
const UPDATE_CAPACITY: usize = 64;
/// Maximum queued account and lifecycle events.
const EVENT_CAPACITY: usize = 8192;
