use std::time::Duration;

use crate::{
    pool::COMMAND_CAP,
    websocket::{self, Reader, Writer, MAX_MESSAGE},
    Connection, Error, Event, Reservation, RpcError, Subscription, UiAccount, Url,
};
use ahash::AHashMap;
use derive_more::{Deref, DerefMut};
use fastwebsockets::{Frame, OpCode, Payload};
use futures::{
    future::{self, AbortHandle, Abortable},
    stream::FuturesUnordered,
    StreamExt,
};
use json::{JsonValueTrait, LazyValue};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::{self, Instant, MissedTickBehavior, Sleep},
};

pub(crate) enum Command {
    /// Requests remote coverage for an already admitted reservation.
    Subscribe(Reservation),
    /// Unsubscribes acknowledged coverage using its provider ID.
    Release(Subscription),
}

/// Connection and RPC acknowledgement budget; writes are intentionally untimed.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Ping cadence; a missing pong at the next tick invalidates coverage.
const HEARTBEAT: Duration = Duration::from_secs(15);

struct Pending {
    /// Retains the admission identity and, for release, the provider subscription ID.
    command: Command,
    /// Cancels the acknowledgement timer once a response is correlated.
    timer: AbortHandle,
}

/// Owns subscription transitions, request batching, and event delivery for one incarnation.
pub(crate) struct Orchestrator {
    /// Wire request IDs: even for subscribe, odd for release.
    pending: AHashMap<u64, Pending>,
    /// Outstanding acknowledgement deadlines, cancelled as responses arrive.
    timers: FuturesUnordered<Abortable<Sleep>>,
    /// Provider IDs route notifications to established handles.
    active: AHashMap<u64, Subscription>,
    /// Reused by serialization and transport masking for objects and request batches.
    output: Vec<u8>,
    /// Bounded delivery applies backpressure to this socket's protocol processing.
    events: Sender<Event>,
}

