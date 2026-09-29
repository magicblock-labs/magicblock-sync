use std::{
    collections::hash_map::Entry::Occupied,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use ahash::AHashMap;
use nucleus::shutdown::{Service, ShutdownManager, ShutdownReason};
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
    Config, Connection, Error, Event, Result,
};
use crate::AccountSubscription;
use solana_pubkey::Pubkey;

/// Subscription handle for a shutdown-managed WebSocket pool.
pub(crate) struct Pool {
    /// Bounded queue for caller subscription operations.
    commands: Sender<SubscriptionRequest>,
    /// Confirmed context-slot watermark retained across reconnects.
    slot: Arc<AtomicU64>,
}

/// Completion channel for one caller operation.
type Reply = oneshot::Sender<Result<()>>;

/// One account operation submitted to the registry.
enum SubscriptionRequest {
    /// Subscribes once and reports the server acknowledgement.
    Subscribe(AccountSubscription, Reply),
    /// Releases a subscription and reports the server outcome.
    Unsubscribe(Pubkey, Reply),
}

/// Per-account state that occupies socket capacity.
enum Subscription {
    /// Subscribe request awaiting a server acknowledgement.
    Pending {
        /// Caller waiting for the server acknowledgement.
        reply: Reply,
        /// Account and optional ProgramData target to publish on acknowledgement.
        account: AccountSubscription,
        /// Unsubscribe waiter if removal overtook the subscribe acknowledgement.
        cancel: Option<Reply>,
    },
    /// Acknowledged subscription shared by identical acquisition requests.
    Active {
        /// Provider subscription ID used for final release.
        remote: u64,
        /// Exact subscription identity; a different target cannot share it.
        account: AccountSubscription,
        /// Callers that still own this subscription across a lease handoff.
        owners: usize,
    },
    /// Release following an acknowledged or cancelled subscription.
    Releasing(Reply),
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
    /// Connecting or closed socket tasks cannot accept user subscriptions.
    fn healthy(&self) -> bool {
        self.ready && !self.commands.is_closed()
    }

    /// The internal `Clock` subscription consumes provider capacity too.
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

impl Pool {
    /// Starts connecting on the current Tokio runtime without waiting for
    /// readiness. The pool and its socket tasks stop on coordinated shutdown.
    /// Connection failures arrive as [`Event::Dropped`].
    pub fn new(config: Config, manager: &mut ShutdownManager) -> (Self, Receiver<Event>) {
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
        let mut shutdown = manager.handle(Service::ChainSyncWebSocket);
        tokio::spawn(async move {
            for provider in 0..registry.config.providers.len() {
                registry.open(provider, true);
            }
            tokio::select! {
                biased;
                _ = shutdown.signalled() => {},
                _ = events.closed() => {},
                _ = registry.run(requests, incoming) => {},
            }
            for socket in &mut registry.sockets {
                socket.task.abort();
                let _ = (&mut socket.task).await;
            }
            drop(registry);
            let reason = if shutdown.requested() {
                ShutdownReason::Signalled
            } else {
                ShutdownReason::Unexpected
            };
            shutdown.terminate(reason);
        });
        (Self { commands, slot }, receiver)
    }

    /// Shared confirmed-update watermark, initially zero. It is not the chain
    /// head; callers must not lower or otherwise modify it.
    pub fn slot(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.slot)
    }

    /// Subscribes until server acknowledgement, not an initial snapshot.
    /// `ChainSync` owns admission; identical active requests share one provider ID.
    /// The target is retained in queued updates, including those buffered before
    /// an unsubscribe completes.
    ///
    /// Capacity failures return immediately. Do not cancel: admitted work may
    /// complete after the caller stops waiting.
    pub(crate) async fn subscribe(&self, account: AccountSubscription) -> Result<()> {
        self.request(SubscriptionRequest::Subscribe, account).await
    }

    /// Releases one owner, removing the provider subscription after the last owner.
    /// Do not overlap ordinary subscribe and unsubscribe operations for the same key.
    ///
    /// Already-lost subscriptions are a no-op; buffered updates may still arrive.
    /// Connection loss returns [`Error::Disconnected`], with the cause in
    /// [`Event::Dropped`].
    pub(crate) async fn unsubscribe(&self, pubkey: Pubkey) -> Result<()> {
        self.request(SubscriptionRequest::Unsubscribe, pubkey).await
    }

