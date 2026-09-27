use std::sync::{atomic::AtomicU64, Arc};

use nucleus::shutdown::{Service, ShutdownManager, ShutdownReason};
use solana_pubkey::Pubkey;
use tokio::sync::mpsc;

use super::{session::Session, Event, Result, StreamConfig};
use crate::AccountSubscription;

/// One serialized change to a stream's logical account interest.
pub(crate) enum Command {
    /// Tracks an acknowledged WS account until its filter becomes eligible.
    Track { sub: AccountSubscription, gen: u64 },
    /// Ends logical interest immediately; remote removal waits for a rebuild.
    Remove(Pubkey),
    /// Sends the full filter only when aged additions or removals changed it.
    Rebuild,
}

/// Control handle for one shutdown-managed Yellowstone stream.
pub(crate) struct Client(mpsc::UnboundedSender<Command>);

impl Client {
    /// Starts one stream while sharing ordered events and the HTTP freshness watermark.
    pub(crate) fn new(
        id: usize,
        config: StreamConfig,
        authority: Pubkey,
        slot: Arc<AtomicU64>,
        events: mpsc::Sender<Event>,
        manager: &mut ShutdownManager,
    ) -> Result<Self> {
        let (commands, requests) = mpsc::unbounded_channel();
        let session = Session::new(id, config, authority, slot, events.clone())?;
        let mut shutdown = manager.handle(Service::ChainSyncGrpc(id));
        tokio::spawn(async move {
            let reason = tokio::select! {
                biased;
                _ = shutdown.signalled() => ShutdownReason::Signalled,
                result = session.run(requests) => match result {
                    Ok(()) => ShutdownReason::Unexpected,
                    Err(error) => ShutdownReason::Error(Box::new(error)),
                },
            };
            shutdown.terminate(reason);
        });
        Ok(Self(commands))
    }

    /// Queues a logical change without waiting for remote filter delivery.
    pub(crate) fn command(&self, command: Command) {
        let _ = self.0.send(command);
    }
}
