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

use super::{
    session::{Command, Notice, Session, COMMAND_CAP},
    Config, Connection, Error, Event,
};
use solana_pubkey::Pubkey;

/// Maximum public events awaiting consumption across the pool.
const EVENT_CAP: usize = 8192;

/// Completion channel for one caller operation.
type Reply = oneshot::Sender<Result<(), Error>>;

/// One account operation submitted to the registry.
struct SubscriptionRequest {
    /// Account whose subscription state changes.
    pubkey: Pubkey,
    /// Whether this is subscribe rather than unsubscribe.
    subscribe: bool,
    /// Receives admission failure or server acknowledgement.
    reply: Reply,
}

/// Per-account state that occupies socket capacity.
enum Subscription {
    /// Operation awaiting a server acknowledgement.
    Pending(Reply),
    /// Acknowledged subscription with its provider ID.
    Active(u64),
}

/// Pool entry whose capacity remains allocated across reconnects.
struct Socket {
    /// Current attempt identity; reconnect advances its generation.
    id: Connection,
    /// Commands for the current socket task.
    commands: UnboundedSender<Command>,
    /// Aborted when this entry is replaced or dropped.
    task: JoinHandle<()>,
    /// User subscription states and pending replies.
    accounts: AHashMap<Pubkey, Subscription>,
    /// Whether the registry observed connection readiness.
    ready: bool,
    /// Whether this entry maintains the internal `Clock` subscription.
    clock: bool,
    /// Reconnect delay reset after observed readiness.
    backoff: Duration,
}

impl Socket {
    /// Requires both observed readiness and an open command channel.
    fn healthy(&self) -> bool {
        self.ready && !self.commands.is_closed()
    }

    /// Counts user and internal `Clock` subscriptions against capacity.
    fn occupied(&self) -> usize {
        self.accounts.len() + usize::from(self.clock)
    }
}

impl Drop for Socket {
    /// Stops socket I/O when the entry is replaced or the pool ends.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Shared lifetime control without a registry-owned reference cycle.
struct PoolTask {
    /// Bounded queue for caller subscription operations.
    commands: Sender<SubscriptionRequest>,
    /// Confirmed context-slot watermark retained across reconnects.
    slot: Arc<AtomicU64>,
    /// Aborted when the final pool handle drops.
    handle: JoinHandle<()>,
}

impl Drop for PoolTask {
    /// Stops registry and socket tasks with the last public handle.
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Cloneable subscription handle. The pool stops when all handles or its event
/// receiver are dropped.
#[derive(Clone)]
pub struct Pool {
    /// Shared registry lifetime and command sender.
    task: Arc<PoolTask>,
}

impl Pool {
    /// Starts connecting on the current Tokio runtime without waiting for
    /// readiness. Connection failures arrive as [`Event::Dropped`].
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
                task: Arc::new(PoolTask { commands, slot, handle: task }),
            },
            receiver,
        )
    }

    /// Shared confirmed-update watermark, initially zero. It is not the chain
    /// head; callers must not lower or otherwise modify it.
    pub fn slot(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.task.slot)
    }

    /// Subscribes until server acknowledgement, not an initial snapshot. The key
    /// must have no existing subscription or pending operation; `Clock` is reserved.
    ///
    /// Capacity failures return immediately. Do not cancel: admitted work may
    /// complete after the caller stops waiting.
    pub async fn subscribe(&self, pubkey: Pubkey) -> Result<(), Error> {
        self.request(pubkey, true).await
    }

    /// Releases an acknowledged subscription. Do not overlap or cancel operations
    /// for the same key; `Clock` is reserved.
    ///
    /// Already-lost subscriptions are a no-op; buffered updates may still arrive.
    /// Connection loss returns [`Error::Disconnected`], with the cause in
    /// [`Event::Dropped`].
    pub async fn unsubscribe(&self, pubkey: Pubkey) -> Result<(), Error> {
        self.request(pubkey, false).await
    }

    /// Waits for registry admission and the server's operation outcome.
    async fn request(&self, pubkey: Pubkey, subscribe: bool) -> Result<(), Error> {
        let (reply, result) = oneshot::channel();
        self.task
            .commands
            .send(SubscriptionRequest { pubkey, subscribe, reply })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

/// Owns subscription routing and capacity accounting for the pool.
struct Registry {
    /// Provider limits and reconnect policy.
    config: Config,
    /// Entries retain stable indices across reconnects.
    sockets: Vec<Socket>,
    /// Pubkey to socket index; lifecycle state lives in that entry.
    routes: AHashMap<Pubkey, usize>,
    /// Public updates and lifecycle events share this queue.
    events: Sender<Event>,
    /// Socket outcomes arrive independently of public updates.
    notices: UnboundedSender<Notice>,
    /// Capacity occupied by user operations and internal `Clock` subscriptions.
    occupied: usize,
    /// Capacity allocated to all entries, including connecting sockets.
    capacity: usize,
    /// Next socket considered for admission.
    cursor: usize,
    /// Shared minimum confirmed slot for HTTP snapshots.
    slot: Arc<AtomicU64>,
}

impl Registry {
    /// Processes caller commands and socket outcomes in one ownership task.
    async fn run(
        &mut self,
        mut requests: Receiver<SubscriptionRequest>,
        mut notices: UnboundedReceiver<Notice>,
    ) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    let SubscriptionRequest { pubkey, subscribe, reply } = request;
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

    /// Reserves capacity before waiting for remote acknowledgement.
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

    /// Keeps capacity occupied until acknowledgement or socket loss.
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

    /// Finds ready capacity without queuing behind connection attempts.
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

    /// Applies lifecycle outcomes before waking callers or reporting loss.
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

    /// Commits the server outcome before completing the caller's operation.
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

    /// Starts an attempt with internal `Clock` ahead of user commands.
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

    /// Requests more sockets at 75% of allocated pool-wide capacity.
    fn should_grow(&self) -> bool {
        self.occupied * 4 >= self.capacity * 3
    }

    /// Gives each configured provider one growth opportunity.
    fn grow(&mut self) {
        for provider in 0..self.config.providers.len() {
            self.grow_provider(provider);
        }
    }

    /// Adds up to one new socket per healthy socket within provider limits.
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

    /// Allocates a pool entry and starts its first connection attempt.
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
