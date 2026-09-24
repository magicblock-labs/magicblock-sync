use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering::*},
        Arc,
    },
    time::Duration,
};

use hyper::{body::Bytes, header::CONTENT_TYPE};
use json::LazyValue;
use reqwest::{redirect::Policy, retry, Client};
use serde::{Deserialize, Serialize};
use solana_account::OwnedAccount;
use solana_pubkey::Pubkey;
use tokio::time::{self, Instant};
use url::Url;

use crate::rpc::{AccountConfig, ContextValue, Request, WireAccount};

use super::Error;

/// JSON-RPC method for fetching multiple accounts in one response context.
const GET_MULTIPLE_ACCOUNTS: &str = "getMultipleAccounts";

/// Total budget across provider selection, cooldown waits, and attempts.
const OVERALL: Duration = Duration::from_secs(10);
/// Maximum time for one provider, capped by the remaining overall budget.
const ATTEMPT: Duration = Duration::from_secs(2);
/// Shared pause after a transient provider failure; successes do not clear it early.
const COOLDOWN: Duration = Duration::from_millis(100);

/// One response context with accounts in input order, including duplicate keys.
/// Accounts retain `Uninit` mode for classification before materialization.
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

/// Positional RPC arguments for one account batch.
#[derive(Serialize)]
struct BatchParams(Vec<String>, AccountConfig);

/// Provider selected for an attempt, with its stable error-reporting index.
struct Candidate<'a> {
    /// Stable provider position reported with attempt failures.
    index: usize,
    /// Selected endpoint, including cooldown state shared by concurrent fetches.
    provider: &'a Provider,
}

/// Fetches account batches over HTTP with provider failover.
/// Callers bound concurrency and supply same-chain endpoints that support the
/// standard 100-key RPC limit. This fetcher does not split batches or manage
/// subscriptions.
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
        let client = Client::builder().redirect(Policy::none()).retry(retry::never()).build()?;
        let providers = providers
            .into_iter()
            .map(|url| Provider { url, until: AtomicU64::new(0) })
            .collect();
        Ok(Self {
            client,
            providers,
            slot,
            cursor: AtomicUsize::new(0),
            epoch: Instant::now(),
        })
    }

    /// Fetches 1–100 keys at confirmed commitment in one request per attempt.
    /// The request uses the greater of `min_slot` and the shared watermark,
    /// captured once; failover never lowers this floor. HTTP responses do not
    /// advance the watermark.
    ///
    /// Transient failures retry with a 100 ms provider cooldown. Malformed
    /// responses and account decoding errors return immediately with provider
    /// context. Each attempt has up to two seconds within a ten-second total
    /// budget. Dropping this future cancels HTTP I/O, but synchronous decoding
    /// may outlive the attempt budget.
    pub async fn fetch(&self, keys: &[Pubkey], min_slot: Option<u64>) -> Result<Snapshot, Error> {
        let minimum = min_slot.unwrap_or(0).max(self.slot.load(Relaxed));
        let deadline = Instant::now() + OVERALL;
        if !(1..=100).contains(&keys.len()) {
            return Err(Error::BatchSize);
        }
        let params = BatchParams(
            keys.iter().map(ToString::to_string).collect::<Vec<_>>(),
            AccountConfig::new(Some(minimum)),
        );
        let request = Request::new(1, GET_MULTIPLE_ACCOUNTS, params);
        let body = Bytes::from(json::to_vec(&request)?);
        let mut last = None;
        while let Some(Candidate { index, provider }) = self.available(deadline).await {
            let end = (Instant::now() + ATTEMPT).min(deadline);
            let result = self.attempt(provider, body.clone(), end).await;
            let error = match result {
                Ok(snapshot) => return Ok(snapshot),
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
    async fn available(&self, deadline: Instant) -> Option<Candidate<'_>> {
        while Instant::now() < deadline {
            let len = self.providers.len();
            let start = self.cursor.fetch_add(1, Relaxed) % len;
            let now = self.epoch.elapsed().as_millis() as u64;
            let mut wake = deadline;
            for index in (start..len).chain(0..start) {
                let provider = &self.providers[index];
                let until = provider.until.load(Relaxed);
                if until <= now {
                    return Some(Candidate { index, provider });
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
        end: Instant,
    ) -> Result<Snapshot, Error> {
        let response = self
            .client
            .post(provider.url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .timeout(end.saturating_duration_since(Instant::now()))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(Error::Status(response.status()));
        }
        let bytes = response.bytes().await?;
        let response: Response<'_> = json::from_slice(&bytes)?;
        if let Some(error) = response.error {
            return Err(Error::Rpc(json::from_str(error.as_raw_str())?));
        }
        let result = response.result.ok_or(Error::Protocol("missing HTTP result"))?;
        let result: ContextValue<Vec<Option<WireAccount<'_>>>> =
            json::from_str(result.as_raw_str())?;
        let slot = result.context.slot;
        let accounts = result
            .value
            .into_iter()
            .map(|account| account.map(|account| account.decode(slot)).transpose())
            .collect::<Result<_, _>>()?;
        Ok(Snapshot { slot, accounts })
    }
}

/// HTTP result or rejection, borrowing its payload until it is decoded.
#[derive(Deserialize)]
struct Response<'a> {
    /// Success payload, decoded after the outer response.
    #[serde(borrow)]
    result: Option<LazyValue<'a>>,
    /// Provider rejection, retained with its structured diagnostic data.
    #[serde(borrow)]
    error: Option<LazyValue<'a>>,
}
