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
        AccountUpdate::SyncTerminated => break,
    }
}
```

See `src/lib.rs` for API docs.
