//! Module for the randomized caching algorithm considering AI scores.
//!
//! This module provides the core caching engine that manages cache entries
//! dynamically based on traffic patterns and heuristic AI scoring.

pub mod config;

use crate::clock::{Clock, SystemClock};
use crate::error::Result;
use crate::sync::lock;
use config::EngineConfig;
use rand::Rng;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};

/// The number of independent mutex locks used for lock sharding.
pub const NUM_SHARDS: usize = 256;

/// Default number of random items selected during an eviction tournament
/// ([`EngineConfig::n_samples`]).
pub const N_SAMPLES: usize = 5;

/// Default score threshold required to promote an item from Ghost Cache to Main Cache
/// ([`EngineConfig::promotion_threshold`]).
pub const PROMOTION_THRESHOLD: f32 = 2.0;

/// Default ghost-cache capacity per shard, as a multiple of the main-cache capacity per
/// shard ([`EngineConfig::ghost_multiplier`]).
pub const GHOST_CAPACITY_MULTIPLIER: usize = 4;

/// A callback told about every key evicted from the main cache, e.g. to log evictions by
/// origin. See [`DtacCacheEngine::with_eviction_listener`].
pub type EvictionListener<K> = Arc<dyn Fn(&K) + Send + Sync>;

/// Wraps an [`EvictionListener`] so the engine can still derive `Debug`.
struct Listener<K>(EvictionListener<K>);

impl<K> fmt::Debug for Listener<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EvictionListener")
    }
}

/// Metadata for a cached resource, tracking access time and EWMA score.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    /// The last time this resource was accessed, in milliseconds on the engine's clock.
    pub last_accessed: u64,
    /// The EWMA popularity score as of `last_accessed`.
    pub score: f32,
}

impl Metadata {
    /// The score decayed to `now_ms`: halved once per whole `decay_half_life_ms` elapsed
    /// since `last_accessed`, and 0 after `decay_cutoff_half_lives`.
    pub fn decayed_score(&self, now_ms: u64, config: &EngineConfig) -> f32 {
        let half_lives = now_ms.saturating_sub(self.last_accessed) / config.decay_half_life_ms;
        if half_lives >= u64::from(config.decay_cutoff_half_lives) {
            return 0.0;
        }
        // Multiplying by a power of two is exact, like the original `score / (1 << dt)`.
        i32::try_from(half_lives).map_or(0.0, |n| self.score * 0.5_f32.powi(n))
    }
}

/// Anything stored in a tournament-evicted dense vector that carries [`Metadata`].
trait HasMeta {
    fn meta(&self) -> &Metadata;
}

impl HasMeta for Metadata {
    fn meta(&self) -> &Metadata {
        self
    }
}

impl<P> HasMeta for MainCacheEntry<P> {
    fn meta(&self) -> &Metadata {
        &self.meta
    }
}

/// Runs a randomized eviction tournament (Algorithm 2) over `entries` and returns the
/// index of the victim, or `None` if `entries` is empty.
///
/// Samples `config.n_samples` random entries (with replacement) and picks the one with
/// the lowest decayed score. Ties, which are common once scores have decayed to 0, go
/// to the entry with the oldest `last_accessed`, so they fall back to LRU order instead
/// of to whichever entry happened to be sampled first.
fn tournament_victim<K, M: HasMeta>(
    entries: &[(K, M)],
    now_ms: u64,
    config: &EngineConfig,
    rng: &mut impl Rng,
) -> Option<usize> {
    if entries.is_empty() {
        return None;
    }
    let rank = |idx: usize| {
        let meta = entries[idx].1.meta();
        (meta.decayed_score(now_ms, config), meta.last_accessed)
    };
    (0..config.n_samples)
        .map(|_| rng.gen_range(0..entries.len()))
        .min_by(|&a, &b| {
            let (score_a, time_a) = rank(a);
            let (score_b, time_b) = rank(b);
            score_a.total_cmp(&score_b).then(time_a.cmp(&time_b))
        })
}

