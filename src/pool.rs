use std::time::Duration;

use ahash::AHashMap;
use derive_more::Deref;
use tokio::{
    sync::mpsc::{self, error::TrySendError, Receiver, Sender},
    task::JoinHandle,
};

use crate::{
    connection::{Command, Session},
    Config, Connection, Error, Event, Pubkey, Reservation, Subscription,
};

/// Maximum commands awaiting processing on one socket.
pub(crate) const COMMAND_CAP: usize = 256;
/// Maximum events awaiting consumption across the pool.
const EVENT_CAP: usize = 8192;

#[derive(Deref)]
struct Socket {
    /// Current incarnation; replacing the task advances its generation.
    id: Connection,
    /// Bounded admission queue for this incarnation's I/O task.
    #[deref]
    commands: Sender<Command>,
    /// Aborted on drop so a replaced socket cannot outlive its pool entry.
    task: JoinHandle<()>,
    /// Reservations not yet released or lost, including pending and releasing ones.
    reservations: AHashMap<u64, Admission>,
    /// Whether the pool has consumed this incarnation's `Connected` event.
    ready: bool,
    /// Retry delay for this attempt; reset when the pool observes connection success.
    backoff: Duration,
}

impl Socket {
    /// Starts an empty incarnation with a fresh command queue after the requested delay.
    fn spawn(id: Connection, config: &Config, events: Sender<Event>, backoff: Duration) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAP);
        let task = tokio::spawn(Session::start(
            id,
            config.providers[id.provider].url.clone(),
            receiver,
            events,
            backoff,
        ));
        Self {
            id,
            commands,
            task,
            reservations: AHashMap::new(),
            ready: false,
            backoff,
        }
    }

    /// Queues a command without waiting, distinguishing pressure from lost admission.
    fn enqueue(&self, command: Command) -> Result<(), Error> {
        self.commands.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => Error::Busy,
            TrySendError::Closed(_) => Error::Unavailable,
        })
    }

    /// Requires observed connection success and an I/O task still accepting commands.
    fn healthy(&self) -> bool {
        self.ready && !self.is_closed()
    }

    /// Checks subscription headroom; command-queue capacity is checked separately.
    fn available(&self, limit: usize) -> bool {
        self.healthy() && self.reservations.len() < limit
    }
}

impl Drop for Socket {
    /// Stops socket I/O instead of detaching the task when its handle is dropped.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Local admission state retained until acknowledgement or socket loss frees capacity.
struct Admission {
    /// Identity used to reject stale handles and correlate lifecycle events.
    reservation: Reservation,
    /// Whether release has been queued successfully; further releases are idempotent.
    releasing: bool,
}

/// Single-owner subscription registry. There is one I/O task per socket, not per account.
/// Dropping the registry aborts its tasks and closes their sockets.
/// Reservation IDs, their paired wire request IDs, and connection generations are
/// assumed never to exhaust their `u64` range during a pool's lifetime.
pub struct Pool {
    /// Caller-provided limits and connection policy, fixed for this pool's lifetime.
    config: Config,
    /// Stable slots shared by all providers; reconnects replace entries in place.
    sockets: Vec<Socket>,
    /// Bounded delivery queue; consuming events also advances pool bookkeeping.
    events: Receiver<Event>,
    /// Shared with socket tasks and retained so the receiver never ends unexpectedly.
    sender: Sender<Event>,
    /// Last allocated reservation ID; IDs are never reused within this pool.
    sequence: u64,
    /// First socket considered next time, rotating allocation among equal-load ties.
    cursor: usize,
}

impl Pool {
    /// Starts one connection attempt per provider on the current Tokio runtime.
    /// Network failures arrive as `Dropped` events and retry with capped backoff.
    /// The caller must satisfy [`Config`]'s requirements; construction does not validate them.
    pub fn new(config: Config) -> Self {
        let (sender, events) = mpsc::channel(EVENT_CAP);
        let mut pool = Self {
            config,
            sockets: Vec::new(),
            events,
            sender,
            sequence: 0,
            cursor: 0,
        };
        for provider in 0..pool.config.providers.len() {
            pool.open(provider);
        }
        pool
    }

