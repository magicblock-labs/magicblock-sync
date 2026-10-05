use std::convert::Infallible;

use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use solana_pubkey::Pubkey;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use url::Url;

use super::{within, Account, LOOPBACK, RPC_VERSION};

/// Recorded request held until its scenario releases a response.
pub struct Call {
    pub uri: String,
    pub body: Value,
    reply: oneshot::Sender<(StatusCode, String)>,
}

impl Call {
    /// Ordered `getMultipleAccounts` keys, including companion positions.
    pub fn keys(&self) -> impl ExactSizeIterator<Item = Pubkey> + '_ {
        self.body["params"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap().parse().unwrap())
    }

    /// Releases the held request with its scenario's status and raw response body.
    pub fn respond(self, status: StatusCode, body: impl Into<String>) {
        self.reply.send((status, body.into())).unwrap();
    }

    /// Preserves null positions and stamps the entire response with one confirmed context slot.
    pub fn snapshot(self, slot: u64, accounts: &[Option<Account>]) {
        let values: Vec<_> = accounts.iter().map(|a| a.as_ref().map(Account::rpc)).collect();
        let body = json!({"jsonrpc":RPC_VERSION, "id":self.body["id"],
            "result":{"context":{"slot":slot}, "value":values}});
        self.respond(StatusCode::OK, body.to_string());
    }
}

/// Scripted HTTP fixture shared by RPC and AML, with each response explicitly released by its test.
pub struct Server {
    pub endpoint: Url,
    calls: mpsc::UnboundedReceiver<Call>,
    cancel: CancellationToken,
    tasks: TaskTracker,
}

impl Server {
    /// Accepts concurrent loopback requests while keeping each response under script control.
    pub async fn new() -> Self {
        let listener = TcpListener::bind(LOOPBACK).await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap()).parse().unwrap();
        let cancel = CancellationToken::new();
        let tasks = TaskTracker::new();
        let (tx, calls) = mpsc::unbounded_channel();
        let token = cancel.clone();
        let tracker = tasks.clone();
        tasks.spawn(async move {
            loop {
                let socket = tokio::select! {
                    _ = token.cancelled() => break,
                    socket = listener.accept() => socket.unwrap().0,
                };
                let tx = tx.clone();
                let token = token.clone();
                tracker.spawn(async move {
                    let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                        let tx = tx.clone();
                        async move {
                            let uri = request.uri().to_string();
                            let bytes = request.into_body().collect().await.unwrap().to_bytes();
                            let body = if bytes.is_empty() { Value::Null } else {
                                serde_json::from_slice(&bytes).unwrap()
                            };
                            let (reply, rx) = oneshot::channel();
                            tx.send(Call { uri, body, reply }).unwrap();
                            let (status, body) = rx.await.unwrap();
                            Ok::<_, Infallible>(Response::builder().status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(body))).unwrap())
                        }
                    });
                    tokio::select! {
                        _ = token.cancelled() => {},
                        _ = http1::Builder::new().serve_connection(TokioIo::new(socket), service) => {},
                    }
                });
            }
        });
        Self { endpoint, calls, cancel, tasks }
    }

    pub async fn next(&mut self) -> Call {
        within(self.calls.recv()).await.expect("HTTP request")
    }

    /// Consumes one queued request, if any; assertions call this only when no request is expected.
    pub fn pending(&mut self) -> bool {
        self.calls.try_recv().is_ok()
    }

    /// Cancels held requests and joins the accept loop and every connection task.
    pub async fn close(self) {
        self.cancel.cancel();
        self.tasks.close();
        within(self.tasks.wait()).await;
    }
}
