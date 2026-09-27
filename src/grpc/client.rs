use super::{session::Session, Config, Error, Event};
use crate::AccountSubscription;
use derive_more::Deref;
use std::sync::{atomic::AtomicU64, Arc};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Last-handle lifetime control for the provider task.
pub struct ClientTask {
    /// Aborted when all client handles are dropped.
    handle: JoinHandle<()>,
    /// Bounded membership-update queue.
    updates: mpsc::Sender<SubscriptionUpdate>,
}

/// Cloneable control handle for one provider's event stream.
#[derive(Clone, Deref)]
pub struct Client(Arc<ClientTask>);

impl Drop for ClientTask {
    /// Stops the provider task when the final handle is released.
    fn drop(&mut self) {
        self.handle.abort();
    }
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
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = events.closed() => {},
                _ = session.run(requests) => {},
            }
        });
        let task = Arc::new(ClientTask { updates, handle });
        let client = Self(task);
        Ok((client, receiver))
    }

    /// Retains accounts and optional ProgramData targets without waiting for
    /// remote coverage. Cancelling cannot retract an admitted change.
    pub async fn retain(&self, add: Vec<AccountSubscription>) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.updates
            .send(SubscriptionUpdate { add, reply })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)
    }
}

/// Membership change acknowledged after request delivery.
pub(super) struct SubscriptionUpdate {
    /// Accounts to retain alongside WebSocket coverage.
    pub(super) add: Vec<AccountSubscription>,
    /// Signals delivery, not remote coverage.
    pub(super) reply: oneshot::Sender<()>,
}

/// Maximum queued membership changes.
const UPDATE_CAPACITY: usize = 64;
/// Maximum queued account and lifecycle events.
const EVENT_CAPACITY: usize = 8192;
