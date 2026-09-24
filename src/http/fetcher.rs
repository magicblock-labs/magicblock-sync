use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
        Arc,
    },
    time::Duration,
};

use hyper::body::Bytes;
use json::LazyValue;
use serde::Deserialize;
use solana_account::OwnedAccount;
use solana_pubkey::Pubkey;
use tokio::time::{self, Instant};
use url::Url;

use crate::rpc::{AccountConfig, ContextValue, Request, WireAccount, VERSION};

use super::Error;

/// Fetches a batch of accounts with one shared response context.
const GET_MULTIPLE_ACCOUNTS: &str = "getMultipleAccounts";

/// Total budget across provider selection, cooldown waits, and attempts.
const OVERALL: Duration = Duration::from_secs(10);
/// Maximum time for one provider, capped by the remaining overall budget.
const ATTEMPT: Duration = Duration::from_secs(2);
/// Shared pause after a transient provider failure; successes do not clear it early.
const COOLDOWN: Duration = Duration::from_millis(100);

/// One response context, preserving input order and duplicate keys.
/// Accounts retain Uninit mode for caller classification before materialization.
pub struct Snapshot {
    /// Confirmed Solana context slot shared by every account in this response.
    pub slot: u64,
    /// Only explicit JSON null becomes None; invalid accounts fail the entire fetch.
    pub accounts: Vec<Option<OwnedAccount>>,
}

/// Endpoint and its approximate shared eligibility across concurrent attempts.
struct Provider {
    /// Caller-supplied HTTP endpoint on the same chain as the watermark source.
    url: Url,
    /// Milliseconds since the fetcher's monotonic epoch; zero means eligible.
    until: AtomicU64,
}

/// Concurrent single-batch HTTP fetching. Callers bound concurrency and supply
/// endpoints on the same chain, each supporting the standard 100-key RPC limit.
/// No background tasks, cache, splitting, or subscription coordination are provided.
pub struct Fetcher {
    /// Shared connection pool; provider selection owns retries and redirects are disabled.
    client: reqwest::Client,
    /// Nonempty endpoint list; positions are stable provider identities.
    providers: Vec<Provider>,
    /// Confirmed WebSocket watermark sampled once at fetch entry.
    slot: Arc<AtomicU64>,
    /// Rotating first candidate across concurrent fetches and failover attempts.
    cursor: AtomicUsize,
    /// Monotonic origin for provider eligibility timestamps.
    epoch: Instant,
}