    /// Reserves one account on a least-loaded ready socket, rotating equal-load ties.
    /// Pending and releasing reservations count toward limits. The caller guarantees
    /// at most one live subscription per account; the pool does not deduplicate accounts.
    /// Success is admission only: wait for `Established` before fetching a snapshot.
    pub fn subscribe(&mut self, account: Pubkey) -> Result<Reservation, Error> {
        let len = self.sockets.len();
        let candidate = (0..len)
            .map(|offset| (self.cursor + offset) % len)
            .filter(|&i| {
                let socket = &self.sockets[i];
                socket.available(self.config.providers[socket.id.provider].subs_per_connection)
                    && socket.capacity() > 0
            })
            .min_by_key(|&i| self.sockets[i].reservations.len());
        let Some(index) = candidate else {
            let full = self.config.providers.iter().enumerate().all(|(provider, config)| {
                let mut sockets = self.sockets.iter().filter(|s| s.id.provider == provider);
                sockets.clone().count() == config.max_connections
                    && sockets.all(|s| s.reservations.len() == config.subs_per_connection)
            });
            if full {
                return Err(Error::Capacity);
            }
            let busy = self
                .sockets
                .iter()
                .any(|s| s.available(self.config.providers[s.id.provider].subs_per_connection));
            return Err(if busy { Error::Busy } else { Error::Unavailable });
        };
        // Two wire request IDs per reservation: subscribe is even, release is odd.
        self.sequence += 1;
        let socket = &mut self.sockets[index];
        let reservation = Reservation {
            account,
            connection: socket.id,
            id: self.sequence,
        };
        socket.enqueue(Command::Subscribe(reservation))?;
        socket
            .reservations
            .insert(reservation.id, Admission { reservation, releasing: false });
        self.cursor = (index + 1) % len;
        self.grow(reservation.connection.provider);
        Ok(reservation)
    }

    /// Requests release of established coverage. Idempotent while releasing.
    /// Capacity is retained until acknowledgement or socket loss. A stale handle never
    /// releases a newer reservation for the same account. `Busy` leaves it unchanged.
    pub fn release(&mut self, subscription: Subscription) -> Result<(), Error> {
        let reservation = subscription.reservation;
        let socket = self.sockets.get_mut(reservation.connection.index).ok_or(Error::Stale)?;
        if socket.id != reservation.connection {
            return Err(Error::Stale);
        }
        let admission = socket.reservations.get(&reservation.id).ok_or(Error::Stale)?;
        if admission.releasing {
            return Ok(());
        }
        socket.enqueue(Command::Release(subscription))?;
        socket
            .reservations
            .get_mut(&reservation.id)
            .expect("reservation is current")
            .releasing = true;
        Ok(())
    }

    /// Advances the registry and returns the next event. Cancellation-safe.
    /// Call continuously: a full event queue pauses socket processing until drained.
    /// There is no automatic account resubscription.
    pub async fn next(&mut self) -> Event {
        // The registry retains a sender; only its owner can close this receiver.
        let mut event = self.events.recv().await.expect("registry owns event sender");
        match &mut event {
            Event::Connected(connection) => {
                let socket = &mut self.sockets[connection.index];
                socket.ready = true;
                socket.backoff = Duration::ZERO;
                self.grow(connection.provider);
            }
            Event::Released(Subscription { reservation, .. })
            | Event::Rejected { reservation, .. } => {
                self.sockets[reservation.connection.index].reservations.remove(&reservation.id);
            }
            Event::Dropped { connection, reservations, .. } => {
                let socket = &mut self.sockets[connection.index];
                reservations.extend(
                    socket.reservations.drain().map(|(_, admission)| admission.reservation),
                );
                let id = Connection {
                    generation: connection.generation + 1,
                    ..*connection
                };
                let delay =
                    (socket.backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                self.sockets[connection.index] =
                    Socket::spawn(id, &self.config, self.sender.clone(), delay);
                self.grow(connection.provider);
            }
            Event::Established(_) | Event::Update { .. } => {}
        }
        event
    }

    /// Adds a bounded connection batch when healthy occupancy warrants more capacity.
    fn grow(&mut self, provider: usize) {
        let mut count = 0;
        let mut healthy = 0usize;
        let mut load = 0usize;
        for socket in self.sockets.iter().filter(|s| s.id.provider == provider) {
            count += 1;
            // Only fresh attempts block another growth batch. Reconnects still
            // consume the hard limit, but cannot hold healthy capacity back.
            if !socket.ready && socket.id.generation == 0 {
                return;
            }
            if socket.healthy() {
                healthy += 1;
                load += socket.reservations.len();
            }
        }
        let config = &self.config.providers[provider];
        // With no healthy sockets, existing reconnect attempts restore capacity.
        if healthy == 0 || count == config.max_connections {
            return;
        }
        let capacity = healthy * config.subs_per_connection;
        // 25%, 37.5%, 50%, 62.5%, then 75% occupancy as the pool doubles.
        let eighths = (2 + healthy.ilog2()).min(6) as usize;
        if load * 8 < capacity * eighths {
            return;
        }
        let batch = healthy.min(config.max_connections - count);
        for _ in 0..batch {
            self.open(provider);
        }
    }

    /// Allocates a fresh socket slot and starts its first connection attempt immediately.
    fn open(&mut self, provider: usize) {
        let id = Connection {
            provider,
            index: self.sockets.len(),
            generation: 0,
        };
        let socket = Socket::spawn(id, &self.config, self.sender.clone(), Duration::ZERO);
        self.sockets.push(socket);
    }
}
