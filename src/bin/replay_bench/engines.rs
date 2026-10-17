//! Cache engines `replay_bench` can replay a log against.

use bytes::Bytes;
use dtac::cache_engine::config::EngineConfig;
use dtac::cache_engine::{DtacCacheEngine, EvictionListener, shard_of};
use dtac::dtac_cache::DtacCache;
use dtac::heuristic_router::config::RouterConfig;
use dtac::types::RequestData;
use lru::LruCache;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// What one access did besides returning the payload. Hits and misses are counted
/// through the fetch closure, which only runs on a miss.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Outcome {
    /// The router's bot probability, for the classifier report. `None` for engines
    /// that don't score requests.
    pub bot_prob: Option<f32>,
    /// `(T_proxy, T_cache)` for engines that time their own phases. For the others the
    /// bench counts the whole access as `T_cache`.
    pub phases: Option<(Duration, Duration)>,
}

/// Sizes of an engine's structures at the end of a replay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Structure {
    pub shards: usize,
    pub main_entries: usize,
    /// Metadata-only entries (DTAC's ghost cache, S3FIFO's ghost queue).
    pub ghost_entries: Option<usize>,
    pub ghost_capacity_per_shard: Option<usize>,
    /// IPs the router's `SpaceSaver` tracks, for engines with a router.
    pub tracked_ips: Option<usize>,
}

/// A cache `replay_bench` can drive. Every engine shards keys with
/// [`shard_of`] and splits its capacity evenly across the shards, so engines differ
/// only in policy.
pub trait BenchEngine: Send + Sync + 'static {
    /// The `--algorithm` value, as written to `--json-out`.
    fn name(&self) -> &'static str;

    /// Looks up `key`, calling `fetch` on a miss. `request` builds the request the
    /// router scores; engines without a router never call it, so they don't pay for it.
    fn access<R, F, Fut>(
        &self,
        key: String,
        request: R,
        fetch: F,
    ) -> impl Future<Output = Outcome> + Send
    where
        R: FnOnce() -> RequestData + Send,
        F: FnOnce(&String) -> Fut + Send,
        Fut: Future<Output = Bytes> + Send;

    /// The keys currently cached. Locks every shard in turn; diagnostics only.
    fn resident_keys(&self) -> Vec<String>;

    /// Calls `listener` with every key evicted from the cache, outside any shard lock.
    fn set_eviction_listener(&mut self, listener: EvictionListener<String>);

    fn structure(&self) -> Structure;

    /// The effective configs, for `--json-out`. `None` where the engine has no such part.
    fn configs(&self) -> (Option<RouterConfig>, Option<EngineConfig>) {
        (None, None)
    }
}

/// DTAC's full pipeline: the router scores each request and the engine uses the score.
pub struct DtacEngine(pub DtacCache<String, Bytes>);

impl BenchEngine for DtacEngine {
    fn name(&self) -> &'static str {
        "dtac"
    }

    async fn access<R, F, Fut>(&self, key: String, request: R, fetch: F) -> Outcome
    where
        R: FnOnce() -> RequestData + Send,
        F: FnOnce(&String) -> Fut + Send,
        Fut: Future<Output = Bytes> + Send,
    {
        let (_, stats) = self.0.access_with_stats(key, request(), fetch).await;
        Outcome {
            bot_prob: Some(stats.bot_prob),
            phases: Some((stats.router_time, stats.engine_time)),
        }
    }

    fn resident_keys(&self) -> Vec<String> {
        self.0.engine().resident_keys()
    }

    fn set_eviction_listener(&mut self, listener: EvictionListener<String>) {
        self.0.set_eviction_listener(listener);
    }

    fn structure(&self) -> Structure {
        let engine = self.0.engine();
        Structure {
            shards: engine.shard_count(),
            main_entries: engine.len(),
            ghost_entries: Some(engine.ghost_len()),
            ghost_capacity_per_shard: Some(engine.ghost_capacity_per_shard()),
            tracked_ips: Some(self.0.router().tracked_ips()),
        }
    }

    fn configs(&self) -> (Option<RouterConfig>, Option<EngineConfig>) {
        (
            Some(*self.0.router().config()),
            Some(*self.0.engine().config()),
        )
    }
}