impl Fetcher {
    /// Creates a pooled Rustls client with redirects and automatic retries disabled.
    /// The caller supplies a nonempty list of valid HTTP(S) endpoints on the same chain.
    pub fn new(providers: Vec<Url>, slot: Arc<AtomicU64>) -> Result<Self, Error> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        Ok(Self {
            client,
            providers: providers
                .into_iter()
                .map(|url| Provider { url, until: AtomicU64::new(0) })
                .collect(),
            slot,
            cursor: AtomicUsize::new(0),
            epoch: Instant::now(),
        })
    }

    /// Fetches 1–100 keys at confirmed commitment in one request per attempt.
    /// Captures max(min_slot, watermark) once; failover never relaxes that minimum.
    /// Attempts have a two-second budget within ten seconds overall. Dropping this
    /// future cancels its I/O; successful HTTP responses never advance the watermark.
    /// Deadlines are cooperative: they reject late success but cannot interrupt decoding.
    /// Only transient endpoint failures retry; malformed responses and decoding errors
    /// return immediately with provider context. Transient failures impose a 100 ms cooldown.
    pub async fn fetch(&self, keys: &[Pubkey], min_slot: Option<u64>) -> Result<Snapshot, Error> {
        let minimum = min_slot.unwrap_or(0).max(self.slot.load(Relaxed));
        let deadline = Instant::now() + OVERALL;
        if !(1..=100).contains(&keys.len()) {
            return Err(Error::BatchSize);
        }
        let request = Request::new(
            1,
            GET_MULTIPLE_ACCOUNTS,
            (
                keys.iter().map(ToString::to_string).collect::<Vec<_>>(),
                AccountConfig::new(Some(minimum)),
            ),
        );
        let body = Bytes::from(json::to_vec(&request)?);
        let mut last = None;
        while let Some((index, provider)) = self.available(deadline).await {
            let end = (Instant::now() + ATTEMPT).min(deadline);
            let result = time::timeout_at(
                end,
                self.attempt(provider, body.clone(), keys.len(), minimum),
            )
            .await
            .unwrap_or(Err(Error::Timeout("HTTP attempt")));
            // Synchronous decoding cannot be preempted by Tokio's timer. Do not return
            // a late success if decoding consumed the remaining attempt budget.
            let error = match result {
                Ok(snapshot) if Instant::now() < end => return Ok(snapshot),
                Ok(_) => Error::Timeout("HTTP attempt"),
                Err(error) => error,
            };
            let retry = error.retryable();
            let error = Error::Provider {
                provider: index,
                source: Box::new(error),
            };
            if !retry {
                return Err(error);
            }
            let until = (self.epoch.elapsed() + COOLDOWN).as_millis() as u64;
            provider.until.fetch_max(until, Relaxed);
            last = Some(Box::new(error));
        }
        Err(Error::Deadline { last })
    }

    /// Rotates through eligible providers, waiting only when all are cooling down.
    /// Eligibility is approximate, not a lease; concurrent requests may use the same endpoint.
    async fn available(&self, deadline: Instant) -> Option<(usize, &Provider)> {
        while Instant::now() < deadline {
            let len = self.providers.len();
            let start = self.cursor.fetch_add(1, Relaxed) % len;
            let now = self.epoch.elapsed().as_millis() as u64;
            let mut wake = deadline;
            for index in (start..len).chain(0..start) {
                let provider = &self.providers[index];
                let until = provider.until.load(Relaxed);
                if until <= now {
                    return Some((index, provider));
                }
                wake = wake.min(self.epoch + Duration::from_millis(until));
            }
            time::sleep_until(wake).await;
        }
        None
    }

    /// Requests and decodes one complete snapshot without performing failover itself.
    async fn attempt(
        &self,
        provider: &Provider,
        body: Bytes,
        count: usize,
        minimum: u64,
    ) -> Result<Snapshot, Error> {
        let response = self
            .client
            .post(provider.url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Status(response.status()));
        }
        let bytes = response.bytes().await?;
        let response: Response<'_> = json::from_slice(&bytes)?;
        if response.jsonrpc != VERSION || response.id != 1 {
            return Err(Error::Protocol("invalid HTTP response correlation"));
        }
        if response.result.is_some() == response.error.is_some() {
            return Err(Error::Protocol("invalid HTTP response envelope"));
        }
        if let Some(error) = response.error {
            return Err(Error::Rpc(json::from_str(error.as_raw_str())?));
        }
        let result = response.result.ok_or(Error::Protocol("missing HTTP result"))?;
        let result: ContextValue<Vec<Option<WireAccount<'_>>>> =
            json::from_str(result.as_raw_str())?;
        if result.value.len() != count {
            return Err(Error::Protocol("HTTP result length does not match request"));
        }
        if result.context.slot < minimum {
            return Err(Error::Protocol(
                "HTTP context is below requested minimum slot",
            ));
        }
        let slot = result.context.slot;
        let accounts = result
            .value
            .into_iter()
            .map(|account| account.map(|account| account.decode(slot)).transpose())
            .collect::<Result<_, _>>()?;
        Ok(Snapshot { slot, accounts })
    }
}

/// Correlated HTTP result or rejection, borrowed until its envelope is validated.
#[derive(Deserialize)]
struct Response<'a> {
    /// Protocol version expected for the correlated response.
    jsonrpc: &'a str,
    /// Echoed request identity, validated before decoding the payload.
    id: u64,
    /// Success payload, parsed only after envelope validation.
    #[serde(borrow)]
    result: Option<LazyValue<'a>>,
    /// Provider rejection, retained with its structured diagnostic data.
    #[serde(borrow)]
    error: Option<LazyValue<'a>>,
}
