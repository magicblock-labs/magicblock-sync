use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use crate::{
    account::WireAccount,
    rpc::{
        AccountConfig, ContextValue, Request, ACCOUNT_NOTIFICATION, ACCOUNT_SUBSCRIBE,
        ACCOUNT_UNSUBSCRIBE,
    },
    websocket::{self, Reader, Writer, MAX_MESSAGE},
    Connection, Error, Event, Pubkey, RpcError, Url,
};
use ahash::AHashMap;
use fastwebsockets::{Frame, OpCode, Payload};
use futures::{
    future::{self, AbortHandle, Abortable},
    stream::{self, FuturesUnordered},
    StreamExt,
};
use json::{JsonValueTrait, LazyValue};
use serde::{Deserialize, Serialize};
use solana_sdk_ids::sysvar::clock;
use tokio::{
    sync::mpsc::{Sender, UnboundedReceiver, UnboundedSender},
    time::{self, Instant, MissedTickBehavior, Sleep},
};

/// Subscription commands assigned to one connection attempt.
pub(crate) enum Command {
    /// Subscribes to an account whose subscription capacity is already reserved.
    Subscribe(Pubkey),
    /// Unsubscribes using the provider's acknowledged subscription ID.
    Unsubscribe {
        /// Account being unsubscribed.
        pubkey: Pubkey,
        /// Provider-issued subscription ID.
        remote: u64,
    },
}

/// Control-plane outcomes consumed only by the pool registry.
pub(crate) enum Notice {
    /// The socket is accepting commands.
    Connected(Connection),
    /// Subscribe returns a remote ID; unsubscribe returns none; rejection returns an error.
    Acknowledged {
        /// Connection identity responsible for the operation.
        connection: Connection,
        /// Account whose operation completed.
        pubkey: Pubkey,
        /// Provider response, validated by the socket task.
        result: Result<Option<u64>, Error>,
    },
    /// All subscriptions on this connection are lost; no further updates can follow.
    Dropped {
        /// Failed connection identity.
        connection: Connection,
        /// Precise cause, retained for the public loss event.
        error: Error,
    },
}

/// Connection and RPC acknowledgement budget; writes are intentionally untimed.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum commands processed per socket iteration, independent of mailbox capacity.
pub(crate) const COMMAND_CAP: usize = 256;
/// Ping cadence; a missing pong at the next tick invalidates the connection's subscriptions.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// A sent request awaiting acknowledgement within its deadline.
struct Pending {
    /// Retains the pubkey and, for unsubscribe, the provider subscription ID.
    command: Command,
    /// Cancels the acknowledgement timer once a response is correlated.
    timer: AbortHandle,
}

/// Owns protocol state and I/O for one connection attempt. Reads stay pinned across command
/// and timer branches so partially consumed frames are never cancelled.
pub(crate) struct Session {
    /// Exclusive outbound half for requests and control replies.
    writer: Writer,
    /// Monotonic request ID within this connection attempt; assumed not to exhaust u64.
    sequence: u64,
    /// Wire request IDs correlate acknowledgements with pending commands.
    pending: AHashMap<u64, Pending>,
    /// Outstanding acknowledgement deadlines, cancelled as responses arrive.
    timers: FuturesUnordered<Abortable<Sleep>>,
    /// Provider IDs route notifications directly to pubkeys.
    active: AHashMap<u64, Pubkey>,
    /// Identity supplied with every internal lifecycle outcome.
    id: Connection,
    /// Lifecycle-only channel; account updates bypass the registry.
    notices: UnboundedSender<Notice>,
    /// Reused by serialization and transport masking for objects and request batches.
    output: Vec<u8>,
    /// Bounded delivery applies backpressure to this socket's protocol processing.
    events: Sender<Event>,
    /// Pool-wide minimum Solana context slot for HTTP fetches. Valid confirmed updates only
    /// raise it; it survives this socket's replacement and is not a chain-head guarantee.
    slot: Arc<AtomicU64>,
}