/// The ablation: DTAC's engine (ghost admission and the decayed-score tournament) with
/// every request given the same constant bot probability instead of a router score.
///
/// The constant is what a clean human request scores under the router config,
/// `sigmoid(l_0)`, not 0. With 0 every request would add a full point and items would
/// promote faster than in `dtac`, mixing a change in promotion speed into the comparison.
/// With the human baseline, human traffic behaves exactly as in `dtac`, and the only
/// difference is that bots are no longer told apart.
pub struct NoScoreEngine {
    engine: DtacCacheEngine<String, Bytes>,
    bot_prob: f32,
    router_config: RouterConfig,
}

impl NoScoreEngine {
    /// Wraps `engine`, scoring every request as a clean request under `router_config`.
    pub fn new(engine: DtacCacheEngine<String, Bytes>, router_config: RouterConfig) -> Self {
        // A probability in [0, 1] loses nothing that matters when narrowed to f32, and
        // DtacCache narrows the router's score the same way.
        #[allow(clippy::cast_possible_truncation)]
        let bot_prob = router_config.clean_request_bot_prob() as f32;
        Self {
            engine,
            bot_prob,
            router_config,
        }
    }
}

impl BenchEngine for NoScoreEngine {
    fn name(&self) -> &'static str {
        "dtac-no-score"
    }

    async fn access<R, F, Fut>(&self, key: String, _request: R, fetch: F) -> Outcome
    where
        R: FnOnce() -> RequestData + Send,
        F: FnOnce(&String) -> Fut + Send,
        Fut: Future<Output = Bytes> + Send,
    {
        self.engine.access(key, self.bot_prob, fetch).await;
        Outcome::default()
    }

    fn resident_keys(&self) -> Vec<String> {
        self.engine.resident_keys()
    }

    fn set_eviction_listener(&mut self, listener: EvictionListener<String>) {
        self.engine.set_eviction_listener(listener);
    }

    fn structure(&self) -> Structure {
        Structure {
            shards: self.engine.shard_count(),
            main_entries: self.engine.len(),
            ghost_entries: Some(self.engine.ghost_len()),
            ghost_capacity_per_shard: Some(self.engine.ghost_capacity_per_shard()),
            tracked_ips: None,
        }
    }

    /// The router config isn't used to score anything, but it fixes the constant.
    fn configs(&self) -> (Option<RouterConfig>, Option<EngineConfig>) {
        (Some(self.router_config), Some(*self.engine.config()))
    }
}

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
    pub fn set_eviction_listener(&mut self, listener: EvictionListener<K>) {
        self.eviction_listener = Some(listener);
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

impl BenchEngine for BaselineLruEngine<String, Bytes> {
    fn name(&self) -> &'static str {
        "lru"
    }

    async fn access<R, F, Fut>(&self, key: String, _request: R, fetch: F) -> Outcome
    where
        R: FnOnce() -> RequestData + Send,
        F: FnOnce(&String) -> Fut + Send,
        Fut: Future<Output = Bytes> + Send,
    {
        Self::access(self, key, fetch).await;
        Outcome::default()
    }

    fn resident_keys(&self) -> Vec<String> {
        Self::resident_keys(self)
    }

    fn set_eviction_listener(&mut self, listener: EvictionListener<String>) {
        Self::set_eviction_listener(self, listener);
    }

    fn structure(&self) -> Structure {
        Structure {
            shards: self.shards.len(),
            main_entries: (0..self.shards.len()).map(|id| self.shard(id).len()).sum(),
            ..Structure::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dtac::cache_engine::NUM_SHARDS;
    use std::collections::HashMap;
    use std::sync::Arc;
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

    /// Builds `dtac` and `dtac-no-score` with default configs on one shared clock.
    fn dtac_and_ablation() -> (DtacEngine, NoScoreEngine, Arc<dtac::clock::ManualClock>) {
        use dtac::clock::ManualClock;
        use dtac::heuristic_router::HeuristicRouter;

        let clock = Arc::new(ManualClock::new(0));
        let dtac = DtacEngine(
            DtacCache::builder(4_096)
                .router(HeuristicRouter::new())
                .clock(clock.clone())
                .build(),
        );
        let engine =
            DtacCacheEngine::with_config(4_096, EngineConfig::default(), clock.clone()).unwrap();
        let ablation = NoScoreEngine::new(engine, RouterConfig::default());
        (dtac, ablation, clock)
    }

    /// A clean human page view: browser headers, and an IP of its own so velocity and
    /// "Shy Guys" never fire.
    fn human_request(visitor: u32, path: &str) -> RequestData {
        RequestData {
            ip_address: std::net::IpAddr::V4(std::net::Ipv4Addr::from(0x1800_0000 + visitor)),
            path: path.to_string(),
            method: "GET".to_string(),
            headers: HashMap::from([("accept-language".to_string(), "en-US".to_string())]),
        }
    }

    #[tokio::test]
    async fn ablation_promotes_the_same_keys_as_dtac_on_human_traffic() {
        let (dtac, ablation, clock) = dtac_and_ablation();
        let mut visitor = 0;
        // 200 keys, each requested 1-4 times in quick succession: only some reach the
        // promotion threshold, and the cache never fills, so no random eviction happens.
        for i in 0..200_u32 {
            let path = format!("/item/{i}");
            for _ in 0..=(i % 4) {
                visitor += 1;
                clock.advance(Duration::from_millis(100));
                let request = human_request(visitor, &path);
                dtac.access(
                    path.clone(),
                    || request.clone(),
                    |_k| async { Bytes::new() },
                )
                .await;
                ablation
                    .access(path.clone(), || request, |_k| async { Bytes::new() })
                    .await;
            }
        }

        let sorted = |mut keys: Vec<String>| {
            keys.sort();
            keys
        };
        let (dtac_keys, ablation_keys) = (
            sorted(dtac.resident_keys()),
            sorted(ablation.resident_keys()),
        );
        assert!(!dtac_keys.is_empty(), "the prefix should promote something");
        assert!(
            dtac_keys.len() < 200,
            "and leave some keys in the ghost cache"
        );
        assert_eq!(dtac_keys, ablation_keys);
    }

    #[tokio::test]
    async fn ablation_scores_bots_like_humans() {
        let (_, ablation, _) = dtac_and_ablation();
        let fetches = AtomicU64::new(0);
        let fetch = |_k: &String| {
            fetches.fetch_add(1, Ordering::Relaxed);
            async { Bytes::new() }
        };
        let bot = || RequestData {
            headers: HashMap::new(),
            ..human_request(0, "/item/1")
        };
        // With the constant 0.1, three requests promote (3 x 0.9 >= 2) whoever sends them.
        for _ in 0..3 {
            ablation.access("/item/1".to_string(), bot, fetch).await;
        }
        assert_eq!(ablation.resident_keys(), vec!["/item/1".to_string()]);
        let outcome = ablation.access("/item/1".to_string(), bot, fetch).await;
        assert_eq!(
            outcome,
            Outcome::default(),
            "no router, so no score or phases"
        );
        assert_eq!(fetches.load(Ordering::Relaxed), 3);
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
        let mut engine = BaselineLruEngine::<String, i32>::new(2 * NUM_SHARDS, NUM_SHARDS);
        engine.set_eviction_listener(Arc::new(move |key: &String| listener_ledger.on_evict(key)));
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
