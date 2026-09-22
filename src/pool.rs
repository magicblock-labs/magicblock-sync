use std::{
    collections::hash_map::Entry::Occupied,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use ahash::AHashMap;
use solana_sdk_ids::sysvar::clock;
use tokio::{
    sync::{
        mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    task::JoinHandle,
};

use crate::{
    connection::{Command, Notice, Session, COMMAND_CAP},
    Config, Connection, Error, Event, Pubkey,
};

/// Maximum events awaiting consumption across the pool.
const EVENT_CAP: usize = 8192;

/// Completes an operation only after registry bookkeeping reflects its outcome.
type Reply = oneshot::Sender<Result<(), Error>>;

/// One caller operation; operations for the same pubkey must not overlap.
struct Request {
    /// Account to subscribe to or unsubscribe from.
    pubkey: Pubkey,
    /// True requests subscribe; false requests unsubscribe.
    subscribe: bool,
    /// Waiter for admission failure or the server's acknowledgement.
    reply: Reply,
}

/// Subscription state that occupies capacity until rejection, unsubscribe acknowledgement, or connection loss.
enum Subscription {
    /// One subscribe or unsubscribe awaiting acknowledgement; capacity remains occupied.
    Pending(Reply),
    /// Acknowledged subscription identified by the provider's subscription ID.
    Active(u64),
}

/// One connection-pool entry; its allocated capacity is retained across reconnect attempts.
struct Socket {
    /// Current connection identity; replacing the task advances its generation.
    id: Connection,
    /// Logically bounded by capacity and the caller's non-overlapping-operation contract.
    commands: UnboundedSender<Command>,
    /// Aborted on drop so a replaced socket cannot outlive its pool entry.
    task: JoinHandle<()>,
    /// User subscription states and pending replies for this connection.
    accounts: AHashMap<Pubkey, Subscription>,
    /// Whether the registry has observed connection success.
    ready: bool,
    /// Whether this pool entry maintains its provider's internal Clock subscription.
    clock: bool,
    /// Retry delay, reset when the registry observes connection success.
    backoff: Duration,
}

impl Socket {
    /// Requires observed connection success and an I/O task still accepting commands.
    fn healthy(&self) -> bool {
        self.ready && !self.commands.is_closed()
    }

    /// Clock, pending, established, and releasing subscriptions all occupy capacity.
    fn occupied(&self) -> usize {
        self.accounts.len() + usize::from(self.clock)
    }
}

impl Drop for Socket {
    /// Stops socket I/O instead of detaching the task when its handle is dropped.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Commands and task lifetime shared by pool handles; never retained by the registry.
struct Shared {
    /// Bounded admission queue, shared by all pool handles.
    commands: Sender<Request>,
    /// Highest observed confirmed Solana context slot, retained across reconnects.
    slot: Arc<AtomicU64>,
    /// Dropping the last pool handle aborts the registry and therefore every socket task.
    task: JoinHandle<()>,
}

impl Drop for Shared {
    /// Also stops a registry blocked on delivery to a slow event consumer.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Cloneable subscription client with one background registry and one task per socket.
/// Consume the separate event receiver concurrently with operations: a full event queue
/// applies backpressure and can delay acknowledgements. Operations for a pubkey must
/// not overlap, and Clock is reserved for internal use. Dropping all pool handles or
/// closing the event receiver stops the pool.
#[derive(Clone)]
pub struct Pool {
    /// Shared ownership without per-account locks or tasks.
    shared: Arc<Shared>,
}

impl Pool {
    /// Starts one connection attempt per provider on the current Tokio runtime.
    /// Construction does not wait for readiness or validate [`Config`]'s requirements.
    /// Network failures arrive as `Dropped` events and retry with capped backoff.
    pub fn new(config: Config) -> (Self, Receiver<Event>) {
        let (commands, requests) = mpsc::channel(COMMAND_CAP);
        let (events, receiver) = mpsc::channel(EVENT_CAP);
        let (notices, incoming) = mpsc::unbounded_channel();
        let slot = Arc::new(AtomicU64::new(0));
        let mut registry = Registry {
            config,
            sockets: Vec::new(),
            routes: AHashMap::new(),
            events: events.clone(),
            notices,
            occupied: 0,
            capacity: 0,
            cursor: 0,
            slot: Arc::clone(&slot),
        };
        let task = tokio::spawn(async move {
            for provider in 0..registry.config.providers.len() {
                registry.open(provider, true);
            }
            tokio::select! {
                _ = events.closed() => {},
                _ = registry.run(requests, incoming) => {},
            }
        });
        (
            Self {
                shared: Arc::new(Shared { commands, slot, task }),
            },
            receiver,
        )
    }

    /// Highest observed confirmed Solana context slot from account updates, initially zero.
    /// This watermark is not a guarantee of the current chain head.
    /// Callers must not lower or otherwise modify this watermark.
    pub fn slot(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.shared.slot)
    }

    /// Subscribes to an account and returns after server acknowledgement, not an initial snapshot.
    /// The caller guarantees no existing subscription or pending operation for this pubkey.
    /// Clock is reserved for internal use. Admission fails with `Unavailable` or `Capacity`
    /// rather than waiting for a ready socket. Do not cancel this future; admitted work
    /// completes even without a waiter.
    pub async fn subscribe(&self, pubkey: Pubkey) -> Result<(), Error> {
        self.request(pubkey, true).await
    }

    /// Unsubscribes from an account and returns after acknowledgement and capacity reclamation.
    /// Call only after successful subscribe, without overlapping operations or cancellation.
    /// Clock is reserved for internal use. A subscription already removed by connection loss is
    /// a successful no-op. Buffered updates may arrive after this returns. Socket loss
    /// during the operation returns `Disconnected`; `Dropped` retains the cause.
    pub async fn unsubscribe(&self, pubkey: Pubkey) -> Result<(), Error> {
        self.request(pubkey, false).await
    }

    /// Separates command admission from acknowledged completion.
    async fn request(&self, pubkey: Pubkey, subscribe: bool) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.shared
            .commands
            .send(Request { pubkey, subscribe, reply })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

/// Subscription routing and capacity registry; updates bypass this task entirely.
struct Registry {
    /// Fixed provider limits and connection policy.
    config: Config,
    /// Connection-pool entries replaced at the same vector index on reconnect.
    sockets: Vec<Socket>,
    /// Pubkey-to-socket index; lifecycle state lives only in the socket's entry.
    routes: AHashMap<Pubkey, usize>,
    /// Public lifecycle events share the sockets' direct update queue.
    events: Sender<Event>,
    /// Internal lifecycle delivery is bounded logically by admitted work and socket count.
    notices: UnboundedSender<Notice>,
    /// Total occupied capacity, including Clock and pending operations.
    occupied: usize,
    /// Allocated subscription capacity, including connections being opened or reconnected.
    capacity: usize,
    /// First socket considered by rotating admission.
    cursor: usize,
    /// Shared minimum Solana context slot for HTTP fetches, retained across reconnects.
    slot: Arc<AtomicU64>,
}

impl Registry {
    /// Drives control independently of whether a caller is awaiting an operation.
    async fn run(
        &mut self,
        mut requests: Receiver<Request>,
        mut notices: UnboundedReceiver<Notice>,
    ) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    let Request { pubkey, subscribe, reply } = request;
                    if subscribe {
                        self.subscribe(pubkey, reply);
                    } else {
                        self.unsubscribe(pubkey, reply);
                    }
                }
                Some(notice) = notices.recv() => self.notice(notice).await,
            }
        }
    }

    /// Reserves subscription capacity; only server acknowledgement confirms an active subscription.
    fn subscribe(&mut self, pubkey: Pubkey, reply: Reply) {
        let index = match self.admit() {
            Ok(index) => index,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let socket = &mut self.sockets[index];
        if socket.commands.send(Command::Subscribe(pubkey)).is_err() {
            let _ = reply.send(Err(Error::Unavailable));
            return;
        }
        socket.accounts.insert(pubkey, Subscription::Pending(reply));
        self.routes.insert(pubkey, index);
        self.cursor = (index + 1) % self.sockets.len();
        let needed_growth = self.should_grow();
        self.occupied += 1;
        if !needed_growth && self.should_grow() {
            self.grow();
        }
    }

    /// Keeps capacity occupied until unsubscribe is acknowledged or the socket is lost.
    fn unsubscribe(&mut self, pubkey: Pubkey, reply: Reply) {
        let Some(&index) = self.routes.get(&pubkey) else {
            // Connection loss can remove the subscription before the caller unsubscribes.
            let _ = reply.send(Ok(()));
            return;
        };
        let socket = &mut self.sockets[index];
        let Some(state) = socket.accounts.get_mut(&pubkey) else { return };
        let Subscription::Active(remote) = state else { return };
        let remote = *remote;
        *state = Subscription::Pending(reply);
        // If I/O has just stopped, its queued loss notice completes this waiter.
        let _ = socket.commands.send(Command::Unsubscribe { pubkey, remote });
    }

    /// Chooses ready capacity without queuing admission behind connection attempts.
    fn admit(&mut self) -> Result<usize, Error> {
        let len = self.sockets.len();
        if let Some(index) = (self.cursor..len).chain(0..self.cursor).find(|&i| {
            let socket = &self.sockets[i];
            socket.healthy()
                && socket.occupied() < self.config.providers[socket.id.provider].subs_per_connection
        }) {
            return Ok(index);
        }
        self.grow();
        let full = self.occupied == self.capacity
            && self.sockets.len()
                == self.config.providers.iter().map(|p| p.max_connections).sum::<usize>();
        Err(if full { Error::Capacity } else { Error::Unavailable })
    }

    /// Applies lifecycle outcomes before waking callers or reporting connection loss.
    async fn notice(&mut self, notice: Notice) {
        match notice {
            Notice::Connected(connection) => {
                let socket = &mut self.sockets[connection.index];
                socket.ready = true;
                socket.backoff = Duration::ZERO;
                let _ = self.events.send(Event::Connected(connection)).await;
            }
            Notice::Acknowledged { connection, pubkey, result } => {
                // Acknowledgements do not trigger pool growth.
                return self.acknowledge(connection, pubkey, result);
            }
            Notice::Dropped { connection, error } => {
                let socket = &mut self.sockets[connection.index];
                self.occupied -= socket.occupied();
                let clock = socket.clock;
                let delay =
                    (socket.backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                let pubkeys = socket
                    .accounts
                    .drain()
                    .map(|(pubkey, entry)| {
                        self.routes.remove(&pubkey);
                        if let Subscription::Pending(reply) = entry {
                            let _ = reply.send(Err(Error::Disconnected));
                        }
                        pubkey
                    })
                    .collect();
                // The failed task has finished publishing updates. Publish loss before replacing
                // it or accepting new user subscriptions, preserving this connection's event order.
                let _ = self.events.send(Event::Dropped { connection, pubkeys, error }).await;
                let id = Connection {
                    generation: connection.generation + 1,
                    ..connection
                };
                let replacement = self.spawn(id, delay, clock);
                self.occupied += replacement.occupied();
                self.sockets[connection.index] = replacement;
            }
        }
        if self.should_grow() {
            self.grow();
        }
    }

    /// Commits the server's outcome before completing the caller's operation.
    fn acknowledge(
        &mut self,
        connection: Connection,
        pubkey: Pubkey,
        result: Result<Option<u64>, Error>,
    ) {
        let socket = &mut self.sockets[connection.index];
        let Occupied(mut entry) = socket.accounts.entry(pubkey) else { return };
        let pending = match result.as_ref() {
            Ok(Some(remote)) => entry.insert(Subscription::Active(*remote)),
            // Subscribe rejection and unsubscribe acknowledgement both free subscription capacity.
            _ => {
                self.routes.remove(&pubkey);
                self.occupied -= 1;
                entry.remove()
            }
        };
        let Subscription::Pending(reply) = pending else { return };
        let _ = reply.send(result.map(|_| ()));
    }

    /// Starts a connection attempt with Clock queued ahead of every user command.
    fn spawn(&self, id: Connection, backoff: Duration, clock: bool) -> Socket {
        let (commands, receiver) = mpsc::unbounded_channel();
        if clock {
            let _ = commands.send(Command::Subscribe(clock::ID));
        }
        let task = tokio::spawn(Session::start(
            id,
            self.config.providers[id.provider].url.clone(),
            receiver,
            self.events.clone(),
            self.notices.clone(),
            backoff,
            Arc::clone(&self.slot),
        ));
        Socket {
            id,
            commands,
            task,
            accounts: AHashMap::new(),
            ready: false,
            clock,
            backoff,
        }
    }

    /// Fixed 75% pool-wide utilization, including capacity not yet ready for admission.
    fn should_grow(&self) -> bool {
        self.occupied * 4 >= self.capacity * 3
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
            self.open(provider, false);
        }
    }

    /// Adds a connection-pool entry and starts its first connection attempt immediately.
    fn open(&mut self, provider: usize, clock: bool) {
        let id = Connection {
            provider,
            index: self.sockets.len(),
            generation: 0,
        };
        let socket = self.spawn(id, Duration::ZERO, clock);
        self.occupied += socket.occupied();
        self.sockets.push(socket);
        self.capacity += self.config.providers[provider].subs_per_connection;
    }
}
