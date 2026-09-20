use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use crate::{
    websocket::{self, Reader, Writer, MAX_MESSAGE},
    Connection, Error, Event, RpcError, Subscription, UiAccount, Url,
};
use fastwebsockets::{Frame, OpCode, Payload};
use serde::{Deserialize, Serialize};
use sonic_rs::{JsonValueTrait, LazyValue};
use tokio::{
    sync::mpsc::{error::TrySendError, Permit, Receiver, Sender},
    time::{self, Instant, MissedTickBehavior},
};

pub(crate) enum Command {
    /// Requests remote coverage for an already admitted reservation.
    Subscribe(Subscription),
    /// Cancels pending establishment or unsubscribes acknowledged coverage.
    Release(Subscription),
}

/// Connection and RPC acknowledgement budget; writes are intentionally untimed.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Ping cadence; a missing pong at the next tick invalidates coverage.
const HEARTBEAT: Duration = Duration::from_secs(15);

struct Pending {
    /// Reservation whose subscribe or unsubscribe acknowledgement is outstanding.
    subscription: Subscription,
    /// Absolute acknowledgement deadline, also indexed in `Session::deadlines`.
    deadline: Instant,
    /// A release arrived before subscribe acknowledgement; suppress establishment.
    cancelled: bool,
}

/// Subscription protocol state scoped to one connected socket incarnation.
pub(crate) struct Session {
    /// Exclusive outbound half for requests and control replies.
    writer: Writer,
    /// Reusable serialization buffer, also borrowed mutably for client frame masking.
    output: Vec<u8>,
    /// Delivery must reserve space before decoding account payloads.
    events: Sender<Event>,
    /// Wire request IDs awaiting acknowledgement: even for subscribe, odd for release.
    pending: HashMap<u64, Pending>,
    /// Ordered deadline/request pairs avoid scanning pending requests to find the next timeout.
    deadlines: BTreeSet<(Instant, u64)>,
    /// Provider subscription IDs mapped to local reservations for notification routing.
    active: HashMap<u64, Subscription>,
    /// Reverse index from local reservation IDs to provider IDs for unsubscribe requests.
    remote: HashMap<u64, u64>,
}

impl Session {
    /// Runs one incarnation, closing admission before reliably reporting any coverage loss.
    pub(crate) async fn start(
        id: Connection,
        url: Url,
        mut commands: Receiver<Command>,
        events: Sender<Event>,
        delay: Duration,
    ) {
        let result = async {
            time::sleep_until(Instant::now() + delay).await;
            let (reader, writer) = time::timeout(TIMEOUT, websocket::connect(&url))
                .await
                .map_err(|_| Error::Timeout("connect"))??;
            let mut session = Self {
                writer,
                output: Vec::new(),
                events: events.clone(),
                pending: HashMap::new(),
                deadlines: BTreeSet::new(),
                active: HashMap::new(),
                remote: HashMap::new(),
            };
            session.emit(Event::Connected(id))?;
            session.run(reader, &mut commands).await
        }
        .await;
        // Stop admission and drop the socket before waiting for space. This final event
        // cannot use try_send: it explains any update lost to a full delivery queue.
        commands.close();
        if let Err(error) = result {
            let _ = events
                .send(Event::Dropped {
                    connection: id,
                    subscriptions: Vec::new(),
                    error,
                })
                .await;
        }
    }

