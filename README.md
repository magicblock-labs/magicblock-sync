# magicblock-sync

Fetch account snapshots over HTTP and follow changes over WebSocket, with decoded
Engine accounts from both transports. Provider failover and a shared confirmed
account-update watermark (the highest observed Solana context slot) keep fetching
independent of any one endpoint.

Part of the [Chainlink rewrite](https://github.com/magicblock-labs/magicblock-validator/issues/1698).
`ChainSync::sync` fetches accounts missing from Engine in batches of up to 100 and
materializes them, including HTTP `null` responses as default accounts. Discovery,
subscription-before-fetch coordination, and reconciliation remain caller
responsibilities. Direct `Fetcher` users receive accounts in `Uninit` mode and
must classify them before materialization when their workflow requires it.

## Fetching

Create a `Fetcher` with a nonempty list of valid HTTP(S) endpoints and the pool's
shared watermark. Endpoint validity is a caller contract, not a constructor check.
All HTTP and WebSocket providers must belong to the same chain.

```rust
use magicblock_sync::http::Fetcher;

let fetcher = Fetcher::new(
    vec!["https://api.devnet.solana.com".parse()?],
    pool.slot(),
)?;
// After subscribe succeeds, fetch while the event consumer keeps running.
let snapshot = fetcher.fetch(&[account], None).await?;
// snapshot.accounts follows input order, including duplicates.
```

Each call accepts 1–100 keys and sends one
[`getMultipleAccounts`](https://solana.com/docs/rpc/http/getmultipleaccounts) request
per attempt. Every endpoint must support the standard 100-key limit. There is no
splitting or coalescing. Put related accounts in the same batch when they need one
response context; separate fetches may return different slots. Only explicit JSON
`null` produces `None`; invalid account data fails the operation, never a partial snapshot.

At entry, fetching captures the greater of the caller's `min_slot` and the shared
watermark. It never relaxes that floor during retries. The watermark starts at
zero and records the highest observed confirmed account-update context slot, not a guaranteed
current chain head. Disconnects retain it; HTTP responses do not advance it.
Every valid WebSocket account update advances it, including explicit absence.
Malformed account data does not advance it.

Concurrent calls share a pooled Rustls client, round-robin provider selection, and
approximate provider cooldowns. Each attempt gets up to two seconds within a
ten-second overall budget. Retryable failures cool the provider for a fixed 100 ms;
an in-flight success does not clear that cooldown early. Other eligible
providers are tried immediately, or the call waits for the earliest cooldown.
Transport failures, timeouts, HTTP 408/429/5xx, node-unhealthy and minimum-slot RPC
errors retry. Parsing and account-decoding failures, malformed responses, wrong
result lengths, below-floor responses, and other HTTP/RPC errors return immediately.
Errors retain the provider index and cause; deadline exhaustion
retains the latest failure when available. Redirects and client-level retries are disabled.

Deadlines are cooperative: synchronous account decoding cannot be interrupted,
but a response that finishes decoding after its attempt budget is not returned as success.

Callers bound concurrency: there is no semaphore, account cache, or background
health probing. Dropping a fetch future cancels its I/O without detached work.

## Subscriptions

Create the pool inside a Tokio runtime with networking and time enabled. It returns
a cloneable command handle and a single event receiver. Subscribe and unsubscribe
by pubkey; the library keeps the provider subscription IDs and routing internally.

```rust
use magicblock_sync::websocket::{Config, Event, Pool, Provider};
use solana_pubkey::Pubkey;

let (pool, mut events) = Pool::new(Config {
    providers: vec![Provider {
        url: "wss://api.devnet.solana.com".parse()?,
        max_connections: 8,
        subs_per_connection: 100,
    }],
});

// Subscribe fails fast while connections are opening. Wait for initial readiness.
while let Some(event) = events.recv().await {
    match event {
        Event::Connected(_) => break,
        Event::Dropped { error, .. } => eprintln!("connection failed: {error}"),
        _ => {}
    }
}

// Drain events concurrently, including while commands await acknowledgement.
let consumer = tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        match event {
            Event::Update { pubkey, slot, account } => {
                // Reconcile this observation with the HTTP snapshot before applying it.
                println!("{pubkey} at {slot}: present={}", account.is_some());
            }
            Event::Dropped { pubkeys, error, .. } => {
                // These accounts lost their subscriptions. Orchestration decides what to restore.
                eprintln!("lost {} subscriptions: {error}", pubkeys.len());
            }
            Event::Connected(_) => {}
        }
    }
});

let account: Pubkey = "11111111111111111111111111111111".parse()?;
pool.subscribe(account).await?;
// The server acknowledged subscribe: fetch and reconcile the HTTP snapshot here.
// Later, when the subscription is no longer needed:
pool.unsubscribe(account).await?;

drop(pool); // Dropping the last handle stops the registry and all socket tasks.
consumer.await?;
```

Each admitted operation returns after the server responds and the library updates
its registry. Successful subscribe confirms the server's acknowledgement,
**not an initial account snapshot**.
Successful unsubscribe frees capacity, but updates already buffered in the event
queue can still arrive afterward. There are no public subscription handles or
separate acknowledgement events.

Clone `Pool` to operate on different accounts concurrently. Calls for the **same
pubkey must not overlap**, and callers must not cancel their operation futures.
Subscribe only when the account has no existing subscription or pending operation, and
unsubscribe only after successful subscribe. There is no duplicate-subscribe handling
or reference counting. Unsubscribe succeeds if connection loss already removed the
subscription. If a waiter disappears, accepted requests still finish internally rather than
rolling back.

`Unavailable` means no healthy socket currently has room; `Capacity` means all
configured limits are occupied. Neither reserves capacity or waits for a new socket.
The caller decides when to retry. Subscribe rejection returns the provider's RPC
error. Socket failure completes pending calls with `Disconnected`; the `Dropped`
event carries the precise cause and affected pubkeys. Closing or dropping the event
receiver also stops the pool, completing outstanding calls with `Closed`.

## Pooling and loss

The pool starts with one socket per provider. Each initial socket reserves one
subscription for internal confirmed Clock tracking before accepting user requests, including
when reconnecting. Additional connections do not subscribe to Clock. This subscription counts
toward hard capacity and utilization, but its establishment and updates are hidden.
Clock keeps the watermark advancing even when there are no user subscriptions;
its updates follow the same decoding and watermark path as other accounts.
Clock is reserved: callers must never pass its pubkey to `subscribe` or `unsubscribe`.
A provider rejection of Clock or an invalid notification closes that socket through
the normal `Dropped` and reconnect path.

New accounts take the first healthy socket with room, starting from a rotating
cursor. This approximates balance;
existing subscriptions never move just to balance load, even after uneven releases.

Pool growth starts at 75% utilization: occupied subscription capacity divided by
the total capacity of all connection-pool entries. Each entry contributes its
provider's per-connection limit, even while connecting or reconnecting. Clock and
pending subscribe/unsubscribe requests occupy capacity until the registry processes
rejection, unsubscribe acknowledgement, or connection loss. Reconnecting an
existing pool entry does not change total capacity.

Accepting a subscribe request that crosses the threshold triggers growth across every provider.
Each provider can add up to its number of healthy sockets, capped by its remaining
connection limit. Initial connection attempts block another growth batch for that provider;
reconnects do not. Providers without healthy sockets rely on their existing attempts.
A subscribe request with no eligible connection also attempts growth. After a connection or loss
event, growth is retried only if pool-wide utilization is still at least 75%.

One background task owns pool routing and lifecycle bookkeeping. Each socket has
one I/O task and batches up to 256 queued commands
without waiting for a full batch. Providers must support WebSocket JSON-RPC batch
requests when multiple commands are ready; a single request is sent as a JSON
object. No delay is added to fill a batch. Each account still has its own request ID
and acknowledgement. Allocation stops at the first eligible socket. Successful admission
uses constant-time pool accounting and scans for growth only at a threshold crossing. Growth rounds
scan sockets once per provider. Updates use direct subscription-ID lookup and go
straight from the socket task to the consumer queue, bypassing the pool registry. There is
no per-account task or lock, transport trait, or automatic user resubscription.

`fastwebsockets` handles framing and fragment collection; its client helper performs
the Hyper upgrade. TLS configuration is shared across reconnects. Socket reads stay
pinned across command/timer branches. Individual frames are limited to 16 MiB;
assembled messages are checked against that limit after collection, not during
accumulation. `sonic-rs` borrows result payloads until routing is validated, then
fully decodes account updates before awaiting event delivery. Requests reuse a
serialization buffer. Invalid messages still report errors; lazy parsing does not
introduce silent update dropping.

All subscriptions request `base64+zstd` encoding with confirmed commitment;
account values are decoded into Engine `OwnedAccount`s using the response context slot.
Only base64+zstd responses are accepted. Connection setup and RPC
acknowledgements have fixed 10-second budgets. Acknowledgement timers are driven by
`FuturesUnordered` and cancelled on response. The pool pings every 15 seconds
and requires a pong by the next tick. Writes are untimed, assuming peers continue reading.

The shared client command queue holds 256 requests; sending waits for space.
Socket command channels are unbounded, but their occupancy is logically bounded
by subscription capacity and the non-overlapping-operation contract. Internal
acknowledgements use a separate channel, bounded logically by admitted work and
socket count. Pending subscriptions count toward the hard limit immediately, so
a burst before acknowledgements cannot exceed it.

The shared event queue holds 8,192 entries. Event sends wait for queue space
instead of dropping updates or disconnecting when the queue is full. While
delivery is blocked, that socket pauses reads, commands, and timeout checks;
deadline budgets still elapse. The registry can also wait for space to report connection
events. Keep consuming the receiver concurrently with subscription operations.

Disconnects, protocol errors, and acknowledgement/heartbeat timeouts produce
`Dropped` with the old connection identity and all affected user pubkeys, including
pending establishment and unsubscription; Clock is omitted. The registry removes only
the affected socket's accounts and completes pending calls before publishing loss.
The socket closes before this terminal event waits for queue space.
Already queued events from that socket precede its loss notification. Replacements
restore only their internal Clock subscription, have new identities, and retry with
exponential backoff from one to thirty seconds. A rejected unsubscribe also closes the socket because its remote
capacity cannot safely be reclaimed.

Updates preserve Solana context slots and return decoded account values. Different providers
have no shared event order; this layer neither deduplicates updates nor interprets
absence as undelegation. Updates may be consumed before the subscribe future resumes,
and buffered updates may outlive an unsubscribe call. Pubkeys are account identities,
not subscription-generation tokens; reconciliation remains the caller's responsibility.

`grpc::Client` provides Yellowstone retained-account subscriptions and delegation
lifecycle observations alongside the WebSocket pool. It reuses upstream automatic
reconnect; orchestration owns deduplication and gap reconciliation. See its Rustdoc
for discovery assumptions and event contracts.