    /// Waits for registry admission and the server's operation outcome.
    async fn request<T>(
        &self,
        make: impl FnOnce(T, Reply) -> SubscriptionRequest,
        value: T,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.commands.send(make(value, reply)).await.map_err(|_| Error::Closed)?;
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
    /// Serializes caller operations with socket outcomes before changing routes
    /// or waking waiting callers.
    async fn run(
        &mut self,
        mut requests: Receiver<SubscriptionRequest>,
        mut notices: UnboundedReceiver<Notice>,
    ) {
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { return };
                    match request {
                        SubscriptionRequest::Subscribe(account, reply) => {
                            if let Some(&index) = self.routes.get(&account.pubkey) {
                                // Two acquisition waves can share one acknowledged subscription
                                // while an incomplete primary is re-leased for its companion.
                                match self.sockets[index].accounts.get_mut(&account.pubkey) {
                                    Some(Subscription::Active { account: current, owners, .. })
                                        if *current == account =>
                                    {
                                        *owners += 1;
                                        let _ = reply.send(Ok(()));
                                    }
                                    _ => {
                                        let _ = reply.send(Err(Error::Unavailable));
                                    }
                                }
                            } else {
                                self.subscribe(account, reply);
                            }
                        }
                        SubscriptionRequest::Unsubscribe(pubkey, reply) => {
                            if self.last_owner(pubkey) {
                                // Queue logical removal before remote release so later
                                // buffered updates fail the worker's coverage check.
                                let _ = self.events.send(Event::Removed(pubkey)).await;
                            }
                            self.unsubscribe(pubkey, reply);
                        }
                    }
                }
                Some(notice) = notices.recv() => self.notice(notice).await,
            }
        }
    }

    /// Only the final active owner ends logical coverage.
    fn last_owner(&self, pubkey: Pubkey) -> bool {
        let Some(&index) = self.routes.get(&pubkey) else { return false };
        matches!(
            self.sockets[index].accounts.get(&pubkey),
            Some(Subscription::Active { owners: 1, .. })
        )
    }

    /// Reserves capacity before waiting for remote acknowledgement.
    fn subscribe(&mut self, account: AccountSubscription, reply: Reply) {
        let pubkey = account.pubkey;
        let index = match self.admit() {
            Ok(index) => index,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let socket = &mut self.sockets[index];
        if socket.commands.send(Command::Subscribe(account)).is_err() {
            let _ = reply.send(Err(Error::Unavailable));
            return;
        }
        socket.accounts.insert(
            pubkey,
            Subscription::Pending { reply, account, cancel: None },
        );
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
        match state {
            Subscription::Active { owners, .. } if *owners > 1 => {
                *owners -= 1;
                let _ = reply.send(Ok(()));
            }
            Subscription::Active { remote, .. } => {
                let remote = *remote;
                *state = Subscription::Releasing(reply);
                // If I/O has just stopped, its queued loss notice completes this waiter.
                let _ = socket.commands.send(Command::Unsubscribe { pubkey, remote });
            }
            Subscription::Pending { cancel, .. } => *cancel = Some(reply),
            Subscription::Releasing(_) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Finds ready capacity without queuing behind connection attempts.
    fn admit(&mut self) -> Result<usize> {
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
            }
            Notice::Acknowledged { connection, pubkey, result } => {
                // Acknowledgements do not trigger pool growth.
                return self.acknowledge(connection, pubkey, result).await;
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
                    .filter_map(|(pubkey, entry)| {
                        self.routes.remove(&pubkey);
                        let report = match entry {
                            Subscription::Pending { reply, cancel, .. } => {
                                // Cancelled subscriptions do not report a source loss.
                                let report = cancel.is_none();
                                let _ = reply.send(Err(Error::Disconnected));
                                if let Some(reply) = cancel {
                                    let _ = reply.send(Err(Error::Disconnected));
                                }
                                report
                            }
                            Subscription::Releasing(reply) => {
                                let _ = reply.send(Err(Error::Disconnected));
                                false
                            }
                            Subscription::Active { .. } => true,
                        };
                        report.then_some(pubkey)
                    })
                    .collect();
                // The failed task has finished publishing updates. Publish loss before replacing
                // it or accepting new user subscriptions, preserving this connection's event order.
                let _ = self.events.send(Event::Dropped { pubkeys, error }).await;
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
    async fn acknowledge(
        &mut self,
        connection: Connection,
        pubkey: Pubkey,
        result: Result<Option<u64>>,
    ) {
        let socket = &mut self.sockets[connection.index];
        let Occupied(mut entry) = socket.accounts.entry(pubkey) else { return };
        match result {
            Ok(Some(remote)) => {
                let Subscription::Pending { account, .. } = entry.get() else { return };
                let account = *account;
                let Subscription::Pending { reply, cancel, .. } =
                    entry.insert(Subscription::Active { remote, account, owners: 1 })
                else {
                    unreachable!()
                };
                if let Some(cancel) = cancel {
                    // A release raced the subscribe acknowledgement; never publish coverage.
                    *entry.get_mut() = Subscription::Releasing(cancel);
                    let _ = socket.commands.send(Command::Unsubscribe { pubkey, remote });
                } else {
                    let _ = self.events.send(Event::Acknowledged(account)).await;
                }
                let _ = reply.send(Ok(()));
            }
            result => {
                self.routes.remove(&pubkey);
                self.occupied -= 1;
                match entry.remove() {
                    Subscription::Pending { reply, cancel, .. } => {
                        let _ = reply.send(result.map(|_| ()));
                        if let Some(cancel) = cancel {
                            let _ = cancel.send(Ok(()));
                        }
                    }
                    Subscription::Releasing(reply) => {
                        let _ = reply.send(result.map(|_| ()));
                    }
                    Subscription::Active { .. } => unreachable!(),
                }
            }
        }
    }

    /// Starts an attempt with internal `Clock` ahead of user commands.
    fn spawn(&self, id: Connection, backoff: Duration, clock: bool) -> Socket {
        let (commands, receiver) = mpsc::unbounded_channel();
        if clock {
            let _ = commands.send(Command::Subscribe(AccountSubscription {
                pubkey: clock::ID,
                target: None,
            }));
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

    /// Tries every provider so one full or disconnected provider does not
    /// prevent healthy providers from adding capacity.
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

/// Maximum public events awaiting consumption across the pool.
const EVENT_CAP: usize = 8192;