/// Removes `entries[idx]` in O(1) with `swap_remove`, keeping the key → index `map`
/// consistent: the removed key is unmapped and the entry moved into `idx` is re-pointed.
fn swap_remove_indexed<K: Eq + Hash + Clone, V>(
    map: &mut HashMap<K, usize>,
    entries: &mut Vec<(K, V)>,
    idx: usize,
) -> (K, V) {
    let removed = entries.swap_remove(idx);
    map.remove(&removed.0);
    if let Some((moved_key, _)) = entries.get(idx) {
        map.insert(moved_key.clone(), idx);
    }
    removed
}

/// An entry in the Main Cache containing both payload and metadata.
#[derive(Clone, Debug)]
pub struct MainCacheEntry<P> {
    /// The actual data payload.
    pub payload: P,
    /// The metadata associated with the payload.
    pub meta: Metadata,
}

/// A single shard of the cache, protected by its own mutex to minimize contention.
#[derive(Debug)]
pub struct CacheShard<K, P> {
    /// Maps a resource key to its index in the `entries` vector.
    main_cache_map: HashMap<K, usize>,
    /// Dense storage of main cache entries for O(1) random sampling during eviction.
    entries: Vec<(K, MainCacheEntry<P>)>,
    /// Maps a ghost-cache key to its index in the `ghost_entries` vector.
    ghost_map: HashMap<K, usize>,
    /// Dense, metadata-only ghost cache for cold-start mitigation, bounded by
    /// `ghost_capacity` and evicted with the same tournament as the main cache.
    ghost_entries: Vec<(K, Metadata)>,
    /// The maximum number of items allowed in the main cache for this shard.
    capacity: usize,
    /// The maximum number of items allowed in the ghost cache for this shard.
    ghost_capacity: usize,
}

impl<K: Eq + Hash + Clone, P: Clone> CacheShard<K, P> {
    /// Creates a new cache shard with the specified capacity and a ghost cache of
    /// [`GHOST_CAPACITY_MULTIPLIER`] times that.
    pub fn new(capacity: usize) -> Self {
        Self::with_ghost_capacity(capacity, capacity * GHOST_CAPACITY_MULTIPLIER)
    }

    /// Creates a new cache shard with explicit main- and ghost-cache capacities.
    pub fn with_ghost_capacity(capacity: usize, ghost_capacity: usize) -> Self {
        Self {
            main_cache_map: HashMap::new(),
            entries: Vec::with_capacity(capacity),
            ghost_map: HashMap::new(),
            ghost_entries: Vec::new(),
            capacity,
            ghost_capacity: ghost_capacity.max(1),
        }
    }

    /// Inserts a new ghost-cache entry, first evicting one by tournament if the ghost
    /// cache is full.
    ///
    /// Bot-inserted entries start with a score near 0, so they lose these tournaments
    /// before entries that humans have requested.
    fn insert_ghost(&mut self, key: K, meta: Metadata, now_ms: u64, config: &EngineConfig) {
        if self.ghost_entries.len() >= self.ghost_capacity
            && let Some(idx) =
                tournament_victim(&self.ghost_entries, now_ms, config, &mut rand::thread_rng())
        {
            swap_remove_indexed(&mut self.ghost_map, &mut self.ghost_entries, idx);
        }
        self.ghost_map.insert(key.clone(), self.ghost_entries.len());
        self.ghost_entries.push((key, meta));
    }

    /// The stored metadata of `key` if it is in the main or ghost cache.
    fn meta_of(&self, key: &K) -> Option<Metadata> {
        self.main_cache_map.get(key).map_or_else(
            || {
                self.ghost_map
                    .get(key)
                    .map(|&idx| self.ghost_entries[idx].1)
            },
            |&idx| Some(self.entries[idx].1.meta),
        )
    }

    /// Removes `key` from the ghost cache if present (it may already have been evicted).
    fn remove_ghost(&mut self, key: &K) {
        if let Some(&idx) = self.ghost_map.get(key) {
            swap_remove_indexed(&mut self.ghost_map, &mut self.ghost_entries, idx);
        }
    }

    /// Evicts one main-cache entry chosen by [`tournament_victim`] and returns its key.
    fn execute_eviction_tournament(&mut self, now_ms: u64, config: &EngineConfig) -> Option<K> {
        let idx = tournament_victim(&self.entries, now_ms, config, &mut rand::thread_rng())?;
        Some(swap_remove_indexed(&mut self.main_cache_map, &mut self.entries, idx).0)
    }
}

