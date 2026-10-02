use std::time::Duration;

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
    Config, ConnectionId, Error, Event, Result,
};
use crate::metrics::{self, Transport};
use crate::AccountSubscription;
use solana_pubkey::Pubkey;
use tracing::{info, warn};

/// Subscription handle for a shutdown-managed WebSocket pool.
pub(crate) struct Pool {
    /// Bounded queue for account subscription requests and cache evictions.
    commands: Sender<SubscriptionRequest>,
}

type Reply = oneshot::Sender<Result<()>>;

/// One account operation submitted to the registry.
enum SubscriptionRequest {
    /// Subscribes once and reports the server acknowledgement.
    Subscribe(AccountSubscription, Reply),
    /// Unsubscribes and reports the server outcome.
    Unsubscribe(Pubkey, Reply),
    /// Unsubscribes all accounts that materialize one cached Engine target.
    Evict(Pubkey),
}

/// Per-account state that occupies socket capacity.
enum Subscription {
    /// Subscribe request awaiting a server acknowledgement.
    Pending {
        reply: Reply,
        /// Account and optional ProgramData target to publish on acknowledgement.
        account: AccountSubscription,
        /// Unsubscribe waiter if removal overtook the subscribe acknowledgement.
        cancel: Option<Reply>,
    },
    /// Acknowledged subscription shared by identical acquisition requests.
    Active {
        /// Provider subscription ID used to unsubscribe.
        remote: u64,
        /// Exact subscription identity; a different target cannot share it.
        account: AccountSubscription,
        /// Unreleased subscription references; the last release unsubscribes.
        refs: usize,
    },
    /// Unsubscribe awaiting acknowledgement or socket loss.
    Unsubscribing(Reply),
}

/// Pool entry whose capacity remains allocated across reconnects.
struct Socket {
    /// Current attempt identity; reconnect advances its generation.
    id: ConnectionId,
    commands: UnboundedSender<Command>,
    /// Aborted when this entry is replaced or dropped.
    task: JoinHandle<()>,
    /// Requested account subscriptions and pending replies; excludes internal `Clock`.
    accounts: AHashMap<Pubkey, Subscription>,
    ready: bool,
    clock: bool,
    /// Reconnect delay reset after observed readiness.
    backoff: Duration,
}

impl Socket {
    fn healthy(&self) -> bool {
        self.ready && !self.commands.is_closed()
    }

    /// The internal `Clock` subscription consumes provider capacity too.
    fn occupied(&self) -> usize {
        self.accounts.len() + usize::from(self.clock)
    }
}

impl Drop for Socket {
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
        let mut registry = Registry {
            config,
            sockets: Vec::new(),
            routes: AHashMap::new(),
            events: events.clone(),
            notices,
            occupied: 0,
            capacity: 0,
            cursor: 0,
        };
        let mut shutdown = manager.handle(Service::ChainSyncWebSocket);
        tokio::spawn(async move {
            for provider in 0..registry.config.providers.len() {
                registry.open(provider, true);
            }
            tokio::select! {
                _ = shutdown.signalled() => {},
                _ = events.closed() => {},
                _ = registry.run(requests, incoming) => {},
            }
            for socket in &mut registry.sockets {
                socket.task.abort();
                let _ = (&mut socket.task).await;
            }
            drop(registry);
            shutdown.terminate(ShutdownReason::Signalled);
        });
        (Self { commands }, receiver)
    }

    /// Retains an acknowledged subscription; identical active requests share one provider ID.
    /// Acknowledgement does not include an initial account image.
    /// The target is retained in queued updates, including those buffered before
    /// an unsubscribe completes.
    ///
    /// Capacity failures return immediately. Do not cancel: admitted work may
    /// complete after the caller stops waiting.
    pub(crate) async fn subscribe(&self, account: AccountSubscription) -> Result<()> {
        self.request(SubscriptionRequest::Subscribe, account).await
    }

    /// Releases one subscription reference; the last release unsubscribes remotely.
    /// Do not overlap ordinary subscribe and unsubscribe operations for the same key.
    ///
    /// Already-lost subscriptions are a no-op; buffered updates may still arrive.
    /// Connection loss returns [`Error::Disconnected`]; the registry logs its cause.
    pub(crate) async fn unsubscribe(&self, pubkey: Pubkey) -> Result<()> {
        self.request(SubscriptionRequest::Unsubscribe, pubkey).await
    }

    /// Orders cache cleanup before subsequent acquisition without waiting for I/O.
    pub(crate) async fn evict(&self, target: Pubkey) -> Result<()> {
        self.commands
            .send(SubscriptionRequest::Evict(target))
            .await
            .map_err(|_| Error::Closed)
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
    /// Capacity occupied by account subscription states and internal `Clock` subscriptions.
    occupied: usize,
    /// Capacity allocated to all entries, including connecting sockets.
    capacity: usize,
    cursor: usize,
}