/// Drives socket I/O without cancelling partial reads when commands or timers become ready.
#[derive(Deref, DerefMut)]
pub(crate) struct Session {
    /// Exclusive outbound half for requests and control replies.
    writer: Writer,
    #[deref]
    #[deref_mut]
    orchestrator: Orchestrator,
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
                orchestrator: Orchestrator::new(events.clone()),
            };
            session.emit(Event::Connected(id)).await?;
            session.run(reader, &mut commands).await
        }
        .await;
        // Stop admission and drop the socket before waiting to report coverage loss.
        commands.close();
        if let Err(error) = result {
            let msg = Event::Dropped {
                connection: id,
                reservations: Vec::new(),
                error,
            };
            let _ = events.send(msg).await;
        }
    }

    /// Drives reads, commands, and deadlines without cancelling partially consumed frames.
    async fn run(
        &mut self,
        mut reader: Reader,
        commands: &mut Receiver<Command>,
    ) -> Result<(), Error> {
        let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut awaiting_pong = false;
        let mut batch = Vec::with_capacity(COMMAND_CAP);
        // Automatic replies are disabled; the session sends them through its writer.
        // Writes and event delivery are untimed. While either waits, this socket cannot
        // read or check heartbeat/acknowledgement deadlines; their budgets still elapse.
        let mut send = |_| async { Err::<(), _>(Error::Protocol("unexpected automatic reply")) };
        loop {
            // fastwebsockets consumes header bytes before awaiting the payload. Keep
            // this future alive across command/timer branches or partial reads corrupt
            // framing. Split I/O uses one uncontended Tokio mutex, not another task.
            let read = reader.read_frame(&mut send);
            tokio::pin!(read);
            loop {
                tokio::select! {
                    // Socket reads and deadlines must progress even under command pressure.
                    frame = &mut read => {
                        let frame = frame?;
                        match frame.opcode {
                            OpCode::Text | OpCode::Binary => {
                                // The collector has already allocated the complete message.
                                if frame.payload.len() > MAX_MESSAGE {
                                    return Err(Error::Protocol("message exceeds size limit"));
                                }
                                self.message(&frame.payload).await?;
                            }
                            OpCode::Continuation => return Err(Error::Protocol("unexpected continuation")),
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
                    count = commands.recv_many(&mut batch, COMMAND_CAP) => {
                        if count == 0 {
                            return Ok(());
                        }
                        if count > 1 {
                            self.output.push(b'[');
                        }
                        for (index, command) in batch.drain(..).enumerate() {
                            if index > 0 {
                                self.output.push(b',');
                            }
                            match command {
                                Command::Subscribe(sub) => self.subscribe(sub)?,
                                Command::Release(sub) => self.release(sub)?,
                            }
                        }
                        if count > 1 {
                            self.output.push(b']');
                        }
                        self.flush().await?;
                    }
                    _ = self.expired() => {
                        return Err(Error::Timeout("RPC acknowledgement"));
                    }
                    _ = heartbeat.tick() => {
                        if awaiting_pong { return Err(Error::Timeout("heartbeat")); }
                        let frame = Frame::new(true, OpCode::Ping, None, Payload::Borrowed(b""));
                        self.writer.write_frame(frame).await?;
                        awaiting_pong = true;
                    }
                }
            }
        }
    }

    /// Sends ready requests, reusing the buffer for serialization and client masking.
    async fn flush(&mut self) -> Result<(), Error> {
        if self.output.is_empty() {
            return Ok(());
        }
        let payload = self.orchestrator.output.as_mut_slice().into();
        self.writer.write_frame(Frame::text(payload)).await?;
        self.output.clear();
        Ok(())
    }
}

impl Orchestrator {
    fn new(events: Sender<Event>) -> Self {
        Self {
            pending: AHashMap::new(),
            timers: FuturesUnordered::new(),
            active: AHashMap::new(),
            output: Vec::new(),
            events,
        }
    }

    /// Acknowledged requests wake only to discard their timers; an empty set stays idle.
    async fn expired(&mut self) {
        while let Some(result) = self.timers.next().await {
            if result.is_ok() {
                return;
            }
        }
        future::pending().await
    }

    /// Requests compressed account notifications; coverage begins only after acknowledgement.
    fn subscribe(&mut self, reservation: Reservation) -> Result<(), Error> {
        let config = AccountConfig {
            encoding: "base64+zstd",
            commitment: "confirmed",
        };
        self.request(
            Command::Subscribe(reservation),
            "accountSubscribe",
            (reservation.account.to_string(), config),
        )
    }

    /// Requests release without freeing remote capacity before acknowledgement.
    fn release(&mut self, subscription: Subscription) -> Result<(), Error> {
        self.request(
            Command::Release(subscription),
            "accountUnsubscribe",
            [subscription.remote],
        )
    }

    /// Appends to the current wire batch; serialization and writing count toward the budget.
    fn request(
        &mut self,
        command: Command,
        method: &'static str,
        params: impl Serialize,
    ) -> Result<(), Error> {
        let (timer, registration) = AbortHandle::new_pair();
        self.timers.push(Abortable::new(time::sleep(TIMEOUT), registration));
        let id = match &command {
            Command::Subscribe(reservation) => reservation.id * 2,
            Command::Release(subscription) => subscription.reservation.id * 2 + 1,
        };
        self.pending.insert(id, Pending { command, timer });
        let request = Request {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        json::to_writer(&mut self.output, &request)?;
        Ok(())
    }

    /// Batch responses may be reordered; each envelope follows the same correlation path.
    async fn message(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'[') {
            // The lazy iterator stops at `]`; validate the whole message to reject trailing data.
            let batch: LazyValue<'_> = json::from_slice(bytes)?;
            let mut messages = json::to_array_iter(batch.as_raw_str()).peekable();
            if messages.peek().is_none() {
                return Err(Error::Protocol("empty RPC batch"));
            }
            for message in messages {
                self.envelope(message?.as_raw_str().as_bytes()).await?;
            }
        } else {
            self.envelope(bytes).await?;
        }
        Ok(())
    }

    /// Validates routing before decoding an update, then waits for delivery capacity.
    async fn envelope(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let message: Envelope<'_> = json::from_slice(bytes)?;
        if let Some(id) = message.id {
            let pending = self.pending.remove(&id).ok_or(Error::Protocol("unknown request ID"))?;
            pending.timer.abort();
            return self.response(pending, message.result, message.error).await;
        }
        let notification = message.params.ok_or(Error::Protocol("missing notification params"))?;
        let subscription = *self
            .active
            .get(&notification.subscription)
            .ok_or(Error::Protocol("unknown remote subscription"))?;
        let account: Account = json::from_str(notification.result.as_raw_str())?;
        self.emit(Event::Update {
            subscription,
            slot: account.context.slot,
            account: account.value,
        })
        .await
    }

    /// Applies a correlated acknowledgement to subscription state and emits its outcome.
    async fn response(
        &mut self,
        pending: Pending,
        result: Option<LazyValue<'_>>,
        error: Option<LazyValue<'_>>,
    ) -> Result<(), Error> {
        if let Some(error) = error {
            let error: RpcError = json::from_str(error.as_raw_str())?;
            // A rejected unsubscribe leaves remote capacity ambiguous. Close the socket
            // instead of pretending the reservation is free or leaking it indefinitely.
            return match pending.command {
                Command::Release(_) => Err(Error::Rpc(error)),
                Command::Subscribe(reservation) => {
                    self.emit(Event::Rejected { reservation, error }).await
                }
            };
        }
        let result = result.ok_or(Error::Protocol("missing RPC result"))?;
        let event = match pending.command {
            Command::Release(subscription) => {
                if result.as_bool() != Some(true) {
                    return Err(Error::Protocol("unsubscribe was not acknowledged"));
                }
                self.active
                    .remove(&subscription.remote)
                    .ok_or(Error::Protocol("release has no active subscription"))?;
                Event::Released(subscription)
            }
            Command::Subscribe(reservation) => {
                let remote =
                    result.as_u64().ok_or(Error::Protocol("invalid remote subscription ID"))?;
                let subscription = Subscription { reservation, remote };
                self.active.insert(remote, subscription);
                Event::Established(subscription)
            }
        };
        self.emit(event).await
    }

    /// Preserves event order and waits for the consumer rather than dropping a full queue's update.
    async fn emit(&self, event: Event) -> Result<(), Error> {
        self.events.send(event).await.map_err(|_| Error::Closed)
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
    /// Base64+zstd keeps compressed notification data in the wire representation for callers.
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
    /// Account payload left borrowed until routing is validated.
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
