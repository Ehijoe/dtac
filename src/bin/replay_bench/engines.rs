//! Cache engines `replay_bench` can replay a log against.

use dtac::cache_engine::{EvictionListener, shard_of};
use dtac::dtac_cache::DtacCache;
use lru::LruCache;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// A standard, bot-unaware sharded LRU cache: the baseline that
/// `--algorithm lru` benchmarks against DTAC's own engine. Shards the same
/// way as `DtacCacheEngine` (same shard count and key hashing, same
/// capacity-per-shard math) so the comparison isolates eviction/admission
/// policy rather than lock granularity or capacity partitioning. Bench-tool-only -- not part
/// of the published `dtac` library.
pub struct BaselineLruEngine<K, P> {
    shards: Vec<Mutex<LruCache<K, P>>>,
    eviction_listener: Option<EvictionListener<K>>,
}

impl<K: Eq + Hash + Clone, P: Clone> BaselineLruEngine<K, P> {
    /// An LRU cache of `total_capacity` entries split evenly across `shard_count` shards.
    pub fn new(total_capacity: usize, shard_count: usize) -> Self {
        let shard_count = shard_count.max(1);
        let cap = NonZeroUsize::new(total_capacity / shard_count).unwrap_or(NonZeroUsize::MIN);
        let shards = (0..shard_count)
            .map(|_| Mutex::new(LruCache::new(cap)))
            .collect();
        Self {
            shards,
            eviction_listener: None,
        }
    }

    /// Calls `listener` with every evicted key, after the shard lock is released, like
    /// `DtacCacheEngine::with_eviction_listener`.
    #[must_use]
    pub fn with_eviction_listener(mut self, listener: EvictionListener<K>) -> Self {
        self.eviction_listener = Some(listener);
        self
    }