impl Session {
    /// Runs one connection attempt, closing its command receiver before reporting subscription loss.
    pub(crate) async fn start(
        id: Connection,
        url: Url,
        mut commands: UnboundedReceiver<Command>,
        events: Sender<Event>,
        notices: UnboundedSender<Notice>,
        delay: Duration,
        slot: Arc<AtomicU64>,
    ) {
        let result = async {
            time::sleep_until(Instant::now() + delay).await;
            let (reader, writer) = time::timeout(TIMEOUT, websocket::connect(&url))
                .await
                .map_err(|_| Error::Timeout("connect"))??;
            let mut session = Self {
                writer,
                sequence: 0,
                id,
                notices: notices.clone(),
                pending: AHashMap::new(),
                timers: FuturesUnordered::new(),
                active: AHashMap::new(),
                output: Vec::new(),
                events: events.clone(),
                slot,
            };
            session.notify(Notice::Connected(id))?;
            session.run(reader, &mut commands).await
        }
        .await;
        // Close the command receiver and drop the socket before reporting subscription loss.
        commands.close();
        if let Err(error) = result {
            let _ = notices.send(Notice::Dropped { connection: id, error });
        }
    }

    /// Drives reads, commands, and deadlines without cancelling partially consumed frames.
    async fn run(
        &mut self,
        reader: Reader,
        commands: &mut UnboundedReceiver<Command>,
    ) -> Result<(), Error> {
        let mut heartbeat = time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut awaiting_pong = false;
        let mut batch = Vec::with_capacity(COMMAND_CAP);
        // The pinned stream owns the in-flight read. Dropping next() when another
        // branch wins does not cancel a partially consumed frame or restart framing.
        // No extra task or heap allocation is needed for the stream itself.
        let frames = stream::unfold(reader, |mut reader| async {
            // Automatic replies are disabled; all writes stay on the session's writer.
            let mut send =
                |_| async { Err::<(), _>(Error::Protocol("unexpected automatic reply")) };
            let frame = reader.read_frame(&mut send).await;
            Some((frame, reader))
        });
        tokio::pin!(frames);
        // Writes and event delivery are untimed. While either waits, this socket cannot
        // read or check deadlines, but their budgets continue to elapse.
        loop {
            tokio::select! {
                biased;
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
                            Command::Subscribe(pubkey) => {
                                let params = (pubkey.to_string(), AccountConfig::new(None));
                                self.request(Command::Subscribe(pubkey), params)?;
                            }
                            Command::Unsubscribe { pubkey, remote } => {
                                self.request(Command::Unsubscribe { pubkey, remote }, [remote])?;
                            }
                        }
                    }
                    if count > 1 {
                        self.output.push(b']');
                    }
                    // Reuse the nonempty batch buffer for transport masking and serialization.
                    let payload = self.output.as_mut_slice().into();
                    self.writer.write_frame(Frame::text(payload)).await?;
                    self.output.clear();
                }
                // Socket reads and deadlines must progress even under command pressure.
                Some(frame) = frames.next() => {
                    let frame = frame?;
                    match frame.opcode {
                        OpCode::Text | OpCode::Binary => {
                            // The collector has already allocated the complete message.
                            if frame.payload.len() > MAX_MESSAGE {
                                return Err(Error::Protocol("message exceeds size limit"));
                            }
                            self.message(&frame.payload).await?;
                        }
                        OpCode::Continuation => {
                            return Err(Error::Protocol("unexpected continuation"));
                        }
                        OpCode::Ping | OpCode::Pong if frame.payload.len() > 125 => {
                            return Err(Error::Protocol("oversized control frame"));
                        }
                        OpCode::Ping => {
                            self.writer.write_frame(Frame::pong(frame.payload)).await?;
                        }
                        OpCode::Pong => awaiting_pong = false,
                        OpCode::Close => {
                            let _ = self.writer.write_frame(Frame::close(1000, b"")).await;
                            return Err(Error::Disconnected);
                        }
                    }
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

    /// Acknowledged requests wake only to discard their timers; an empty set stays idle.
    async fn expired(&mut self) {
        while let Some(result) = self.timers.next().await {
            if result.is_ok() {
                return;
            }
        }
        future::pending().await
    }

    /// Appends to the current wire batch; serialization and writing count toward the budget.
    fn request(&mut self, command: Command, params: impl Serialize) -> Result<(), Error> {
        let (timer, registration) = AbortHandle::new_pair();
        self.timers.push(Abortable::new(
            time::sleep_until(Instant::now() + TIMEOUT),
            registration,
        ));
        self.sequence += 1;
        let id = self.sequence;
        let method = match &command {
            Command::Subscribe(_) => ACCOUNT_SUBSCRIBE,
            Command::Unsubscribe { .. } => ACCOUNT_UNSUBSCRIBE,
        };
        self.pending.insert(id, Pending { command, timer });
        let request = Request::new(id, method, params);
        json::to_writer(&mut self.output, &request)?;
        Ok(())
    }

    /// Batch acknowledgements may be reordered. Single replies and account notifications
    /// are objects, so both wire shapes use the same envelope routing.
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
            if message.result.is_some() == message.error.is_some() {
                return Err(Error::Protocol("invalid RPC response envelope"));
            }
            let pending = self.pending.remove(&id).ok_or(Error::Protocol("unknown request ID"))?;
            pending.timer.abort();
            return self.response(pending, message.result, message.error);
        }
        if message.method != Some(ACCOUNT_NOTIFICATION) {
            return Err(Error::Protocol("invalid notification method"));
        }
        let notification = message.params.ok_or(Error::Protocol("missing notification params"))?;
        let pubkey = *self
            .active
            .get(&notification.subscription)
            .ok_or(Error::Protocol("unknown remote subscription"))?;
        let account: ContextValue<Option<WireAccount<'_>>> =
            json::from_str(notification.result.as_raw_str())?;
        let slot = account.context.slot;
        let account = account.value.map(|value| value.decode(slot)).transpose()?;
        // Every valid confirmed update contributes, including explicit absence.
        self.slot.fetch_max(slot, Relaxed);
        if pubkey == clock::ID {
            return Ok(());
        }
        self.events
            .send(Event::Update { pubkey, slot, account })
            .await
            .map_err(|_| Error::Closed)
    }

    /// Updates wire routing before reporting an acknowledgement to the registry.
    fn response(
        &mut self,
        pending: Pending,
        result: Option<LazyValue<'_>>,
        error: Option<LazyValue<'_>>,
    ) -> Result<(), Error> {
        if let Some(error) = error {
            let error: RpcError = json::from_str(error.as_raw_str())?;
            // Clock is mandatory; rejected unsubscribe leaves remote capacity ambiguous.
            return match pending.command {
                Command::Subscribe(pubkey) if pubkey != clock::ID => {
                    self.notify(Notice::Acknowledged {
                        connection: self.id,
                        pubkey,
                        result: Err(Error::Rpc(error)),
                    })
                }
                _ => Err(Error::Rpc(error)),
            };
        }
        let result = result.ok_or(Error::Protocol("missing RPC result"))?;
        let (pubkey, remote) = match pending.command {
            Command::Unsubscribe { pubkey, remote } => {
                if result.as_bool() != Some(true) {
                    return Err(Error::Protocol("unsubscribe was not acknowledged"));
                }
                self.active.remove(&remote);
                (pubkey, None)
            }
            Command::Subscribe(pubkey) => {
                let remote =
                    result.as_u64().ok_or(Error::Protocol("invalid remote subscription ID"))?;
                if self.active.insert(remote, pubkey).is_some() {
                    return Err(Error::Protocol("duplicate remote subscription ID"));
                }
                if pubkey == clock::ID {
                    return Ok(());
                }
                (pubkey, Some(remote))
            }
        };
        self.notify(Notice::Acknowledged {
            connection: self.id,
            pubkey,
            result: Ok(remote),
        })
    }

    /// Lifecycle outcomes remain ordered per socket without blocking account-update delivery.
    fn notify(&self, notice: Notice) -> Result<(), Error> {
        self.notices.send(notice).map_err(|_| Error::Closed)
    }
}

// Providers are assumed to send standard JSON-RPC envelopes and unique active IDs.
// Borrow payloads until their routing identity has passed validation.
// Accepted account updates still use full typed decoding; no intermediate Value tree.
/// Borrowed routing envelope, validated before interpreting its operation-specific payload.
#[derive(Deserialize)]
struct Envelope<'a> {
    /// Account-notification method for messages without a request ID.
    method: Option<&'a str>,
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

/// Account update tied to an established remote subscription.
#[derive(Deserialize)]
struct Notification<'a> {
    /// Provider-issued subscription ID, valid only on this connection.
    subscription: u64,
    /// Account payload left borrowed until routing is validated.
    #[serde(borrow)]
    result: LazyValue<'a>,
}
