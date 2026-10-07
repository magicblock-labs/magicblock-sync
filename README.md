# MagicBlock ChainSync

Synchronizes Solana base-chain accounts into MagicBlock Engine for local execution.
Request the accounts a transaction needs; ChainSync acquires missing state and keeps
subscribed accounts up to date. Updates are applied directly to Engine, not exposed
as a separate stream for callers to consume.

## Usage

Create one `ChainSync` for an Engine, with at least one HTTP provider and one gRPC
stream, and WebSocket capacity for the accounts you need to track. All providers
must serve the same base chain.

HTTP providers must support confirmed `getMultipleAccounts` with `base64+zstd`
encoding; WebSocket providers must support confirmed account subscriptions with
the same encoding. Yellowstone gRPC streams provide delegation lifecycle events
and delayed redundancy for WebSocket subscriptions.

```no_run
use std::{error::Error, sync::Arc, time::Duration};

use engine::Engine;
use magicblock_chainsync::{
    AccountProperty, ChainSync, ChainSyncAccount, ChainSyncConfig, GrpcStreamConfig,
    WebSocketConfig, WebSocketProvider,
};
use nucleus::shutdown::ShutdownManager;
use solana_pubkey::Pubkey;

async fn synchronize(
    engine: Engine,
    shutdown: &mut ShutdownManager,
    payer: Pubkey,
    account: Pubkey,
) -> Result<Arc<ChainSync>, Box<dyn Error>> {
    let config = ChainSyncConfig {
        aml: None,
        http: vec!["https://rpc.example.com".parse()?],
        websocket: WebSocketConfig {
            providers: vec![WebSocketProvider {
                url: "wss://rpc.example.com".parse()?,
                max_connections: 4,
                subs_per_connection: 100,
            }],
        },
        grpc: vec![GrpcStreamConfig {
            endpoint: "https://yellowstone.example.com".parse()?,
            token: None,
            duplication_delay: Duration::from_secs(30 * 60),
        }],
    };
    let chain_sync = ChainSync::new(engine, config, shutdown)?;
    chain_sync.sync([
        ChainSyncAccount { pubkey: payer, property: AccountProperty::Payer },
        ChainSyncAccount { pubkey: account, property: AccountProperty::Readonly },
    ])
    .await?;
    Ok(chain_sync)
}
```

Replace the example endpoints and capacity limits with your provider settings.
Construction starts background services but does not wait for providers to connect;
an immediate synchronization request can fail if no WebSocket connection is ready.
The services participate in coordinated shutdown through the supplied manager.

## Synchronization flow

`sync` acquires only accounts missing from Engine. It does not refresh resident
accounts or add subscriptions for them. Choose each account's role according to
its use in the transaction:

| Property | Behavior for a missing account |
| --- | --- |
| `Payer` | Fetches the account and its delegation record; keeps live updates unless an accepted delegation is found. |
| `Readonly` | Fetches the account with live updates; refetches with a companion if it discovers DLP ownership or a Loader V3 program. |
| `Writable` | Acquires the account and checks for delegation, without subscribing to ordinary base-chain updates. |
| `Program` | Acquires an executable image and tracks upgrades, including Loader V3 ProgramData. |

Acquisition proceeds in waves:

1. **Check local presence.** Engine rechecks missing accounts under account leases,
   so overlapping syncs do not fetch accounts another request has already installed.
2. **Subscribe, then fetch.** Where the role requires live updates, WebSocket
   acknowledgement precedes HTTP acquisition. Batches contain at most 100 addresses,
   including delegation-record and ProgramData companions.
3. **Resolve the image.** Apply the branches below. A `Readonly` account that needs
   a companion is refetched in the next wave without installing its incomplete
   image or dropping its existing subscription. The refetch cannot use an older
   snapshot than the one that revealed the companion.
4. **Resolve action dependencies.** Delegations with post-delegation actions wait
   while their account and program dependencies go through the same acquisition
   flow. Nested dependencies activate before the delegation that needs them.

| Acquired state | Local result |
| --- | --- |
| Ordinary account, with no accepted delegation | HTTP image in `Uninit` mode, even if funded. Later ordinary stream updates select the mirror mode described below. |
| HTTP `null` | Default `Uninit` account at the response slot, caching the absence. |
| Program acquisition | Supported loader layouts become a rent-funded, `ReadOnly` ELF image. Loader V3 uses ProgramData under the program's address; other loaders use the program account itself. Unsupported or invalid images fail acquisition. |
| DLP-owned account and record for this Engine's authority | Original owner and delegation slot restored; enters `Delegated` mode, with actions handled after dependencies. |
| DLP-owned account and record with the default authority | Original owner restored; enters confined `Magic` mode with zero lamports, without actions or commit authority. |
| Missing, invalid, or other-authority delegation record | Does not establish local delegation; the account remains an ordinary snapshot. |
| Canonical token ATA with a valid eATA delegation to this Engine | Projects the delegated balance onto the ATA, preserving its token program and extensions; enters `Delegated` mode. Without a valid projection, the base ATA follows ordinary acquisition. |
| Raw eATA | Not materialized or kept as an ordinary subscription. Streamed eATA delegations can instead update a matching ATA already present in Engine. |