    /// Drives reads, commands, and deadlines without cancelling partially consumed frames.
    async fn run(
        &mut self,
        mut reader: Reader,
        commands: &mut Receiver<Command>,
    ) -> Result<(), Error> {
        let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut awaiting_pong = false;
        let mut fragments = None;
        // Automatic replies are disabled; the session sends them through its writer.
        // Peers are assumed to keep reading. Writes have no deadline, so a stalled
        // write would also suspend this session's heartbeat and acknowledgement timers.
        let mut send = |_| async { Err::<(), _>(Error::Protocol("unexpected automatic reply")) };
        loop {
            // fastwebsockets consumes header bytes before awaiting the payload. Keep
            // this future alive across command/timer branches or partial reads corrupt
            // framing. Split I/O uses one uncontended Tokio mutex, not another task.
            let read = reader.read_frame(&mut send);
            tokio::pin!(read);
            loop {
                let deadline = self.deadlines.first().map(|(at, _)| *at);
                tokio::select! {
                    // Socket reads and deadlines must progress even under command pressure.
                    frame = &mut read => {
                        let frame = frame?;
                        match frame.opcode {
                            OpCode::Text | OpCode::Binary | OpCode::Continuation => self.data(frame, &mut fragments).await?,
                            OpCode::Ping | OpCode::Pong if frame.payload.len() > 125 => {
                                return Err(Error::Protocol("oversized control frame"));
                            }
                            OpCode::Ping => self.writer.write_frame(Frame::pong(frame.payload)).await?,
                            OpCode::Pong => awaiting_pong = false,
                            OpCode::Close => {
                                let _ = self.writer.write_frame(Frame::close(1000, b"")).await;
                                return Err(Error::Disconnected);
                            }
                        }
                        break;
                    }
                    command = commands.recv() => {
                        match command {
                            Some(Command::Subscribe(subscription)) => self.subscribe(subscription).await?,
                            Some(Command::Release(subscription)) => self.release(subscription).await?,
                            None => return Ok(()),
                        }
                    }
                    _ = time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                        return Err(Error::Timeout("RPC acknowledgement"));
                    }
                    _ = heartbeat.tick() => {
                        if awaiting_pong { return Err(Error::Timeout("heartbeat")); }
                        self.writer.write_frame(Frame::new(true, OpCode::Ping, None, Payload::Borrowed(b""))).await?;
                        awaiting_pong = true;
                    }
                }
            }
        }
    }

    /// Routes a complete data message or assembles fragments within the message-size bound.
    async fn data(
        &mut self,
        frame: Frame<'_>,
        fragments: &mut Option<Vec<u8>>,
    ) -> Result<(), Error> {
        if frame.opcode == OpCode::Continuation {
            let data = fragments.as_mut().ok_or(Error::Protocol("unexpected continuation"))?;
            if frame.payload.len() > MAX_MESSAGE - data.len() {
                return Err(Error::Protocol("fragmented message exceeds size limit"));
            }
            data.extend_from_slice(&frame.payload);
            if frame.fin {
                self.message(data).await?;
                *fragments = None;
            }
            return Ok(());
        }
        if fragments.is_some() {
            return Err(Error::Protocol("interleaved data messages"));
        }
        if frame.fin {
            return self.message(&frame.payload).await;
        }
        *fragments = Some(frame.payload.into());
        Ok(())
    }

    /// Requests base64 account notifications; coverage begins only after acknowledgement.
    async fn subscribe(&mut self, subscription: Subscription) -> Result<(), Error> {
        let id = subscription.id * 2;
        let config = AccountConfig {
            encoding: "base64",
            commitment: "confirmed",
        };
        self.request(
            id,
            subscription,
            "accountSubscribe",
            (subscription.account.to_string(), config),
        )
        .await
    }

    /// Cancels a reservation without freeing remote capacity prematurely.
    async fn release(&mut self, subscription: Subscription) -> Result<(), Error> {
        if let Some(pending) = self.pending.get_mut(&(subscription.id * 2)) {
            // The remote ID does not exist until the subscribe acknowledgement arrives.
            pending.cancelled = true;
            return Ok(());
        }
        let Some(&remote) = self.remote.get(&subscription.id) else {
            // A rejection can race a queued release; its terminal event is already sent.
            return Ok(());
        };
        let id = subscription.id * 2 + 1;
        self.request(id, subscription, "accountUnsubscribe", [remote]).await
    }

    /// Sends an RPC request and tracks its acknowledgement deadline.
    async fn request(
        &mut self,
        id: u64,
        subscription: Subscription,
        method: &'static str,
        params: impl Serialize,
    ) -> Result<(), Error> {
        // Time spent writing counts toward the acknowledgement budget.
        let deadline = Instant::now() + TIMEOUT;
        let request = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        // Separate fields let the writer borrow the buffer for masking without moving it out.
        self.output.clear();
        sonic_rs::to_writer(&mut self.output, &request)?;
        self.writer.write_frame(Frame::text(self.output.as_mut_slice().into())).await?;
        self.pending.insert(
            id,
            Pending {
                subscription,
                deadline,
                cancelled: false,
            },
        );
        self.deadlines.insert((deadline, id));
        Ok(())
    }

    /// Routes a complete provider message to its local reservation.
    async fn message(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let message: Envelope<'_> = sonic_rs::from_slice(bytes)?;
        if let Some(id) = message.id {
            let pending = self.pending.remove(&id).ok_or(Error::Protocol("unknown request ID"))?;
            self.deadlines.remove(&(pending.deadline, id));
            return self.response(id, pending, message.result, message.error).await;
        }
        let notification = message.params.ok_or(Error::Protocol("missing notification params"))?;
        let subscription = *self
            .active
            .get(&notification.subscription)
            .ok_or(Error::Protocol("unknown remote subscription"))?;
        // Reserve delivery before decoding account data: overflow invalidates coverage
        // without allocating a payload that cannot be delivered.
        let permit = self.reserve()?;
        let account: Account = sonic_rs::from_str(notification.result.as_raw_str())?;
        permit.send(Event::Update {
            subscription,
            slot: account.context.slot,
            account: account.value,
        });
        Ok(())
    }

    /// Applies a correlated acknowledgement to subscription state and emits its outcome.
    async fn response(
        &mut self,
        id: u64,
        pending: Pending,
        result: Option<LazyValue<'_>>,
        error: Option<LazyValue<'_>>,
    ) -> Result<(), Error> {
        let subscription = pending.subscription;
        if let Some(error) = error {
            let error: RpcError = sonic_rs::from_str(error.as_raw_str())?;
            // A rejected unsubscribe leaves remote capacity ambiguous. Close the socket
            // instead of pretending the reservation is free or leaking it indefinitely.
            if id % 2 == 1 {
                return Err(Error::Rpc(error));
            }
            return self.emit(if pending.cancelled {
                Event::Released(subscription)
            } else {
                Event::Rejected { subscription, error }
            });
        }
        let result = result.ok_or(Error::Protocol("missing RPC result"))?;
        if id % 2 == 1 {
            if result.as_bool() != Some(true) {
                return Err(Error::Protocol("unsubscribe was not acknowledged"));
            }
            let remote = self.remote.remove(&subscription.id).expect("release has a remote ID");
            self.active.remove(&remote).expect("release has an active subscription");
            return self.emit(Event::Released(subscription));
        }
        let remote = result.as_u64().ok_or(Error::Protocol("invalid remote subscription ID"))?;
        self.active.insert(remote, subscription);
        self.remote.insert(subscription.id, remote);
        if pending.cancelled {
            return self.release(subscription).await;
        }
        self.emit(Event::Established(subscription))
    }

    /// Delivers without waiting; queue pressure becomes explicit coverage loss in the caller.
    fn emit(&self, event: Event) -> Result<(), Error> {
        self.reserve()?.send(event);
        Ok(())
    }

    /// Claims delivery capacity without allocating an account payload or blocking socket progress.
    fn reserve(&self) -> Result<Permit<'_, Event>, Error> {
        self.events.try_reserve().map_err(|error| match error {
            TrySendError::Full(_) => Error::DeliveryFull,
            TrySendError::Closed(_) => Error::Closed,
        })
    }
}

