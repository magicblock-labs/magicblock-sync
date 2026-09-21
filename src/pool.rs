use std::time::Duration;

use ahash::AHashMap;
use tokio::{
    sync::mpsc::{self, Receiver, Sender, UnboundedSender},
    task::JoinHandle,
};

use crate::{
    connection::{Command, Session},
    Config, Connection, Error, Event, Pubkey, Reservation, Subscription,
};

/// Maximum events awaiting consumption across the pool.
const EVENT_CAP: usize = 8192;

struct Socket {
    /// Current incarnation; replacing the task advances its generation.
    id: Connection,
    /// Logically bounded by the subscription limit: each live reservation has at most
    /// one queued command, since release follows establishment and occurs at most once.
    commands: UnboundedSender<Command>,
    /// Aborted on drop so a replaced socket cannot outlive its pool entry.
    task: JoinHandle<()>,
    /// Live reservation IDs and accounts; this socket supplies their connection identity.
    reservations: AHashMap<u64, Pubkey>,
    /// Whether the pool has consumed this incarnation's `Connected` event.
    ready: bool,
    /// Retry delay for this attempt; reset when the pool observes connection success.
    backoff: Duration,
}

impl Socket {
    /// Starts an empty incarnation with a fresh command queue after the requested delay.
    fn spawn(id: Connection, config: &Config, events: Sender<Event>, backoff: Duration) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let session = Session::start(
            id,
            config.providers[id.provider].url.clone(),
            receiver,
            events,
            backoff,
        );
        let task = tokio::spawn(session);
        Self {
            id,
            commands,
            task,
            reservations: AHashMap::new(),
            ready: false,
            backoff,
        }
    }

    /// Requires observed connection success and an I/O task still accepting commands.
    fn healthy(&self) -> bool {
        self.ready && !self.commands.is_closed()
    }

    /// Pending, established, and releasing subscriptions all occupy hard capacity.
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
    /// Total live reservations; socket maps remain authoritative for admission and loss.
    reservations: usize,
    /// Capacity of all provisioned slots, including connecting and reconnecting sockets.
    capacity: usize,
    /// First socket considered next time, rotating first-eligible allocation.
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
            reservations: 0,
            capacity: 0,
            cursor: 0,
        };
        for provider in 0..pool.config.providers.len() {
            pool.open(provider);
        }
        pool
    }

    /// Reserves one account on the first eligible socket from a rotating cursor.
    /// Rotation approximates balance without moving existing subscriptions.
    /// Pending and releasing reservations count toward limits. The caller guarantees
    /// at most one live subscription per account; the pool does not deduplicate accounts.
    /// Success is admission only: wait for `Established` before fetching a snapshot.
    pub fn subscribe(&mut self, account: Pubkey) -> Result<Reservation, Error> {
        let len = self.sockets.len();
        let candidate = (self.cursor..len).chain(0..self.cursor).find(|&i| {
            let socket = &self.sockets[i];
            socket.available(self.config.providers[socket.id.provider].subs_per_connection)
        });
        let Some(index) = candidate else {
            self.grow();
            let full = self.reservations == self.capacity
                && self.sockets.len()
                    == self.config.providers.iter().map(|p| p.max_connections).sum::<usize>();
            if full {
                return Err(Error::Capacity);
            }
            return Err(Error::Unavailable);
        };
        // Two wire request IDs per reservation: subscribe is even, release is odd.
        self.sequence += 1;
        let socket = &mut self.sockets[index];
        let reservation = Reservation {
            account,
            connection: socket.id,
            id: self.sequence,
        };
        socket
            .commands
            .send(Command::Subscribe(reservation))
            .map_err(|_| Error::Unavailable)?;
        socket.reservations.insert(reservation.id, account);
        self.cursor = (index + 1) % len;
        let was_loaded = self.loaded();
        self.reservations += 1;
        if !was_loaded && self.loaded() {
            self.grow();
        }
        Ok(reservation)
    }

    /// Enqueues release, not a remote acknowledgement. Release each subscription at most
    /// once, without retries; obsolete identities and lost socket mailboxes are ignored.
    /// Capacity remains occupied until `Released` or `Dropped` is consumed via `next`.
    pub fn release(&mut self, subscription: Subscription) {
        let reservation = subscription.reservation;
        // Same-pool handles retain a valid slot; only reconnects invalidate its identity.
        let socket = &self.sockets[reservation.connection.index];
        if socket.id != reservation.connection {
            return;
        }
        // A closed mailbox means coverage is already lost; its Dropped event reports why.
        let _ = socket.commands.send(Command::Release(subscription));
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
            }
            Event::Released(Subscription { reservation, .. })
            | Event::Rejected { reservation, .. } => {
                // Correlated terminal responses precede loss and retire each reservation once.
                self.sockets[reservation.connection.index].reservations.remove(&reservation.id);
                self.reservations -= 1;
            }
            Event::Dropped { connection, reservations, .. } => {
                let socket = &mut self.sockets[connection.index];
                self.reservations -= socket.reservations.len();
                reservations.extend(
                    socket.reservations.drain().map(|(id, account)| Reservation {
                        account,
                        connection: socket.id,
                        id,
                    }),
                );
                let id = Connection {
                    generation: connection.generation + 1,
                    ..*connection
                };
                let delay =
                    (socket.backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                self.sockets[connection.index] =
                    Socket::spawn(id, &self.config, self.sender.clone(), delay);
            }
            Event::Established(_) | Event::Update { .. } => {}
        }
        if matches!(event, Event::Connected(_) | Event::Dropped { .. }) && self.loaded() {
            self.grow();
        }
        event
    }

    /// Fixed 75% pool-wide utilization, including capacity not yet ready for admission.
    fn loaded(&self) -> bool {
        self.reservations * 4 >= self.capacity * 3
    }

    /// Visits every provider once; added capacity does not truncate the growth round.
    fn grow(&mut self) {
        for provider in 0..self.config.providers.len() {
            self.grow_provider(provider);
        }
    }

    /// Adds at most one socket per healthy socket, bounded by the provider's limit.
    fn grow_provider(&mut self, provider: usize) {
        let mut count = 0;
        let mut healthy = 0usize;
        for socket in self.sockets.iter().filter(|s| s.id.provider == provider) {
            count += 1;
            // Only fresh attempts block another growth batch. Reconnects still
            // consume the hard limit, but cannot hold healthy capacity back.
            if !socket.ready && socket.id.generation == 0 {
                return;
            }
            if socket.healthy() {
                healthy += 1;
            }
        }
        let config = &self.config.providers[provider];
        // A full provider or one without healthy sockets naturally opens an empty batch.
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
        self.capacity += self.config.providers[provider].subs_per_connection;
    }
}