impl Registry {
    /// Serializes subscription requests and socket outcomes before replying or changing routes.
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
                            self.subscribe(account, reply);
                        }
                        SubscriptionRequest::Unsubscribe(pubkey, reply) => {
                            self.unsubscribe(pubkey, reply).await;
                        }
                        SubscriptionRequest::Evict(target) => self.evict(target).await,
                    }
                }
                Some(notice) = notices.recv() => self.notice(notice).await,
            }
        }
    }

    /// Releases all subscription references for an evicted Engine target.
    async fn evict(&mut self, target: Pubkey) {
        for sub in AccountSubscription::for_target(target) {
            let pubkey = sub.pubkey;
            let Some(&index) = self.routes.get(&pubkey) else { continue };
            let Some(Subscription::Active { account, refs, .. }) =
                self.sockets[index].accounts.get_mut(&pubkey)
            else {
                continue;
            };
            if *account != sub {
                continue;
            }
            *refs = 1;
            let (reply, _) = oneshot::channel();
            self.unsubscribe(pubkey, reply).await;
        }
    }

    /// Reserves capacity before waiting for remote acknowledgement.
    fn subscribe(&mut self, account: AccountSubscription, reply: Reply) {
        let pubkey = account.pubkey;
        if let Some(&index) = self.routes.get(&pubkey) {
            // Overlapping acquisition waves share an acknowledged subscription.
            let result = match self.sockets[index].accounts.get_mut(&pubkey) {
                Some(Subscription::Active { account: current, refs, .. })
                    if *current == account =>
                {
                    *refs += 1;
                    Ok(())
                }
                _ => Err(Error::Unavailable),
            };
            let _ = reply.send(result);
            return;
        }
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
    async fn unsubscribe(&mut self, pubkey: Pubkey, reply: Reply) {
        let Some(&index) = self.routes.get(&pubkey) else {
            // Connection loss may have removed this subscription before its release.
            let _ = reply.send(Ok(()));
            return;
        };
        if matches!(
            self.sockets[index].accounts.get(&pubkey),
            Some(Subscription::Active { refs: 1, .. })
        ) {
            // Queue logical removal before provider unsubscribe so buffered updates
            // fail the worker's coverage check. Remaining references retain coverage.
            let _ = self.events.send(Event::Removed(pubkey)).await;
        }
        let socket = &mut self.sockets[index];
        let Some(state) = socket.accounts.get_mut(&pubkey) else { return };
        match state {
            Subscription::Active { refs, .. } if *refs > 1 => {
                *refs -= 1;
                let _ = reply.send(Ok(()));
            }
            Subscription::Active { remote, .. } => {
                let remote = *remote;
                *state = Subscription::Unsubscribing(reply);
                // If I/O has just stopped, its queued loss notice completes this waiter.
                let _ = socket.commands.send(Command::Unsubscribe { pubkey, remote });
            }
            Subscription::Pending { cancel, .. } => *cancel = Some(reply),
            Subscription::Unsubscribing(_) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Finds ready capacity without queuing behind connection attempts.
    fn admit(&mut self) -> Result<usize> {
        let len = self.sockets.len();
        for index in (self.cursor..len).chain(0..self.cursor) {
            let socket = &self.sockets[index];
            if !socket.healthy() {
                continue;
            }
            let limit = self.config.providers[socket.id.provider].subs_per_connection;
            if socket.occupied() < limit {
                return Ok(index);
            }
        }
        self.grow();
        let limit: usize =
            self.config.providers.iter().map(|provider| provider.max_connections).sum();
        let full = self.occupied == self.capacity && self.sockets.len() == limit;
        Err(if full { Error::Capacity } else { Error::Unavailable })
    }

    /// Applies socket outcomes before replying to requests or reporting lost coverage.
    async fn notice(&mut self, notice: Notice) {
        match notice {
            Notice::Connected(connection) => {
                let provider = connection.provider;
                if connection.generation > 0 {
                    info!(provider, "WS reconnected");
                }
                let socket = &mut self.sockets[connection.index];
                socket.ready = true;
                socket.backoff = Duration::ZERO;
            }
            Notice::Acknowledged { connection, pubkey, result } => {
                // Acknowledgements do not trigger pool growth.
                return self.acknowledge(connection, pubkey, result).await;
            }
            Notice::Dropped { connection, error } => {
                let provider = connection.provider;
                metrics::transport(provider, Transport::WebSocket);
                let socket = &mut self.sockets[connection.index];
                self.occupied -= socket.occupied();
                let clock = socket.clock;
                let delay =
                    (socket.backoff * 2).clamp(Duration::from_secs(1), Duration::from_secs(30));
                let mut pubkeys = Vec::new();
                for (pubkey, entry) in socket.accounts.drain() {
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
                        Subscription::Unsubscribing(reply) => {
                            let _ = reply.send(Err(Error::Disconnected));
                            false
                        }
                        Subscription::Active { .. } => true,
                    };
                    if report {
                        pubkeys.push(pubkey);
                    }
                }
                // The failed task has finished publishing updates. Publish loss before replacing
                // it or accepting new account subscriptions, preserving this connection's event order.
                warn!(provider, lost = pubkeys.len(), %error, "WS connection failed");
                let _ = self.events.send(Event::Dropped { pubkeys }).await;
                let id = ConnectionId {
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

    /// Updates subscription state and coverage before replying to the request.
    async fn acknowledge(
        &mut self,
        connection: ConnectionId,
        pubkey: Pubkey,
        result: Result<Option<u64>>,
    ) {
        let socket = &mut self.sockets[connection.index];
        let Some(state) = socket.accounts.remove(&pubkey) else { return };
        let (reply, cancel, result) = match (state, result) {
            (Subscription::Pending { reply, account, cancel }, Ok(Some(remote))) => {
                if let Some(cancel) = cancel {
                    // Unsubscribe raced the subscribe acknowledgement; never publish coverage.
                    socket.accounts.insert(pubkey, Subscription::Unsubscribing(cancel));
                    let _ = socket.commands.send(Command::Unsubscribe { pubkey, remote });
                } else {
                    socket
                        .accounts
                        .insert(pubkey, Subscription::Active { remote, account, refs: 1 });
                    let _ = self.events.send(Event::Acknowledged(account)).await;
                }
                let _ = reply.send(Ok(()));
                return;
            }
            (Subscription::Pending { reply, cancel, .. }, result) => (reply, cancel, result),
            (Subscription::Unsubscribing(reply), result @ (Ok(None) | Err(_))) => {
                (reply, None, result)
            }
            (state, _) => {
                socket.accounts.insert(pubkey, state);
                return;
            }
        };
        self.routes.remove(&pubkey);
        self.occupied -= 1;
        let _ = reply.send(result.map(|_| ()));
        if let Some(cancel) = cancel {
            let _ = cancel.send(Ok(()));
        }
    }

    /// Starts a connection with internal `Clock` queued before account subscription commands.
    fn spawn(&self, id: ConnectionId, backoff: Duration, clock: bool) -> Socket {
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
        let id = ConnectionId {
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
