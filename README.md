# magicblock-sync

Real-time synchronization of delegation records states via Laserstream. 

```rust
let channels = DlpSyncer::start(endpoint, api_key).await?;
let (requester, mut updates) = channels.split();

requester.subscribe(record_pubkey).await;

while let Some(update) = updates.recv().await {
    match update {
        AccountUpdate::Delegated { record, data, slot } => { /* ... */ }
        AccountUpdate::Undelegated { record, slot } => { /* ... */ }
        AccountUpdate::SyncInterrupted => { /* revalidate cached delegation state */ }
        AccountUpdate::SyncTerminated => break,
    }
}
```

Records are matched by their account discriminator rather than datasize, so
records carrying appended post-delegation actions are observed too. Updates
arrive at confirmed commitment. When the stream drops, the syncer resumes
from the last observed slot; if the server cannot replay that far back it
reconnects fresh and emits `SyncInterrupted`.

The syncer is a fast path over the live record stream, not a store of record
state: absence of an update for a record is never evidence the record does
not exist or was undelegated — fall back to fetching in that case.

See `src/lib.rs` for API docs.
