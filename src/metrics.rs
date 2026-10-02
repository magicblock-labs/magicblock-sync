//! Process-wide ChainSync metrics; provider indices are scoped by transport.

use std::sync::OnceLock;

use nucleus::metrics::{
    counter, counter_vec, gauge_vec, with_metrics, IntCounter, IntCounterVec, IntGaugeVec,
    MetricOperation, MetricSpec, OperationCounters, OperationTimer,
};

use crate::http::{Error as HttpError, Outcome};

/// Shared collectors registered once for all ChainSync instances.
static METRICS: OnceLock<Metrics> = OnceLock::new();

/// Low-cardinality timing boundaries within account synchronization.
#[derive(Clone, Copy)]
pub(crate) enum Op {
    /// All acquisition waves, dependencies, leases, and materialization.
    Sync,
    /// A complete snapshot fetch, including retries and cooldown waits.
    HttpFetch,
    /// A coverage-admitted streamed update, including its Engine lease.
    Apply,
    /// Non-confined delegation materialization, including assessment and rescue.
    Delegate,
    /// One account's ownership-return handler, including any ATA resolution.
    Undelegate,
    /// One provider request, including response decoding.
    HttpAttempt,
}

impl MetricOperation for Op {
    fn label(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::HttpFetch => "http_fetch",
            Self::Apply => "apply",
            Self::Delegate => "delegate",
            Self::Undelegate => "undelegate",
            Self::HttpAttempt => "http_attempt",
        }
    }
}

/// Subscription source and transport-failure label shared by the workers.
#[derive(Clone, Copy)]
pub(crate) enum Transport {
    WebSocket,
    Grpc,
}

impl Transport {
    fn label(self) -> &'static str {
        match self {
            Self::WebSocket => "ws",
            Self::Grpc => "grpc",
        }
    }
}

/// Process-wide collectors; labels are selected at each recording boundary.
struct Metrics {
    /// Elapsed microseconds grouped by operation.
    durations: OperationCounters,
    /// Current logical subscriptions grouped by confirmed source combination.
    coverage: IntGaugeVec,
    /// Unexpected losses of a logical subscription's final source.
    losses: IntCounterVec,
    /// Failed activations that enter rescue, regardless of its outcome.
    activation_failures: IntCounter,
    /// Rescue scheduling results and superseded-image decisions.
    rescues: IntCounterVec,
    /// Returned HTTP attempts grouped by provider and classified outcome.
    http: IntCounterVec,
    /// WS attempt and gRPC outer-session failures grouped by provider.
    transport: IntCounterVec,
}

/// Registers namespaced collectors once; subsequent calls reuse them.
pub(crate) fn init() {
    METRICS.get_or_init(|| Metrics {
        durations: OperationCounters::new(spec(
            "sync_duration_micros",
            "ChainSync operation duration in microseconds",
        )),
        coverage: gauge_vec(
            spec("sync_coverage", "Logical subscriptions by coverage"),
            &["coverage"],
        ),
        losses: counter_vec(
            spec("sync_coverage_losses", "Unexpected final-source losses"),
            &["source"],
        ),
        activation_failures: counter(
            spec("sync_activation_failures", "Entries into activation rescue"),
            0,
        ),
        rescues: counter_vec(
            spec("sync_rescues", "Rescue decisions and scheduling results"),
            &["outcome"],
        ),
        http: counter_vec(
            spec("sync_http_attempts", "HTTP attempt results by provider"),
            &["provider", "outcome"],
        ),
        transport: counter_vec(
            spec(
                "sync_transport_failures",
                "WS attempt and gRPC outer-session failures",
            ),
            &["transport", "provider"],
        ),
    });
}

fn spec(name: &'static str, help: &'static str) -> MetricSpec {
    MetricSpec { name, help }
}

/// Records elapsed microseconds on drop; a no-op before initialization.
pub(crate) fn time(op: Op) -> OperationTimer<'static> {
    op.time(METRICS.get().map(|metrics| &metrics.durations))
}

/// Moves one logical subscription between `ws_only`, `grpc_only`, and `both`.
/// `None` means no confirmed coverage; unchanged states do not update gauges.
pub(crate) fn coverage(before: Option<&'static str>, after: Option<&'static str>) {
    if before == after {
        return;
    }
    with_metrics(&METRICS, |metrics| {
        if let Some(before) = before {
            metrics.coverage.with_label_values(&[before]).dec();
        }
        if let Some(after) = after {
            metrics.coverage.with_label_values(&[after]).inc();
        }
    });
}

/// Counts an unexpected final-source loss, not an intentional removal.
pub(crate) fn lost(source: Transport) {
    with_metrics(&METRICS, |metrics| {
        metrics.losses.with_label_values(&[source.label()]).inc()
    });
}

pub(crate) fn activation_failed() {
    with_metrics(&METRICS, |metrics| metrics.activation_failures.inc());
}

/// Counts a `scheduled`, `superseded`, or `failed` rescue outcome.
/// Scheduling success means Engine accepted materialization, not on-chain undelegation.
pub(crate) fn rescue(outcome: &'static str) {
    with_metrics(&METRICS, |metrics| {
        metrics.rescues.with_label_values(&[outcome]).inc()
    });
}

/// Counts one failed WS attempt or gRPC outer session by provider index.
/// Yellowstone's internal reconnects do not enter this boundary.
pub(crate) fn transport(provider: usize, transport: Transport) {
    with_metrics(&METRICS, |metrics| {
        metrics
            .transport
            .with_label_values(&[transport.label(), &provider.to_string()])
            .inc();
    });
}

/// Counts a returned HTTP attempt using the retry policy's error classification.
pub(crate) fn http_attempt<T>(provider: usize, result: &Result<T, HttpError>) {
    with_metrics(&METRICS, |metrics| {
        let outcome = result.as_ref().err().map_or(Outcome::Success, HttpError::outcome);
        metrics.http.with_label_values(&[&provider.to_string(), outcome.label()]).inc();
    });
}
