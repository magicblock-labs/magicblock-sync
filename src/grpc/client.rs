use std::sync::{atomic::AtomicU64, Arc};

use solana_pubkey::Pubkey;
use tokio::{sync::mpsc, task::JoinHandle, time::Instant};

use super::{session::Session, Error, Event, StreamConfig};
use crate::AccountSubscription;

/// One serialized change to a stream's logical account interest.
pub(crate) enum Command {
    /// Tracks an acknowledged WS account until its filter becomes eligible.
    Track(AccountSubscription, u64, Instant),
    /// Ends logical interest immediately; remote removal waits for a rebuild.
    Remove(Pubkey),
    /// Sends the full filter only when aged additions or removals changed it.
    Rebuild,
}

/// Control handle and lifetime owner for one Yellowstone stream.
pub(crate) struct Client {
    /// Aborted when the synchronizer drops this handle.
    handle: JoinHandle<()>,
    /// Ordered stream commands.
    commands: mpsc::Sender<Command>,
}

impl Drop for Client {
    /// Stops stream I/O when its control handle is released.
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl Client {
    /// Starts one stream while sharing ordered events and the HTTP freshness watermark.
    pub(crate) fn new(
        id: usize,
        config: StreamConfig,
        authority: Pubkey,
        slot: Arc<AtomicU64>,
        events: mpsc::Sender<Event>,
    ) -> Result<Self, Error> {
        let (commands, requests) = mpsc::channel(COMMAND_CAPACITY);
        let session = Session::new(id, config, authority, slot, events.clone())?;
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = events.closed() => {},
                _ = session.run(requests) => {},
            }
        });
        Ok(Self { commands, handle })
    }

    /// Queues a logical change without waiting for remote filter delivery.
    pub(crate) async fn command(&self, command: Command) -> Result<(), Error> {
        self.commands.send(command).await.map_err(|_| Error::Closed)
    }
}

/// Maximum queued logical changes per Yellowstone stream.
const COMMAND_CAPACITY: usize = 8192;
/// Maximum queued account and lifecycle events from all streams.
pub(crate) const EVENT_CAPACITY: usize = 8192;