Accepted delegations stop ordinary mirroring of the local account. For projected
ATAs, local closure is disabled and native-token lamports retain only the rent
reserve; the delegated token balance is not spendable as native lamports.

Successful `sync` returns the number of HTTP account entries fetched, including
companions, action dependencies, and null results. Later-wave refetches count
again; provider retries do not. Empty or entirely resident requests return zero.
Background lifecycle work and AML requests are not included.

## Keeping accounts up to date

ChainSync separates ordinary mirrors from locally authoritative accounts:

| Path | What it does |
| --- | --- |
| WebSocket account subscription | Supplies confirmed updates for acquired mirrors. |
| Delayed gRPC copy | Supplies the same account updates through one assigned stream after its duplication delay and a filter rebuild. |
| Mandatory gRPC lifecycle filters | Observe DLP-owned accounts, accepted delegation records, undelegation requests, and successful DLP transactions independently of ordinary subscriptions. Every configured stream carries these filters. |

Ordinary non-program updates become `ReadOnly` while funded and `Uninit` at zero
lamports. They do not infer delegation from ownership alone. Program updates are
normalized to ELF; a Loader V3 ProgramData update replaces the local program image,
not a separate local ProgramData account.

Only currently covered subscriptions may apply ordinary updates. Released
subscriptions, obsolete gRPC confirmations, and updates from the wrong stream are
ignored. Engine enforces lifecycle transitions and slot ordering: older images and
same-slot, same-mode duplicates are skipped; ordinary mirrors cannot overwrite a
`Delegated` or `Magic` account. Per-account background application failures are
logged, not returned to an earlier `sync` caller.

## Delegation and undelegation

**Delegation detection** uses two confirmed gRPC account updates: the funded,
DLP-owned application account and its derived delegation-record PDA. Either may
arrive first, but both must arrive on the same stream in the same slot, and the
record's delegation slot must equal that slot. Only this Engine's authority or the
default authority is accepted; unmatched observations are discarded when the
stream changes slots. HTTP acquisition validates account and record snapshots
instead and can restore an existing delegation without observing its creation.

On a matched delegation, ChainSync retires ordinary mirror coverage before
activation. A local delegation restores ownership, acquires action dependencies,
then materializes the account and executes its actions in one Engine transaction.
A confined account is materialized directly, without dependencies, actions, or
rescue. An eATA delegation activates only through a matching resident ATA; it does
not create an ATA that was never acquired.

Optional AML assessment checks distinct post-delegation action signers. Rejected
signers, invalid actions, and activation failures enter rescue undelegation unless
local state already supersedes that delegation. Assessment service failures and
dependency-acquisition failures propagate without rescue. Successful rescue
scheduling can make `sync` succeed, but does not mean the intended actions ran or
on-chain undelegation completed.

**Undelegation has two separate signals:**

| Signal | Detection and application |
| --- | --- |
| Request to commit and undelegate | A funded DLP-owned undelegation-request account at its derived PDA identifies the target. ChainSync schedules the current local account only if it is `Delegated` and the request is not older than its delegation slot. Scheduling moves it to `Transient`; repeated requests do not reschedule it. |
| Confirmed undelegation | A successful DLP transaction invokes `Undelegate` or timeout rollback, directly or through CPI. ChainSync removes eligible local state only if the event is not older than it. `Transient` accounts may be removed; still-authoritative `Delegated` and `Magic` accounts are preserved. |

For a raw eATA release with no local account under that address, ChainSync fetches
its base-chain image at or after the event slot to identify matching projected
ATAs and applies the same cleanup rules. A missing image cannot identify those
projections. Transaction-based detection currently resolves static message keys
only; accounts referenced through address lookup tables are unsupported.

## Freshness and recovery

HTTP retries use alternative providers without lowering the required snapshot slot.
Separate batches may observe different slots, and a failed `sync` can leave earlier accounts
materialized: synchronization is not an atomic snapshot or transaction.

If one ordinary update source is lost, the other can continue supplying updates.
Losing the last source removes eligible local mirrors so a later `sync` can reacquire
them; locally authoritative accounts are preserved. Engine cache eviction also
retires subscriptions before removing an eligible mirror. Explicit unsubscribe
releases coverage but leaves local-state cleanup to its caller.

Reconnection and replay do not guarantee gapless delivery. Engine's persisted
chain slot advances from confirmed gRPC Clock observations, not from completed
account application. HTTP snapshots use it as a freshness floor, not proof that
all earlier updates have been applied. Newly opened gRPC streams request replay
from two slots before that watermark, saturating at zero; ordinary filter refreshes
do not restart replay.

For implementation entry points, see [acquisition](src/acquisition.rs),
[ordinary update handling](src/worker/mod.rs), [lifecycle handling](src/worker/lifecycle.rs),
and [gRPC delegation matching](src/grpc/delegation.rs).
