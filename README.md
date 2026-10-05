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

## Account behavior

`sync` acquires only accounts missing from Engine. It does not refresh resident
accounts or add subscriptions for them. Choose each account's role according to
its use in the transaction:

| Property | Behavior for a missing account |
| --- | --- |
| `Payer` | Acquires the account and checks for delegation; keeps live updates unless delegated to this Engine. |
| `Readonly` | Acquires the account with live updates; discovers program or delegation state when needed. |
| `Writable` | Acquires the account and checks for delegation, without subscribing to ordinary base-chain updates. |
| `Program` | Acquires an executable image and tracks upgrades, including Loader V3 ProgramData. |

Delegations to this Engine restore the original owner and become locally executable.
Post-delegation actions run after their account and program dependencies have been
acquired. Authority-free delegations become confined accounts that cannot be
committed back to the base chain. For token accounts, a balance delegated through
the eATA program can be represented locally as a canonical associated token account
(ATA). The raw eATA account is not exposed for local execution.

Optional anti-money-laundering (AML) assessment checks distinct post-delegation
action signers. Rejected signers, invalid actions, and failed activation trigger
rescue undelegation;
assessment service failures propagate without triggering rescue. Scheduling rescue
does not mean on-chain undelegation has completed.

## Freshness and recovery

For tracked accounts, WebSocket subscriptions are acknowledged before the initial
HTTP snapshot. HTTP retries
use alternative providers without lowering the required snapshot slot. Separate
batches may observe different slots, and a failed `sync` can leave earlier accounts
materialized: synchronization is not an atomic snapshot or transaction.

WebSocket subscriptions become eligible for one gRPC copy after the configured
duplication delay. If one source is lost, the other can continue supplying updates.
Losing the last source removes eligible local mirrors so a later `sync` can reacquire
them; locally authoritative accounts are preserved.

Reconnection and replay do not guarantee gapless delivery. Engine's persisted
chain slot advances from confirmed gRPC Clock observations, not from completed
account application. HTTP snapshots use it as a freshness floor, not proof that
all earlier updates have been applied.