/// The Dynamic Traffic-Aware Cache (DTAC) Engine.
///
/// Implements a Randomized Eviction architecture combined with a resource-specific
/// Exponentially Weighted Moving Average (EWMA) and a metadata-only Ghost Cache.
#[derive(Debug)]
pub struct DtacCacheEngine<K, P> {
    shards: Vec<Mutex<CacheShard<K, P>>>,
    ghost_capacity_per_shard: usize,
    config: EngineConfig,
    clock: Arc<dyn Clock>,
    eviction_listener: Option<Listener<K>>,
}

impl<K: Eq + Hash + Clone, P: Clone> DtacCacheEngine<K, P> {
    /// Creates a new DTAC Engine with the specified total capacity.
    /// The capacity is divided evenly among all shards.
    ///
    /// Uses the default [`EngineConfig`] and the system clock.
    pub fn new(total_capacity: usize) -> Self {
        Self::build(
            total_capacity,
            EngineConfig::default(),
            SystemClock::shared(),
        )
    }

    /// Creates an engine with a custom [`EngineConfig`] that reads time from `clock`.
    ///
    /// Fails with [`DTACError::ConfigError`](crate::error::DTACError::ConfigError) if
    /// `config` does not pass [`EngineConfig::validate`].
    pub fn with_config(
        total_capacity: usize,
        config: EngineConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self::build(total_capacity, config, clock))
    }

    /// Shared constructor; `config` must already be valid.
    pub(crate) fn build(
        total_capacity: usize,
        config: EngineConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let capacity_per_shard = (total_capacity / NUM_SHARDS).max(1);
        let ghost_capacity_per_shard = capacity_per_shard * config.ghost_multiplier;
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(Mutex::new(CacheShard::with_ghost_capacity(
                capacity_per_shard,
                ghost_capacity_per_shard,
            )));
        }
        Self {
            shards,
            ghost_capacity_per_shard,
            config,
            clock,
            eviction_listener: None,
        }
    }

    /// Calls `listener` with the key of every entry evicted from the main cache.
    ///
    /// The listener runs after the shard lock is released, so it may call back into the
    /// engine. Ghost-cache evictions are not reported.
    #[must_use]
    pub fn with_eviction_listener(mut self, listener: EvictionListener<K>) -> Self {
        self.eviction_listener = Some(Listener(listener));
        self
    }

    /// The configuration this engine runs with.
    pub const fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The clock this engine reads time from.
    pub const fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The maximum number of ghost-cache entries each shard holds.
    pub const fn ghost_capacity_per_shard(&self) -> usize {
        self.ghost_capacity_per_shard
    }

    /// The number of resources currently in the main cache, across all shards.
    ///
    /// Locks every shard in turn, so it is meant for reporting, not for hot paths.
    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| lock(shard).entries.len())
            .sum()
    }

    /// Whether the main cache is empty. See [`len`](Self::len).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of metadata-only ghost-cache entries, across all shards.
    ///
    /// Reported next to hit rates to show the engine's metadata overhead. Locks every
    /// shard in turn, so it is meant for reporting, not for hot paths.
    pub fn ghost_len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| lock(shard).ghost_entries.len())
            .sum()
    }

    /// Hashes the requested resource to determine its specific concurrency bucket.
    // Only the hash's low bits matter for picking one of NUM_SHARDS shards.
    #[allow(clippy::cast_possible_truncation)]
    fn get_shard_id(resource: &K) -> usize {
        let mut hasher = DefaultHasher::new();
        resource.hash(&mut hasher);
        (hasher.finish() as usize) % NUM_SHARDS
    }

    /// Returns `(total_entries, total_mapped_keys)` summed across all shards.
    ///
    /// Intended for tests/diagnostics only -- it locks every shard sequentially
    /// and is not meant for use on a hot path. In a consistent cache these two
    /// numbers are always equal (every `entries` slot is reachable from exactly
    /// one `main_cache_map` entry); a mismatch indicates an orphaned/leaked
    /// capacity slot.
    pub fn debug_integrity_counts(&self) -> (usize, usize) {
        self.shards.iter().fold((0, 0), |(e, m), shard| {
            let shard = lock(shard);
            (e + shard.entries.len(), m + shard.main_cache_map.len())
        })
    }

    /// The keys currently in the main cache, across all shards.
    ///
    /// Intended for diagnostics only, like
    /// [`debug_integrity_counts`](Self::debug_integrity_counts): it locks every shard in
    /// turn, so the result is not a single consistent snapshot under concurrent access.
    pub fn resident_keys(&self) -> Vec<K> {
        self.shards
            .iter()
            .flat_map(|shard| {
                lock(shard)
                    .entries
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Returns `(ghost_entries, ghost_mapped_keys)` for each shard.
    ///
    /// Intended for tests/diagnostics only. In a consistent cache the two numbers are
    /// equal and at most [`ghost_capacity_per_shard`](Self::ghost_capacity_per_shard).
    pub fn debug_ghost_counts(&self) -> Vec<(usize, usize)> {
        self.shards
            .iter()
            .map(|shard| {
                let shard = lock(shard);
                (shard.ghost_entries.len(), shard.ghost_map.len())
            })
            .collect()
    }

    /// The current decayed score of `resource` if it is in the main or ghost cache.
    ///
    /// Intended for tests/diagnostics only.
    pub fn debug_score(&self, resource: &K) -> Option<f32> {
        let meta = lock(&self.shards[Self::get_shard_id(resource)]).meta_of(resource)?;
        Some(meta.decayed_score(self.clock.now_millis(), &self.config))
    }

    /// Accesses a resource from the cache, applying the EWMA and Ghost Cache logic.
    ///
    /// # Arguments
    /// * `requested_resource` - The key for the resource.
    /// * `request_bot_prob` - The AI-Score proxy probability of the request being a bot (0.0 to 1.0).
    /// * `fetch_from_origin` - A closure to fetch the resource payload if it's missing or promoted.
    pub async fn access<F, Fut>(
        &self,
        requested_resource: K,
        request_bot_prob: f32,
        fetch_from_origin: F,
    ) -> P
    where
        F: FnOnce(&K) -> Fut,
        Fut: Future<Output = P>,
    {
        // Phase 1: Concurrency Management (Lock Sharding)
        let shard_id = Self::get_shard_id(&requested_resource);

        let action = {
            let mut shard = lock(&self.shards[shard_id]);
            let current_time = self.clock.now_millis();

            // Phase 2: Cache Hit Evaluation & Score Update
            let slot = shard
                .main_cache_map
                .get(&requested_resource)
                .map(|&idx| Slot::Main(idx))
                .or_else(|| {
                    shard
                        .ghost_map
                        .get(&requested_resource)
                        .map(|&idx| Slot::Ghost(idx))
                });

            if let Some(slot) = slot {
                // Get metadata
                let mut meta = match slot {
                    Slot::Main(idx) => shard.entries[idx].1.meta,
                    Slot::Ghost(idx) => shard.ghost_entries[idx].1,
                };

                // Time-Decay (Bitwise Shift) and AI-Score Integration
                let decayed_score = meta.decayed_score(current_time, &self.config);

                // Increment value is inversely proportional to AI-Score (request_bot_prob)
                let new_score = decayed_score + (1.0 - request_bot_prob);

                meta.last_accessed = current_time;
                meta.score = new_score;

                match slot {
                    Slot::Main(idx) => {
                        shard.entries[idx].1.meta = meta;
                        Action::Hit(shard.entries[idx].1.payload.clone())
                    }
                    Slot::Ghost(idx) => {
                        // Store the updated meta in place; on promotion the ghost entry
                        // is only removed once the promotion commits.
                        shard.ghost_entries[idx].1 = meta;
                        // Promotion from Ghost Cache to Main Cache
                        if new_score >= self.config.promotion_threshold {
                            Action::Promote(meta)
                        } else {
                            Action::GhostHit
                        }
                    }
                }
            } else {
                // Phase 3: Cache Miss (Ghost Cache Insertion)
                let initial_score = 1.0 - request_bot_prob;
                shard.insert_ghost(
                    requested_resource.clone(),
                    Metadata {
                        last_accessed: current_time,
                        score: initial_score,
                    },
                    current_time,
                    &self.config,
                );
                Action::Miss
            }
        };

        match action {
            Action::Hit(payload) => payload,
            Action::Promote(meta) => {
                let payload = fetch_from_origin(&requested_resource).await;
                let evicted = {
                    let mut shard = lock(&self.shards[shard_id]);

                    // Another concurrent task may have already promoted this same key
                    // while our fetch_from_origin above was in flight (the shard lock is
                    // dropped for the duration of that await, and the ghost-cache entry
                    // is updated in place rather than removed until a promotion commits,
                    // so every concurrent racer's Phase 1/2 check can independently reach
                    // Action::Promote for the same key). If so, adopt the winner's entry
                    // rather than pushing a second one: a second push+insert would
                    // overwrite main_cache_map's index for this key while leaving the
                    // first-pushed `entries` slot unreachable -- a permanent, leaked
                    // capacity slot that only random eviction sampling could ever clear.
                    // We deliberately discard our own duplicate fetch/meta here rather
                    // than merging them in; the winning entry's score/last_accessed will
                    // be refreshed by the very next ordinary Hit on this (by definition
                    // hot) key, and the extra fetch_from_origin call is accepted as
                    // bounded, non-corrupting wasted work rather than a correctness bug.
                    if let Some(&idx) = shard.main_cache_map.get(&requested_resource) {
                        return shard.entries[idx].1.payload.clone();
                    }

                    let evicted = if shard.entries.len() >= shard.capacity {
                        shard.execute_eviction_tournament(self.clock.now_millis(), &self.config)
                    } else {
                        None
                    };

                    let idx = shard.entries.len();
                    shard.entries.push((
                        requested_resource.clone(),
                        MainCacheEntry {
                            payload: payload.clone(),
                            meta,
                        },
                    ));
                    shard.main_cache_map.insert(requested_resource.clone(), idx);
                    // The ghost entry may already have been evicted while the fetch was in
                    // flight; removal is a no-op then.
                    shard.remove_ghost(&requested_resource);
                    evicted
                };

                // Told only after the shard lock is released, so the listener may re-enter.
                if let (Some(key), Some(listener)) = (evicted, &self.eviction_listener) {
                    (listener.0)(&key);
                }
                payload
            }
            Action::GhostHit | Action::Miss => fetch_from_origin(&requested_resource).await,
        }
    }
}

/// Where a resource currently lives within its shard.
#[derive(Clone, Copy)]
enum Slot {
    /// Index into the shard's main-cache `entries`.
    Main(usize),
    /// Index into the shard's `ghost_entries`.
    Ghost(usize),
}

enum Action<P> {
    Hit(P),
    Promote(Metadata),
    GhostHit,
    Miss,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use rand::SeedableRng;
    use rand::rngs::StdRng;

    fn meta(score: f32, last_accessed: u64) -> Metadata {
        Metadata {
            last_accessed,
            score,
        }
    }

    fn rng() -> StdRng {
        StdRng::seed_from_u64(0x00D7_AC00)
    }

    /// Enough samples that every entry of a small slice is almost surely drawn.
    fn exhaustive() -> EngineConfig {
        EngineConfig {
            n_samples: 256,
            ..Default::default()
        }
    }

    #[test]
    fn decayed_score_halves_per_half_life_and_cuts_off() {
        let cfg = EngineConfig {
            decay_half_life_ms: 10_000,
            decay_cutoff_half_lives: 3,
            ..Default::default()
        };
        let m = meta(8.0, 1_000);
        assert!((m.decayed_score(1_000, &cfg) - 8.0).abs() < f32::EPSILON);
        assert!((m.decayed_score(10_999, &cfg) - 8.0).abs() < f32::EPSILON);
        assert!((m.decayed_score(11_000, &cfg) - 4.0).abs() < f32::EPSILON);
        assert!((m.decayed_score(21_000, &cfg) - 2.0).abs() < f32::EPSILON);
        assert!(m.decayed_score(31_000, &cfg).abs() < f32::EPSILON);
        // A clock reading before `last_accessed` does not inflate the score.
        assert!((m.decayed_score(0, &cfg) - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn tournament_picks_lowest_decayed_score() {
        let entries = [
            ("a", meta(3.0, 0)),
            ("b", meta(0.5, 0)),
            ("c", meta(2.0, 0)),
        ];
        let victim = tournament_victim(&entries, 0, &exhaustive(), &mut rng());
        assert_eq!(victim, Some(1));
    }

    #[test]
    fn hot_entry_decayed_to_zero_loses_the_tournament() {
        // Default config: halve per second, zero after 32 s. 40 s later the hot entry's
        // stored score of 100 is worth less than a fresh entry's 0.5.
        let entries = [("hot", meta(100.0, 0)), ("fresh", meta(0.5, 40_000))];
        let victim = tournament_victim(&entries, 40_000, &exhaustive(), &mut rng());
        assert_eq!(victim, Some(0));
    }

    #[test]
    fn ties_at_zero_evict_the_least_recently_accessed() {
        let entries = [
            ("newer", meta(9.0, 20_000)),
            ("oldest", meta(9.0, 5_000)),
            ("newest", meta(9.0, 30_000)),
        ];
        // At 100 s every score has decayed to exactly 0.
        let victim = tournament_victim(&entries, 100_000, &exhaustive(), &mut rng());
        assert_eq!(victim, Some(1));
    }

    #[test]
    fn n_samples_controls_how_often_the_minimum_is_found() {
        let entries: Vec<(usize, Metadata)> = (0..10_u16)
            .map(|i| (usize::from(i), meta(f32::from(i), 0)))
            .collect();
        let one = EngineConfig {
            n_samples: 1,
            ..Default::default()
        };
        let mut rng = rng();
        let min_found = |cfg: &EngineConfig, rng: &mut StdRng| {
            (0..200)
                .filter(|_| tournament_victim(&entries, 0, cfg, rng) == Some(0))
                .count()
        };
        assert_eq!(min_found(&exhaustive(), &mut rng), 200);
        // With one sample the minimum is found about 1 time in 10.
        assert!(min_found(&one, &mut rng) < 60);
    }

    #[test]
    fn tournament_on_empty_slice_has_no_victim() {
        let entries: [(u8, Metadata); 0] = [];
        assert_eq!(
            tournament_victim(&entries, 0, &exhaustive(), &mut rng()),
            None
        );
    }

    /// `count` distinct keys that all hash to shard 0.
    fn keys_in_shard_zero(count: usize) -> Vec<String> {
        (0..)
            .map(|i| format!("k{i}"))
            .filter(|k| DtacCacheEngine::<String, ()>::get_shard_id(k) == 0)
            .take(count)
            .collect()
    }

    #[tokio::test]
    async fn bot_scored_ghost_entries_are_evicted_before_human_ones() {
        // One main slot and four ghost slots per shard; sample widely so the tournament
        // almost surely sees every ghost entry.
        let clock = Arc::new(crate::clock::ManualClock::new(0));
        let config = EngineConfig {
            n_samples: 64,
            ..Default::default()
        };
        let engine = DtacCacheEngine::<String, ()>::with_config(NUM_SHARDS, config, clock).unwrap();
        assert_eq!(engine.ghost_capacity_per_shard(), 4);

        let keys = keys_in_shard_zero(22);
        let (humans, bots) = keys.split_at(2);
        for key in humans {
            engine.access(key.clone(), 0.0, |_k| async {}).await;
        }
        // Twenty bot scans churn through the two remaining ghost slots.
        for key in bots {
            engine.access(key.clone(), 1.0, |_k| async {}).await;
        }

        for key in humans {
            let score = engine.debug_score(key);
            assert_eq!(score, Some(1.0), "human ghost entry {key} was evicted");
        }
        assert_eq!(engine.debug_ghost_counts()[0], (4, 4));
    }

    #[test]
    fn swap_remove_indexed_repoints_the_moved_entry() {
        let mut entries = vec![("a", 1), ("b", 2), ("c", 3)];
        let mut map: HashMap<&str, usize> = HashMap::from([("a", 0), ("b", 1), ("c", 2)]);

        assert_eq!(swap_remove_indexed(&mut map, &mut entries, 0), ("a", 1));
        assert_eq!(entries, vec![("c", 3), ("b", 2)]);
        assert_eq!(map, HashMap::from([("c", 0), ("b", 1)]));

        // Removing the last entry moves nothing.
        assert_eq!(swap_remove_indexed(&mut map, &mut entries, 1), ("b", 2));
        assert_eq!(map, HashMap::from([("c", 0)]));
    }
}
