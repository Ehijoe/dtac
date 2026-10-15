//! Cache engines `replay_bench` can replay a log against.

use dtac::cache_engine::NUM_SHARDS;
use dtac::dtac_cache::DtacCache;
use lru::LruCache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// A standard, bot-unaware sharded LRU cache: the baseline that
/// `--algorithm lru` benchmarks against DTAC's own engine. Shards the same
/// way as `DtacCacheEngine` (same `NUM_SHARDS`, same capacity-per-shard
/// math) so the comparison isolates eviction/admission policy rather than
/// lock granularity or capacity partitioning. Bench-tool-only -- not part
/// of the published `dtac` library.
pub struct BaselineLruEngine<K, P> {
    shards: Vec<Mutex<LruCache<K, P>>>,
}

impl<K: Eq + Hash + Clone, P: Clone> BaselineLruEngine<K, P> {
    pub fn new(total_capacity: usize) -> Self {
        let cap = NonZeroUsize::new(total_capacity / NUM_SHARDS).unwrap_or(NonZeroUsize::MIN);
        let shards = (0..NUM_SHARDS)
            .map(|_| Mutex::new(LruCache::new(cap)))
            .collect();
        Self { shards }
    }

    // Only the hash's low bits matter for picking one of NUM_SHARDS shards.
    #[allow(clippy::cast_possible_truncation)]
    fn get_shard_id(key: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % NUM_SHARDS
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
        let shard_id = Self::get_shard_id(&key);

        // lru's get() already promotes to MRU
        let hit = self.shard(shard_id).get(&key).cloned();
        if let Some(payload) = hit {
            return payload;
        }

        // Release the lock before awaiting, matching DtacCacheEngine's
        // convention of never holding a shard lock across an await point.
        let payload = fetch_from_origin(&key).await;
        // idempotent per key: no orphan-entry risk
        self.shard(shard_id).put(key, payload.clone());
        payload
    }
}

#[derive(Clone)]
pub enum Engine {
    Dtac(Arc<DtacCache<String, bytes::Bytes>>),
    Lru(Arc<BaselineLruEngine<String, bytes::Bytes>>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[tokio::test]
    async fn baseline_lru_hit_after_miss_does_not_refetch() {
        let engine = BaselineLruEngine::<String, i32>::new(1);
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

    #[tokio::test]
    async fn baseline_lru_evicts_least_recently_used_within_a_shard() {
        // DefaultHasher is fixed-seed, so shard assignment is deterministic;
        // brute-force three candidate keys that collide into the same shard
        // so recency-ordered (not just capacity-1-swap) eviction is
        // observable, without depending on shard-count internals beyond
        // what BaselineLruEngine already exposes.
        let mut by_shard: HashMap<usize, Vec<String>> = HashMap::new();
        for i in 0..5000 {
            let key = format!("k{i}");
            let shard = BaselineLruEngine::<String, i32>::get_shard_id(&key);
            by_shard.entry(shard).or_default().push(key);
        }
        let (key_a, key_b, key_c) = by_shard
            .values()
            .find(|keys| keys.len() >= 3)
            .map(|keys| (keys[0].clone(), keys[1].clone(), keys[2].clone()))
            .expect("5000 candidate keys across 256 shards must collide 3-deep somewhere");

        // total_capacity = 2 * NUM_SHARDS -> capacity_per_shard == 2.
        let engine = BaselineLruEngine::<String, i32>::new(2 * NUM_SHARDS);

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