#[derive(Serialize)]
struct Request<P> {
    /// Protocol version emitted for all outbound requests.
    jsonrpc: &'static str,
    /// Correlation key pairing this request with its pending acknowledgement.
    id: u64,
    /// Subscription operation understood by the provider.
    method: &'static str,
    /// Operation-specific positional arguments, serialized without an intermediate JSON tree.
    params: P,
}

#[derive(Serialize)]
struct AccountConfig {
    /// Base64 keeps notification data in the wire representation exposed to callers.
    encoding: &'static str,
    /// Always confirmed; this does not guarantee ordering between notifications.
    commitment: &'static str,
}

// Providers are assumed to send standard JSON-RPC envelopes and unique active IDs.
// Borrow payloads until their routing identity has passed validation.
// Accepted account updates still use full typed decoding; no intermediate Value tree.
#[derive(Deserialize)]
struct Envelope<'a> {
    /// Presence selects response handling; absence selects notification handling.
    id: Option<u64>,
    /// Borrowed success payload, decoded according to the pending request's operation.
    #[serde(borrow)]
    result: Option<LazyValue<'a>>,
    /// Borrowed provider error, decoded only after request correlation succeeds.
    #[serde(borrow)]
    error: Option<LazyValue<'a>>,
    /// Required notification routing data when no request ID is present.
    #[serde(borrow)]
    params: Option<Notification<'a>>,
}

#[derive(Deserialize)]
struct Notification<'a> {
    /// Provider-issued ID scoped to this socket incarnation.
    subscription: u64,
    /// Account payload left borrowed until routing and delivery capacity are checked.
    #[serde(borrow)]
    result: LazyValue<'a>,
}

#[derive(Deserialize)]
struct Account {
    /// Provider observation context accompanying the account value.
    context: Context,
    /// Explicit null denotes absence; a missing field is a protocol error.
    #[serde(deserialize_with = "Deserialize::deserialize")]
    value: Option<UiAccount>,
}

#[derive(Deserialize)]
struct Context {
    /// Observation slot reported to the caller, not a cross-provider watermark.
    slot: u64,
}