    /// The keys currently cached, across all shards. Diagnostics only.
    pub fn resident_keys(&self) -> Vec<K> {
        (0..self.shards.len())
            .flat_map(|id| {
                self.shard(id)
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn get_shard_id(&self, key: &K) -> usize {
        shard_of(key, self.shards.len())
    }

    /// Locks one shard, recovering the guard if a previous holder panicked, the same
    /// way `DtacCacheEngine` does.
    fn shard(&self, shard_id: usize) -> MutexGuard<'_, LruCache<K, P>> {
        self.shards[shard_id]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub async fn access<F, Fut>(&self, key: K, fetch_from_origin: F) -> P
    where
        F: FnOnce(&K) -> Fut,
        Fut: Future<Output = P>,
    {
        let shard_id = self.get_shard_id(&key);

        // lru's get() already promotes to MRU
        let hit = self.shard(shard_id).get(&key).cloned();
        if let Some(payload) = hit {
            return payload;
        }

        // Release the lock before awaiting, matching DtacCacheEngine's
        // convention of never holding a shard lock across an await point.
        let payload = fetch_from_origin(&key).await;
        // `push` returns the LRU entry it evicted, or the old value if a concurrent miss
        // inserted the same key first (idempotent per key: no orphan-entry risk).
        let displaced = self.shard(shard_id).push(key.clone(), payload.clone());
        if let (Some((evicted, _)), Some(listener)) = (displaced, &self.eviction_listener)
            && evicted != key
        {
            listener(&evicted);
        }
        payload
    }
}

#[derive(Clone)]
pub enum Engine {
    Dtac(Arc<DtacCache<String, bytes::Bytes>>),
    Lru(Arc<BaselineLruEngine<String, bytes::Bytes>>),
}

impl Engine {
    /// The keys currently in the main cache. Locks every shard in turn.
    pub fn resident_keys(&self) -> Vec<String> {
        match self {
            Self::Dtac(cache) => cache.engine().resident_keys(),
            Self::Lru(lru) => lru.resident_keys(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dtac::cache_engine::NUM_SHARDS;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn baseline_lru_hit_after_miss_does_not_refetch() {
        let engine = BaselineLruEngine::<String, i32>::new(1, 1);
        let fetches = AtomicU64::new(0);

        let miss = engine
            .access("only-key".to_string(), |_k| {
                fetches.fetch_add(1, Ordering::Relaxed);
                async { 42 }
            })
            .await;
        assert_eq!(miss, 42);
        assert_eq!(fetches.load(Ordering::Relaxed), 1);

        let hit = engine
            .access("only-key".to_string(), |_k| {
                fetches.fetch_add(1, Ordering::Relaxed);
                async { -1 } // would prove this ran if it did
            })
            .await;
        assert_eq!(hit, 42);
        assert_eq!(fetches.load(Ordering::Relaxed), 1); // fetch not called again
    }

    /// Three keys that hash to the same LRU shard.
    fn three_colliding_keys() -> (String, String, String) {
        let mut by_shard: HashMap<usize, Vec<String>> = HashMap::new();
        for i in 0..5000 {
            let key = format!("k{i}");
            let shard = shard_of(&key, NUM_SHARDS);
            by_shard.entry(shard).or_default().push(key);
        }
        by_shard
            .values()
            .find(|keys| keys.len() >= 3)
            .map(|keys| (keys[0].clone(), keys[1].clone(), keys[2].clone()))
            .expect("5000 candidate keys across 256 shards must collide 3-deep somewhere")
    }

    #[tokio::test]
    async fn baseline_lru_reports_evictions_by_admitter() {
        use crate::pollution::{Ledger, Origin, PollutionReport};

        let ledger = Arc::new(Ledger::default());
        ledger.start_measuring();
        let listener_ledger = ledger.clone();
        let engine = BaselineLruEngine::<String, i32>::new(2 * NUM_SHARDS, NUM_SHARDS)
            .with_eviction_listener(Arc::new(move |key: &String| listener_ledger.on_evict(key)));
        let (human_key, bot_key, later_key) = three_colliding_keys();

        // Fetch as the bench does: the admitter is the label of the request that fetched.
        let access = |key: &String, origin: Origin| {
            let (ledger, engine, key) = (ledger.clone(), &engine, key.clone());
            async move {
                let fetched = key.clone();
                engine
                    .access(key, move |_k| {
                        ledger.on_fetch(&fetched, origin, 1);
                        async { 0 }
                    })
                    .await
            }
        };
        access(&bot_key, Origin::Bot).await; // shard (MRU->LRU): [bot]
        access(&human_key, Origin::Human).await; // shard: [human, bot]
        access(&later_key, Origin::Human).await; // evicts bot -> [later, human]
        access(&bot_key, Origin::Bot).await; // evicts human -> [bot, later]

        // Re-requesting a resident key is a hit: no fetch, no eviction.
        access(&later_key, Origin::Bot).await;

        ledger.sample(&engine.resident_keys());
        let report = PollutionReport::new(&ledger).unwrap();
        assert_eq!(report.evictions_bot_admitted, 1);
        assert_eq!(report.evictions_human_admitted, 1);
        // Resident: `bot` (no human since admission) and `later` (human-admitted).
        assert!((report.cpi_final - 0.5).abs() < 1e-12);
    }

    #[tokio::test]
    async fn baseline_lru_evicts_least_recently_used_within_a_shard() {
        // DefaultHasher is fixed-seed, so shard assignment is deterministic;
        // brute-force three candidate keys that collide into the same shard
        // so recency-ordered (not just capacity-1-swap) eviction is
        // observable, without depending on shard-count internals beyond
        // what BaselineLruEngine already exposes.
        let (key_a, key_b, key_c) = three_colliding_keys();

        // total_capacity = 2 * NUM_SHARDS -> capacity_per_shard == 2.
        let engine = BaselineLruEngine::<String, i32>::new(2 * NUM_SHARDS, NUM_SHARDS);

        let _ = engine.access(key_a.clone(), |_k| async { 1 }).await; // shard (MRU->LRU): [A]
        let _ = engine.access(key_b.clone(), |_k| async { 2 }).await; // shard: [B, A]
        let _ = engine.access(key_a.clone(), |_k| async { 1 }).await; // hit: promotes A -> shard: [A, B]
        let _ = engine.access(key_c.clone(), |_k| async { 3 }).await; // miss, over capacity: evicts LRU (B) -> shard: [C, A]

        // Check key_a's survival BEFORE checking key_b: verifying a miss is
        // itself a cache-mutating access (it re-inserts the key), so
        // checking key_b first would evict key_a all over again and
        // invalidate this assertion order.
        let refetch_a = AtomicU64::new(0);
        let a_again = engine
            .access(key_a.clone(), |_k| {
                refetch_a.fetch_add(1, Ordering::Relaxed);
                async { 99 }
            })
            .await;
        assert_eq!(a_again, 1); // original value, not 99 -- key_a survived as a hit
        assert_eq!(refetch_a.load(Ordering::Relaxed), 0);

        let refetch_b = AtomicU64::new(0);
        let b_again = engine
            .access(key_b.clone(), |_k| {
                refetch_b.fetch_add(1, Ordering::Relaxed);
                async { -1 }
            })
            .await;
        assert_eq!(b_again, -1);
        assert_eq!(refetch_b.load(Ordering::Relaxed), 1); // confirms key_b was evicted
    }
}
