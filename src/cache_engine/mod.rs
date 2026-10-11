//! Module for the randomized caching algorithm considering AI scores.
//!
//! This module provides the core caching engine that manages cache entries
//! dynamically based on traffic patterns and heuristic AI scoring.

use rand::Rng;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Instant;

/// The number of independent mutex locks used for lock sharding.
pub const NUM_SHARDS: usize = 256;

/// The number of random items selected during an eviction tournament.
pub const N_SAMPLES: usize = 5;

/// The score threshold required to promote an item from Ghost Cache to Main Cache.
pub const PROMOTION_THRESHOLD: f32 = 2.0;

/// Metadata for a cached resource, tracking access time and EWMA score.
#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    /// The last time this resource was accessed.
    pub last_accessed: Instant,
    /// The current EWMA popularity score.
    pub score: f32,
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
    /// Metadata-only ghost cache for cold-start mitigation.
    ghost_cache: HashMap<K, Metadata>,
    /// The maximum number of items allowed in the main cache for this shard.
    capacity: usize,
}

impl<K: Eq + Hash + Clone, P: Clone> CacheShard<K, P> {
    /// Creates a new cache shard with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            main_cache_map: HashMap::new(),
            entries: Vec::with_capacity(capacity),
            ghost_cache: HashMap::new(),
            capacity,
        }
    }

    /// Executes the randomized eviction tournament (Algorithm 2).
    fn execute_eviction_tournament(&mut self, current_time: Instant) {
        if self.entries.is_empty() {
            return;
        }

        let mut rng = rand::thread_rng();
        let mut min_score = f32::INFINITY;
        let mut eviction_target_idx = 0;

        // Sample N random items and find the one with the lowest decayed score
        for _ in 0..N_SAMPLES {
            let idx = rng.gen_range(0..self.entries.len());
            let (_, entry) = &self.entries[idx];

            let dt = current_time
                .duration_since(entry.meta.last_accessed)
                .as_secs();

            // Time-Decay (Bitwise Shift approximation for floats)
            // If delta time is large enough, score naturally decays toward zero.
            let current_decayed_score = if dt >= 32 {
                0.0
            } else {
                entry.meta.score / (1u64 << dt) as f32
            };

            if current_decayed_score < min_score {
                min_score = current_decayed_score;
                eviction_target_idx = idx;
            }
        }

        // Evict the target item in O(1) time using swap_remove
        let (evicted_key, _) = self.entries.swap_remove(eviction_target_idx);
        self.main_cache_map.remove(&evicted_key);

        // If swap_remove moved the last element to the evicted index, update its map entry
        if eviction_target_idx < self.entries.len() {
            let moved_key = self.entries[eviction_target_idx].0.clone();
            self.main_cache_map.insert(moved_key, eviction_target_idx);
        }
    }
}

/// The Dynamic Traffic-Aware Cache (DTAC) Engine.
/// Implements a Randomized Eviction architecture combined with a resource-specific
/// Exponentially Weighted Moving Average (EWMA) and a metadata-only Ghost Cache.
#[derive(Debug)]
pub struct DtacCacheEngine<K, P> {
    shards: Vec<Mutex<CacheShard<K, P>>>,
}

impl<K: Eq + Hash + Clone, P: Clone> DtacCacheEngine<K, P> {
    /// Creates a new DTAC Engine with the specified total capacity.
    /// The capacity is divided evenly among all shards.
    pub fn new(total_capacity: usize) -> Self {
        let capacity_per_shard = (total_capacity / NUM_SHARDS).max(1);
        let mut shards = Vec::with_capacity(NUM_SHARDS);
        for _ in 0..NUM_SHARDS {
            shards.push(Mutex::new(CacheShard::new(capacity_per_shard)));
        }
        Self { shards }
    }

    /// Hashes the requested resource to determine its specific concurrency bucket.
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
    ///
    /// # Panics
    ///
    /// Panics if a shard's mutex is poisoned (i.e. a previous lock holder panicked).
    pub fn debug_integrity_counts(&self) -> (usize, usize) {
        self.shards.iter().fold((0, 0), |(e, m), shard| {
            let shard = shard.lock().unwrap();
            (e + shard.entries.len(), m + shard.main_cache_map.len())
        })
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

        let (action, payload) = {
            let mut shard = self.shards[shard_id].lock().unwrap();
            let current_time = Instant::now();

            // Phase 2: Cache Hit Evaluation & Score Update
            let in_main = shard.main_cache_map.contains_key(&requested_resource);
            let in_ghost = shard.ghost_cache.contains_key(&requested_resource);

            if in_main || in_ghost {
                // Get metadata
                let mut meta = if in_main {
                    let idx = shard.main_cache_map[&requested_resource];
                    shard.entries[idx].1.meta.clone()
                } else {
                    shard.ghost_cache[&requested_resource].clone()
                };

                // Time-Decay (Bitwise Shift) and AI-Score Integration
                let dt = current_time.duration_since(meta.last_accessed).as_secs();
                let decayed_score = if dt >= 32 {
                    0.0
                } else {
                    meta.score / (1u64 << dt) as f32
                };

                // Increment value is inversely proportional to AI-Score (request_bot_prob)
                let new_score = decayed_score + (1.0 - request_bot_prob);

                meta.last_accessed = current_time;
                meta.score = new_score;

                // Promotion from Ghost Cache to Main Cache
                if in_ghost && new_score >= PROMOTION_THRESHOLD {
                    shard.ghost_cache.insert(requested_resource.clone(), meta); // Temporarily store updated meta
                    (Action::Promote(meta), None)
                } else if in_main {
                    let idx = shard.main_cache_map[&requested_resource];
                    shard.entries[idx].1.meta = meta;
                    (Action::Hit, Some(shard.entries[idx].1.payload.clone()))
                } else {
                    shard.ghost_cache.insert(requested_resource.clone(), meta);
                    (Action::GhostHit, None)
                }
            } else {
                // Phase 3: Cache Miss (Ghost Cache Insertion)
                let initial_score = 1.0 - request_bot_prob;
                shard.ghost_cache.insert(
                    requested_resource.clone(),
                    Metadata {
                        last_accessed: current_time,
                        score: initial_score,
                    },
                );
                (Action::Miss, None)
            }
        };

        match action {
            Action::Hit => payload.unwrap(),
            Action::Promote(meta) => {
                let payload = fetch_from_origin(&requested_resource).await;
                let mut shard = self.shards[shard_id].lock().unwrap();

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

                let current_time = Instant::now();

                if shard.entries.len() >= shard.capacity {
                    shard.execute_eviction_tournament(current_time);
                }

                let idx = shard.entries.len();
                shard.entries.push((
                    requested_resource.clone(),
                    MainCacheEntry {
                        payload: payload.clone(),
                        meta,
                    },
                ));
                shard.main_cache_map.insert(requested_resource.clone(), idx);
                shard.ghost_cache.remove(&requested_resource);

                payload
            }
            Action::GhostHit | Action::Miss => fetch_from_origin(&requested_resource).await,
        }
    }
}

enum Action {
    Hit,
    Promote(Metadata),
    GhostHit,
    Miss,
}
