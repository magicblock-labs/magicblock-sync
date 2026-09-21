# magicblock-sync

Base-layer account synchronization for MagicBlock. The current implementation is
the [WebSocket subscription layer](https://github.com/magicblock-labs/magicblock-validator/issues/1721)
of the [Chainlink rewrite](https://github.com/magicblock-labs/magicblock-validator/issues/1698).
HTTP snapshots, reconciliation, gRPC redundancy, and Engine integration are separate work.

## Subscriptions

Create the pool inside a Tokio runtime with networking and time enabled. Each
provider has a typed `Url` and its own connection and per-connection subscription
limits. Both `ws` and `wss` endpoints are supported.

```rust
use magicblock_sync::{Config, Event, Pool, Provider, Pubkey};

let mut pool = Pool::new(Config {
    providers: vec![Provider {
        url: "wss://api.devnet.solana.com".parse()?,
        max_connections: 8,
        subs_per_connection: 100,
    }],
});

// Wait for an empty socket to become available; failures are observable and retried.
loop {
    match pool.next().await {
        Event::Connected(_) => break,
        Event::Dropped { error, .. } => eprintln!("connection failed: {error}"),
        _ => {}
    }
}
let account: Pubkey = "SysvarC1ock11111111111111111111111111111111".parse()?;
let reservation = pool.subscribe(account)?;

loop {
    match pool.next().await {
        Event::Established(subscription) if *subscription == reservation => {
            // Coverage now exists: orchestration may start its HTTP snapshot.
            // Retain subscription to call pool.release(subscription) when finished.
        }
        Event::Update { subscription, slot, account } => {
            // Reconcile by subscription identity and slot before applying state.
        }
        Event::Dropped { reservations: lost, error, .. } => {
            // Invalidate this coverage. Orchestration decides whether to restore it.
            break;
        }
        Event::Rejected { error, .. } => return Err(error.into()),
        _ => {}
    }
}
// After requesting release, keep draining events until Released or Dropped.
// Dropping the pool closes every socket.
```

`subscribe` returns a `Reservation` identifying admitted work, not a releasable
handle. `Established` supplies a `Subscription` containing that reservation and
the provider subscription ID; only this established handle can be released.
Coverage starts with the provider's
[`accountSubscribe` acknowledgement](https://solana.com/docs/rpc/websocket/accountsubscribe),
not with admission or an initial account snapshot. MBV/Engine must ensure at most
one live subscription per account, with one lifecycle owner releasing each established
subscription at most once, without retries. `Reservation` and `Subscription` are
`Copy` identities, not additional leases. The pool does not check account uniqueness
or track duplicate releases. Pending establishment cannot be cancelled.

Drive `next` continuously, including while requests are pending. Admission and
release are synchronous and can be used alongside `next` in an orchestration
`tokio::select!` loop. `Unavailable` means no healthy socket has room or the selected
socket has just failed; `Capacity` means all configured limits are occupied.
Failed admission does not reserve capacity. The caller decides when to retry admission.

`release` returns `()`: it enqueues without waiting for remote acknowledgement and
ignores obsolete handles or already-lost socket mailboxes. Network and provider
failures remain explicit events. Capacity is retained until `next` consumes the
provider's `Released` acknowledgement or the connection's `Dropped` event. Updates
can still arrive during release.

## Pooling and loss

The pool starts with one socket per provider. New accounts take the first healthy
socket with room, starting from a rotating cursor. This approximates balance;
existing subscriptions never move just to balance load, even after uneven releases.

Growth uses a fixed pool-wide threshold of 75%: live reservations divided by the
subscription capacity of all provisioned socket slots, including connecting and
reconnecting slots and each provider's distinct limits. Pending and releasing
subscriptions count as live until rejection, release acknowledgement, or loss is
consumed. Replacing a socket does not change provisioned capacity.

Admission crossing the threshold triggers a growth round across every provider.
Each provider can add up to its number of healthy sockets, capped by its remaining
connection slots. Fresh connection attempts block another batch for that provider;
reconnects do not. Providers without healthy sockets rely on their existing attempts.
Admission with no eligible socket also attempts growth. After a connection or loss
event, growth is retried only if pool-wide utilization is still at least 75%.

Each established socket has one I/O task and batches up to 256 queued commands
without waiting for a full batch. Providers must support WebSocket JSON-RPC batch
requests when multiple commands are ready; a single request is sent as a JSON
object. No delay is added to fill a batch. Each account still has its own request ID
and acknowledgement. Allocation stops at the first eligible socket. Successful admission
uses constant-time pool accounting and scans for growth only at a threshold crossing. Growth rounds
scan sockets once per provider; updates use direct subscription-ID lookup. There is
no per-account task or lock, transport trait, or automatic resubscription.

`fastwebsockets` handles framing and fragment collection; its client helper performs
the Hyper upgrade. TLS configuration is shared across reconnects. Socket reads stay
pinned across command/timer branches. Individual frames are limited to 16 MiB;
assembled messages are checked against that limit after collection, not during
accumulation. `sonic-rs` borrows result payloads until routing is validated, then
fully decodes account updates before awaiting event delivery. Requests reuse a
serialization buffer. Invalid messages still report errors; lazy parsing does not
introduce silent update dropping.

All subscriptions request `base64+zstd` encoding with confirmed commitment; account
values stay encoded for caller-side decoding. Connection setup and RPC
acknowledgements have fixed 10-second budgets. Acknowledgement timers are driven by
`FuturesUnordered` and cancelled on response. The pool pings every 15 seconds
and requires a pong by the next tick. Writes are untimed, assuming peers continue reading.

Socket command channels are unbounded, but their occupancy is logically bounded
by the socket's subscription limit. Each live reservation has at most one queued
command: release requires establishment, which follows processing subscribe, and
release occurs at most once. Pending subscriptions count toward the hard limit
immediately, so even a burst before acknowledgements cannot exceed it. There is
no separate command-queue capacity error; 256 limits each processing batch only.

The shared event queue holds 8,192 entries. Event sends wait for queue space
instead of dropping coverage on overflow. While
delivery is blocked, that socket pauses reads, commands, and timeout checks;
deadline budgets still elapse. Keep draining `next` to let socket processing progress.

Disconnects, protocol errors, and acknowledgement/heartbeat timeouts produce
`Dropped` with the old connection identity and all affected
reservations, including queued requests and pending establishment. Only the affected
socket's reservation map is drained. The socket closes before this terminal event
waits for queue space.
Already queued events from that socket precede its loss notification. Replacements
are empty, have new identities, and retry with exponential backoff from one to
thirty seconds. A rejected unsubscribe also closes the socket because its remote
capacity cannot safely be reclaimed.

Updates preserve context slots and encoded account values. Different providers
have no shared event order; this layer neither deduplicates updates nor interprets
absence as undelegation. Stale subscription handles cannot release replacement
coverage. Handles belong to the pool that issued them.

The original LaserStream dependency and prototype service are removed. Their
replacement belongs to the separate gRPC work.
See the crate API documentation for configuration and event contracts.
