/// Confirmed coverage and redundancy scheduling for the event worker.
mod coverage;
/// Delegation activation, rescue, and undelegation lifecycle.
mod lifecycle;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

use nucleus::shutdown::{ShutdownHandle, ShutdownReason};
use solana_account::{AccountBuilder, StateFlags};
use solana_pubkey::Pubkey;
use tokio::{
    sync::mpsc::{Receiver, UnboundedReceiver},
    time,
};
use tracing::{error, warn};

use self::coverage::{Coverage, Source};
use crate::{
    ata,
    grpc::{self, Command},
    program, websocket, AccountSubscription, ChainSync, Result, DUPLICATION_DELAY,
};

impl ChainSync {
    /// Stable per-key assignment keeps a recently removed remote filter reusable.
    fn grpc_client(&self, pubkey: Pubkey) -> &grpc::Client {
        let mut hash = DefaultHasher::new();
        pubkey.hash(&mut hash);
        &self.grpc[hash.finish() as usize % self.grpc.len()]
    }

    /// Serializes source events, account application, and Engine eviction.
    pub(super) async fn run(
        self: Arc<Self>,
        websocket: Receiver<websocket::Event>,
        grpc: Receiver<grpc::Event>,
        evictions: UnboundedReceiver<Pubkey>,
        mut shutdown: ShutdownHandle,
    ) {
        let reason = tokio::select! {
            biased;
            _ = shutdown.signalled() => ShutdownReason::Signalled,
            result = self.run_updates(websocket, grpc, evictions) => match result {
                Ok(()) => ShutdownReason::Unexpected,
                Err(error) => ShutdownReason::Error(Box::new(error)),
            },
        };
        drop(self);
        shutdown.terminate(reason);
    }

    async fn run_updates(
        &self,
        mut websocket: Receiver<websocket::Event>,
        mut grpc: Receiver<grpc::Event>,
        mut evictions: UnboundedReceiver<Pubkey>,
    ) -> Result<()> {
        let mut coverage = Coverage::default();
        let mut tick = time::interval(DUPLICATION_DELAY);
        tick.tick().await;
        // Coverage changes and account application share this loop so buffered updates
        // cannot overtake a source removal or revive an evicted target.
        loop {
            tokio::select! {
                biased;
                Some(event) = websocket.recv() => {
                    self.on_websocket(event, &mut coverage).await?;
                }
                Some(event) = grpc.recv() => {
                    self.on_grpc(event, &mut coverage).await?;
                }
                pubkey = evictions.recv() => {
                    let Some(pubkey) = pubkey else { return Ok(()) };
                    self.evict_cached(pubkey, &mut coverage).await?;
                }
                _ = tick.tick() => {
                    for client in &self.grpc {
                        client.command(Command::Rebuild);
                    }
                }
            }
        }
    }

    /// Claims only an eviction that still applies, excluding concurrent acquisition.
    async fn evict_cached(&self, pubkey: Pubkey, coverage: &mut Coverage) -> Result<()> {
        let Some(accessor) = self.engine.account(pubkey).await?.into_cached_eviction() else {
            return Ok(());
        };
        for key in coverage.evicted(pubkey) {
            self.grpc_client(key).command(Command::Remove(key));
        }
        // Queue cleanup under the lease so reacquisition cannot subscribe ahead of it.
        self.websocket.evict(pubkey).await?;
        accessor.delete().await?;
        Ok(())
    }

    /// Applies WebSocket coverage changes and filters buffered account updates.
    async fn on_websocket(&self, event: websocket::Event, coverage: &mut Coverage) -> Result<()> {
        match event {
            websocket::Event::Acknowledged(sub) => {
                let gen = coverage.acknowledged(sub);
                self.grpc_client(sub.pubkey).command(Command::Track { sub, gen });
            }
            websocket::Event::Removed(pubkey) => {
                let target = coverage.removed(pubkey);
                self.grpc_client(pubkey).command(Command::Remove(pubkey));
                if let Some(target) = target {
                    self.evict(target).await?;
                }
            }
            websocket::Event::Update { sub, account } => {
                if coverage.ws_contains(sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        error!(source = "WS", %sub.pubkey, %error, "account update failed");
                    }
                }
            }
            websocket::Event::Dropped { pubkeys, error } => {
                warn!(lost = pubkeys.len(), %error, "WebSocket subscriptions lost");
                for pubkey in pubkeys {
                    self.lost(coverage, Source::WebSocket, pubkey).await?;
                }
            }
        }
        Ok(())
    }

    /// Drops a source and ends gRPC interest if no confirmed copy survives.
    async fn lost(&self, coverage: &mut Coverage, source: Source, pubkey: Pubkey) -> Result<()> {
        let (ended, target) = coverage.lost(source, pubkey);
        if ended {
            self.grpc_client(pubkey).command(Command::Remove(pubkey));
        }
        if let Some(target) = target {
            self.evict(target).await?;
        }
        Ok(())
    }

    /// Removes a target only when Engine still considers it non-authoritative.
    async fn evict(&self, target: Pubkey) -> Result<()> {
        if let Some(accessor) = self.engine.account(target).await?.into_eviction() {
            accessor.delete().await?;
        }
        Ok(())
    }

    /// Applies per-stream coverage, account, and delegation lifecycle events.
    async fn on_grpc(&self, event: grpc::Event, coverage: &mut Coverage) -> Result<()> {
        match event {
            grpc::Event::Confirmed { stream, pubkey, gen } => {
                coverage.confirmed(stream, pubkey, gen);
            }
            grpc::Event::Lost(stream) => {
                for pubkey in coverage.stream_keys(stream) {
                    self.lost(coverage, Source::Grpc, pubkey).await?;
                }
            }
            grpc::Event::Update { stream, sub, account } => {
                if coverage.grpc_contains(stream, sub) {
                    if let Err(error) = self.apply(sub, account).await {
                        error!(source = "gRPC", stream, %sub.pubkey, %error, "account update failed");
                    }
                }
            }
            grpc::Event::Delegated(delegation) => {
                let pubkey = delegation.pubkey;
                if let Err(error) = self.delegated(delegation).await {
                    error!(%pubkey, %error, "delegation failed");
                }
            }
            grpc::Event::Undelegated { pubkeys, slot } => {
                for pubkey in pubkeys {
                    if let Err(error) = self.undelegated(pubkey, slot).await {
                        error!(%pubkey, slot, %error, "undelegation failed");
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies a streamed account update to its subscribed account or program target.
    async fn apply(
        &self,
        subscription: AccountSubscription,
        account: AccountBuilder,
    ) -> Result<()> {
        let AccountSubscription { pubkey, target } = subscription;
        if target.is_none() && ata::is_raw_eata(pubkey, &account) {
            self.unsubscribe([pubkey]).await;
            return Ok(());
        }
        let accessor = self.engine.account(subscription.target()).await?;
        let account = match target {
            Some(_) => program::normalize_program_data(account, self.engine.rent())?,
            None if account.read().flags().contains(StateFlags::EXECUTABLE) => {
                program::normalize(account, None, self.engine.rent())?
            }
            None => account,
        };
        accessor.materialize(account, None).await?;
        Ok(())
    }
}
