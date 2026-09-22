use super::{session::Session, Config, Error, Event};
use crate::Pubkey;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// One batched membership change. Removal wins when a key occurs in both lists.
pub(super) struct SubscriptionUpdate {
    /// Retained accounts to promote without replacing their WebSocket subscriptions.
    pub(super) add: Vec<Pubkey>,
    /// Accounts no longer retained by orchestration.
    pub(super) remove: Vec<Pubkey>,
    /// Acknowledges request delivery, not remote coverage.
    pub(super) reply: oneshot::Sender<()>,
}

/// Last-handle lifetime control; the task does not hold this Arc itself.
struct ClientTask {
    /// Bounded control queue, outside the initial-fetch path.
    updates: mpsc::Sender<SubscriptionUpdate>,
    /// Aborted when all handles drop, including if event delivery is blocked.
    handle: JoinHandle<()>,
}

impl Drop for ClientTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Cloneable control handle for one provider and an independent ordered event receiver.
#[derive(Clone)]
pub struct Client {
    /// Keeps the provider task alive and admits subscription updates.
    task: Arc<ClientTask>,
}

impl Client {
    /// Starts on the current Tokio runtime. Pass [`crate::websocket::Pool::slot`] to share freshness
    /// with HTTP/WebSockets. The watermark is not used as a replay checkpoint.
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

    /// Sends one batch without waiting for server-side establishment.
    /// Duplicate additions/removals are harmless; removal wins within a batch.
    /// Cancellation does not retract an admitted change. On failure, the terminal
    /// [`Event::Disconnected`] carries the cause and this call returns [`Error::Closed`].
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

/// Membership batches awaiting delivery.
const UPDATE_CAPACITY: usize = 64;
/// Account and lifecycle events awaiting consumption.
const EVENT_CAPACITY: usize = 8192;
