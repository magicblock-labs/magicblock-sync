use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use url::Url;

use super::{within, Account, LOOPBACK, RPC_VERSION};

/// Held subscription request; the scenario decides when acknowledgement or rejection is released.
pub struct Call {
    pub body: Value,
    reply: oneshot::Sender<Value>,
}

impl Call {
    /// Remote account address in this held `accountSubscribe` request.
    pub fn pubkey(&self) -> Pubkey {
        self.body["params"][0].as_str().unwrap().parse().unwrap()
    }

    /// Assigns the provider ID later used by notifications and unsubscribe requests.
    pub fn ack(self, subscription: u64) {
        self.reply
            .send(json!({"jsonrpc":RPC_VERSION, "id":self.body["id"], "result":subscription}))
            .unwrap();
    }

    /// Rejects only this subscription without disconnecting the socket.
    pub fn reject(self) {
        self.reply
            .send(json!({"jsonrpc":RPC_VERSION, "id":self.body["id"],
            "error":{"code":-32602,"message":"scripted rejection"}}))
            .unwrap();
    }
}

/// Holds acknowledgements and delivers ordered notifications on one controlled provider socket.
pub struct Server {
    pub endpoint: Url,
    calls: mpsc::UnboundedReceiver<Call>,
    unsubscribes: mpsc::UnboundedReceiver<u64>,
    output: mpsc::UnboundedSender<Message>,
    connected: mpsc::UnboundedReceiver<()>,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl Server {
    /// Accepts replacement sockets, auto-acknowledges releases, and holds subscription replies.
    pub async fn new() -> Self {
        let listener = TcpListener::bind(LOOPBACK).await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap()).parse().unwrap();
        let (tx, calls) = mpsc::unbounded_channel();
        let (released, unsubscribes) = mpsc::unbounded_channel();
        let (output, mut rx) = mpsc::unbounded_channel();
        let (ready, connected) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let tasks = TaskTracker::new();
        let token = cancel.clone();
        tasks.spawn(async move {
            'connections: loop {
                let socket = tokio::select! {
                    _ = token.cancelled() => return,
                    result = listener.accept() => result.unwrap().0,
                };
                let socket = accept_async(socket).await.unwrap();
                let (mut sink, mut stream) = socket.split();
                ready.send(()).unwrap();
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break 'connections,
                        Some(message) = rx.recv() => {
                            let closing = matches!(message, Message::Close(_));
                            if sink.send(message).await.is_err() { break; }
                            if closing { break; }
                        }
                        message = stream.next() => {
                            let text = match message {
                                Some(Ok(Message::Text(text))) => text,
                                Some(Ok(Message::Ping(data))) => {
                                    sink.send(Message::Pong(data)).await.unwrap();
                                    continue;
                                }
                                _ => break,
                            };
                            let message: Value = serde_json::from_str(&text).unwrap();
                            let batch = match message {
                                Value::Array(batch) => batch,
                                body => vec![body],
                            };
                            for body in batch {
                                if body["method"] == "accountUnsubscribe" {
                                    released.send(body["params"][0].as_u64().unwrap()).unwrap();
                                    let response = json!({"jsonrpc":RPC_VERSION,"id":body["id"],"result":true});
                                    sink.send(Message::Text(response.to_string().into())).await.unwrap();
                                    continue;
                                }
                                let (reply, response) = oneshot::channel();
                                tx.send(Call { body, reply }).unwrap();
                                let reply = tokio::select! {
                                    _ = token.cancelled() => break 'connections,
                                    reply = response => reply.unwrap(),
                                };
                                sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                            }
                        }
                    }
                }
            }
        });
        Self {
            endpoint,
            calls,
            unsubscribes,
            output,
            connected,
            cancel,
            tasks,
        }
    }

    /// Waits for the provider socket, not for account subscription coverage.
    pub async fn wait_for_connection(&mut self) {
        within(self.connected.recv()).await.expect("provider connection");
    }

    /// Receives the next held subscription request, preserving batch order.
    pub async fn next(&mut self) -> Call {
        within(self.calls.recv()).await.expect("subscription request")
    }

    /// Receives an observed unsubscribe ID independently of subscription scripts.
    pub async fn unsubscribed(&mut self) -> u64 {
        within(self.unsubscribes.recv()).await.expect("unsubscribe request")
    }

    /// Queues a confirmed notification; retired IDs deliberately exercise late delivery.
    pub fn notify(&self, subscription: u64, slot: u64, account: &Account) {
        let update = json!({"jsonrpc":RPC_VERSION, "method":"accountNotification", "params":{
            "subscription":subscription, "result":{"context":{"slot":slot},"value":account.rpc()}}});
        self.output.send(Message::Text(update.to_string().into())).unwrap();
    }

    /// Closes the current socket while keeping the listener available for reconnects.
    pub fn disconnect(&self) {
        self.output.send(Message::Close(None)).unwrap();
    }

    /// Cancels socket I/O and held acknowledgements, then joins the provider task.
    pub async fn close(self) {
        self.cancel.cancel();
        self.tasks.close();
        within(self.tasks.wait()).await;
    }
}
