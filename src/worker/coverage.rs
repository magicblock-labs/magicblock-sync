use std::collections::hash_map::Entry::Occupied;

use ahash::AHashMap;
use solana_pubkey::Pubkey;

use crate::{metrics, AccountSubscription};

/// Transport identity shared by coverage transitions and loss metrics.
pub(super) use crate::metrics::Transport as Source;

/// Confirmed coverage for one logical subscription.
struct Entry {
    /// Exact subscribed key and optional ProgramData Engine target.
    sub: AccountSubscription,
    /// Distinguishes a current subscription from late transport confirmations.
    generation: u64,
    /// Whether the WS pool has acknowledged this account.
    ws: bool,
    /// The one confirmed gRPC stream, if any.
    grpc: Option<usize>,
}

impl Entry {
    /// Gauge label for the current source combination, or `None` after final-source loss.
    fn coverage(&self) -> Option<&'static str> {
        match (self.ws, self.grpc.is_some()) {
            (true, false) => Some("ws_only"),
            (false, true) => Some("grpc_only"),
            (true, true) => Some("both"),
            (false, false) => None,
        }
    }
}

/// Account coverage mutated only by the ChainSync worker; transports report changes as events.
#[derive(Default)]
pub(super) struct Coverage {
    /// Keys with at least one confirmed source.
    entries: AHashMap<Pubkey, Entry>,
    /// Monotonic identity for newly acknowledged logical subscriptions.
    generation: u64,
}

impl Coverage {
    /// Records a WS acknowledgement and returns its current generation for gRPC reuse.
    pub(super) fn acknowledged(&mut self, sub: AccountSubscription) -> u64 {
        if let Some(entry) = self.entries.get_mut(&sub.pubkey) {
            let before = entry.coverage();
            entry.ws = true;
            metrics::coverage(before, entry.coverage());
            return entry.generation;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let entry = Entry {
            sub,
            generation,
            ws: true,
            grpc: None,
        };
        metrics::coverage(None, entry.coverage());
        self.entries.insert(sub.pubkey, entry);
        generation
    }

    /// Removes the logical subscription, including any in-flight transport request.
    pub(super) fn removed(&mut self, pubkey: Pubkey) -> Option<Pubkey> {
        let entry = self.entries.remove(&pubkey)?;
        metrics::coverage(entry.coverage(), None);
        self.eviction_target(entry.sub)
    }

    /// Drops one source; returns whether coverage ended and any Engine target to evict.
    pub(super) fn lost(&mut self, source: Source, pubkey: Pubkey) -> (bool, Option<Pubkey>) {
        let Some(entry) = self.entries.get_mut(&pubkey) else { return (false, None) };
        let before = entry.coverage();
        match source {
            Source::WebSocket => entry.ws = false,
            Source::Grpc => entry.grpc = None,
        }
        metrics::coverage(before, entry.coverage());
        // Losing WebSocket alone does not evict an account with a confirmed gRPC copy.
        if entry.ws || entry.grpc.is_some() {
            return (false, None);
        }
        metrics::lost(source);
        (true, self.removed(pubkey))
    }

    /// Accepts a sent filter only for the current logical subscription.
    pub(super) fn confirmed(&mut self, stream: usize, pubkey: Pubkey, generation: u64) {
        let Some(entry) = self.entries.get_mut(&pubkey) else { return };
        if entry.generation == generation {
            let before = entry.coverage();
            entry.grpc = Some(stream);
            metrics::coverage(before, entry.coverage());
        }
    }

    /// Filters buffered WS updates after logical removal or source loss.
    pub(super) fn ws_contains(&self, sub: AccountSubscription) -> bool {
        self.entries.get(&sub.pubkey).is_some_and(|entry| entry.sub == sub && entry.ws)
    }

    /// Filters buffered gRPC updates after logical removal or source loss.
    pub(super) fn grpc_contains(&self, stream: usize, sub: AccountSubscription) -> bool {
        self.entries
            .get(&sub.pubkey)
            .is_some_and(|entry| entry.sub == sub && entry.grpc == Some(stream))
    }

    /// Lists keys covered by a failed gRPC stream before removing that source.
    pub(super) fn stream_keys(&self, stream: usize) -> Vec<Pubkey> {
        self.entries
            .iter()
            .filter_map(|(&pubkey, entry)| (entry.grpc == Some(stream)).then_some(pubkey))
            .collect()
    }

    /// Ends the target's coverage without requesting another Engine eviction.
    pub(super) fn evicted(&mut self, target: Pubkey) -> impl Iterator<Item = Pubkey> + '_ {
        AccountSubscription::for_target(target).into_iter().filter_map(|sub| {
            let Occupied(entry) = self.entries.entry(sub.pubkey) else { return None };
            if entry.get().sub != sub {
                return None;
            }
            let entry = entry.remove();
            metrics::coverage(entry.coverage(), None);
            Some(entry.sub.pubkey)
        })
    }

    /// An Engine target survives while any other logical subscription names it.
    fn eviction_target(&self, sub: AccountSubscription) -> Option<Pubkey> {
        let target = sub.target();
        (!self.entries.values().any(|entry| entry.sub.target() == target)).then_some(target)
    }
}

impl Drop for Coverage {
    /// Removes this worker's surviving subscriptions from process-wide coverage gauges.
    fn drop(&mut self) {
        for entry in self.entries.values() {
            metrics::coverage(entry.coverage(), None);
        }
    }
}
