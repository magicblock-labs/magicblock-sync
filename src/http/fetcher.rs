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
use solana_account::AccountBuilder;
use solana_pubkey::Pubkey;
use tokio::time::{self, Instant};
use url::Url;

use crate::rpc::{AccountConfig, ContextValue, Request, WireAccount};

use super::Error;

/// RPC operation that returns one shared context for the batch.
const GET_MULTIPLE_ACCOUNTS: &str = "getMultipleAccounts";

/// Total budget across attempts and provider cooldowns.
const OVERALL: Duration = Duration::from_secs(10);
/// Per-provider attempt budget, capped by the overall deadline.
const ATTEMPT: Duration = Duration::from_secs(2);
/// Shared delay after transient provider failure.
const COOLDOWN: Duration = Duration::from_millis(100);

/// Confirmed account snapshot in request order, with accounts in `Uninit` mode.
pub struct Snapshot {
    /// Context slot shared by the batch.
    pub slot: u64,
    /// `None` only for an explicit RPC null; invalid accounts fail the batch.
    pub accounts: Vec<Option<AccountBuilder>>,
}

/// Endpoint with eligibility shared across concurrent fetches.
struct Provider {
    /// Configured same-chain HTTP endpoint.
    url: Url,
    /// Milliseconds since `Fetcher::epoch` when this endpoint becomes eligible.
    until: AtomicU64,
}

/// Positional arguments for one account batch.
#[derive(Serialize)]
struct BatchParams(
    /// Requested pubkeys in response order.
    Vec<String>,
    /// Shared encoding, finality, and slot floor.
    AccountConfig,
);

/// Selected provider and its stable error-reporting index.
struct Candidate<'a> {
    /// Position in the configured endpoint list.
    index: usize,
    /// Endpoint and its shared cooldown state.
    provider: &'a Provider,
}

/// Fetches confirmed account batches with same-chain provider failover.
/// Callers split batches and manage subscriptions.
pub struct Fetcher {
    /// Reusable HTTP connections without implicit redirects or retries.
    client: reqwest::Client,
    /// Stable endpoint order used for error reporting.
    providers: Vec<Provider>,
    /// Confirmed WebSocket watermark sampled at fetch entry.
    slot: Arc<AtomicU64>,
    /// Rotating first candidate for provider selection.
    cursor: AtomicUsize,
    /// Monotonic origin for cooldown timestamps.
    epoch: Instant,
}

impl Fetcher {
    /// Uses a nonempty list of same-chain HTTP(S) endpoints.
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

    /// Fetches 1–100 keys at confirmed commitment. The greater of `min_slot`
    /// and the shared watermark sets a floor that remains fixed across failover.
    /// HTTP responses do not advance the watermark.
    ///
    /// Transient failures retry within a ten-second budget. Malformed responses
    /// fail immediately. Cancelling stops HTTP I/O, but not synchronous decoding.
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

    /// Chooses an eligible provider, waiting only when all are cooling down.
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

    /// Fetches and decodes one complete snapshot from the chosen provider.
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

/// Borrowed success or provider rejection before payload decoding.
#[derive(Deserialize)]
struct Response<'a> {
    /// Success payload decoded after outer-envelope validation.
    #[serde(borrow)]
    result: Option<LazyValue<'a>>,
    /// Structured provider rejection retained for diagnostics.
    #[serde(borrow)]
    error: Option<LazyValue<'a>>,
}
