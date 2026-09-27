use ahash::AHashMap;
use solana_pubkey::Pubkey;

use crate::AccountSubscription;

/// Identity of a transport that can deliver an account update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Source {
    /// The WebSocket pool's single subscription for an account.
    WebSocket,
    /// The gRPC stream selected for this key.
    Grpc,
}

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

/// Single-owner account coverage; transport workers report outcomes but never mutate it.
#[derive(Default)]
pub(super) struct Coverage {
    /// Keys with at least one confirmed source.
    entries: AHashMap<Pubkey, Entry>,
    /// Monotonic identity for newly acknowledged logical subscriptions.
    generation: u64,
}

impl Coverage {
    /// Records a WS acknowledgement and returns its current generation for gRPC reuse.
    pub(super) fn acknowledged(
        &mut self,
        sub: AccountSubscription,
        background: bool,
    ) -> Option<u64> {
        if let Some(entry) = self.entries.get_mut(&sub.pubkey) {
            entry.ws = true;
            return Some(entry.generation);
        }
        if background {
            return None;
        }
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        self.entries.insert(
            sub.pubkey,
            Entry {
                sub,
                generation,
                ws: true,
                grpc: None,
            },
        );
        Some(generation)
    }

    /// Removes the logical subscription, including any in-flight transport request.
    pub(super) fn removed(&mut self, pubkey: Pubkey) -> Option<Pubkey> {
        let entry = self.entries.remove(&pubkey)?;
        self.eviction_target(entry.sub)
    }

    /// Drops one source; returns whether coverage ended and any Engine target to evict.
    pub(super) fn lost(&mut self, source: Source, pubkey: Pubkey) -> (bool, Option<Pubkey>) {
        let Some(entry) = self.entries.get_mut(&pubkey) else { return (false, None) };
        match source {
            Source::WebSocket => {
                entry.ws = false;
            }
            Source::Grpc => entry.grpc = None,
        }
        if entry.ws || entry.grpc.is_some() {
            return (false, None);
        }
        (true, self.removed(pubkey))
    }

    /// Accepts a sent filter only for the current logical subscription.
    pub(super) fn confirmed(&mut self, stream: usize, pubkey: Pubkey, generation: u64) {
        if let Some(entry) =
            self.entries.get_mut(&pubkey).filter(|entry| entry.generation == generation)
        {
            entry.grpc = Some(stream);
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

    /// Returns gRPC-only subscriptions lacking a WebSocket copy.
    pub(super) fn missing_ws(&self) -> impl Iterator<Item = AccountSubscription> + '_ {
        self.entries
            .values()
            .filter(|entry| entry.grpc.is_some() && !entry.ws)
            .map(|entry| entry.sub)
    }

    /// Lists keys covered by a failed gRPC stream before removing that source.
    pub(super) fn stream_keys(&self, stream: usize) -> Vec<Pubkey> {
        self.entries
            .iter()
            .filter_map(|(&pubkey, entry)| (entry.grpc == Some(stream)).then_some(pubkey))
            .collect()
    }

    /// An Engine target survives while any other logical subscription names it.
    fn eviction_target(&self, sub: AccountSubscription) -> Option<Pubkey> {
        let target = sub.target.unwrap_or(sub.pubkey);
        (!self
            .entries
            .values()
            .any(|entry| entry.sub.target.unwrap_or(entry.sub.pubkey) == target))
        .then_some(target)
    }
}
