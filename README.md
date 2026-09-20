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
        subscriptions_per_connection: 100,
    }],
})?;

// Wait for an empty socket to become available; failures are observable and retried.
loop {
    match pool.next().await {
        Event::Connected(_) => break,
        Event::Dropped { error, .. } => eprintln!("connection failed: {error}"),
        _ => {}
    }
}
let account: Pubkey = "SysvarC1ock11111111111111111111111111111111".parse()?;
let handle = pool.subscribe(account)?;

loop {
    match pool.next().await {
        Event::Established(id) if id == handle => {
            // Coverage now exists: orchestration may start its HTTP snapshot.
        }
        Event::Update { subscription, slot, account } => {
            // Reconcile by subscription identity and slot before applying state.
        }
        Event::Dropped { subscriptions: lost, error, .. } => {
            // Invalidate this coverage. Orchestration decides whether to restore it.
            break;
        }
        Event::Rejected { error, .. } => return Err(error.into()),
        _ => {}
    }
}
// To stop following an account, call pool.release(handle) and keep
// draining events until Released or Dropped. Dropping the pool closes every socket.
```

`subscribe` only reserves capacity. Coverage starts with the provider's
[`accountSubscribe` acknowledgement](https://solana.com/docs/rpc/websocket/accountsubscribe),
not with admission or an initial account snapshot. Calls for an already reserved
account return `Duplicate` with its existing handle; handles are not reference-counted.

Drive `next` continuously, including while requests are pending. Admission and
release are synchronous and can be used alongside `next` in an orchestration
`tokio::select!` loop. `Busy` means command-queue backpressure; `Unavailable` means
capacity is not connected yet; `Capacity` means all configured limits are occupied.
Failed admission does not reserve capacity. Release retains capacity until the
provider acknowledges it, even when requested before establishment. In-flight
establishment and update events can still arrive while release is pending.

## Pooling and loss

The pool starts with one socket per provider. It doubles a provider's sockets at
25%, 37.5%, 50%, 62.5%, then 75% occupancy, capped by that provider's limit. Only
one growth batch per provider is in flight. Pending and releasing subscriptions
count toward occupancy. New accounts use the least-loaded available sockets with
rotating ties; existing subscriptions never move just to balance load.

Each established socket has one I/O task and pipelines RPC requests. The registry scans sockets,
not accounts, for allocation; updates use direct subscription-ID lookup. There is
no per-account task or lock, transport trait, or automatic resubscription.

`fastwebsockets` handles framing and its client helper performs the Hyper upgrade.
TLS configuration is shared across reconnects. Socket reads stay pinned across
command/timer branches; fragmented messages are bounded to 16 MiB. `sonic-rs`
borrows result payloads as lazy values until routing and delivery-capacity checks
pass, then fully decodes accepted account updates. Requests reuse a serialization
buffer. Invalid or undeliverable messages still report errors; lazy parsing does
not introduce silent update dropping.

All subscriptions use confirmed commitment. Connection setup and RPC
acknowledgements have fixed 10-second deadlines. The pool pings every 15 seconds
and requires a pong by the next tick. Writes are untimed, assuming peers continue reading.

Command queues hold up to 256 entries per socket; the shared event queue holds
8,192 entries. These bounds are fixed, while provider limits remain configurable.

Disconnects, protocol errors, acknowledgement/heartbeat timeouts, and event-queue
overflow produce `Dropped` with the old connection identity and all affected
reservations. The socket closes before this terminal event waits for queue space.
Already queued events from that socket precede its loss notification. Replacements
are empty, have new identities, and retry with exponential backoff from one to
thirty seconds. A rejected unsubscribe also closes the socket because its remote
capacity cannot safely be reclaimed.

Updates preserve context slots and encoded account values. Different providers
have no shared event order; this layer neither deduplicates updates nor interprets
absence as undelegation. Stale subscription handles cannot release replacement
coverage. Handles belong to the pool that issued them.

The original LaserStream dependency is retained in the build scaffolding; the
prototype service is removed. Its replacement belongs to the separate gRPC work.
See the crate API documentation for configuration and event contracts.
