//! S3-FIFO (Yang et al., "FIFO queues are all you need for cache eviction", SOSP '23),
//! one shard of it.
//!
//! Three FIFO queues: a small queue S (10% of the capacity) that every new key enters,
//! a main queue M (the other 90%), and a ghost queue G of recently evicted keys (no
//! payloads, as many as M holds). Each entry has a 2-bit frequency, capped at 3, that
//! hits increment. Evicting from S moves entries hit more than once to M and the rest
//! to G. A miss on a key in G inserts it straight into M. Evicting from M reinserts
//! entries with a non-zero frequency at the head, decrementing it, until one with
//! frequency 0 comes up.

use crate::engines::ShardPolicy;
use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

/// Highest value of the 2-bit frequency counter.
const MAX_FREQ: u8 = 3;

/// Share of a shard's capacity given to the small queue.
const SMALL_SHARE: f64 = 0.1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Queue {
    Small,
    Main,
}

struct Entry<P> {
    payload: P,
    freq: u8,
    queue: Queue,
}

/// One S3-FIFO shard. Queues hold keys, head at the front and tail at the back.
pub struct S3FifoShard<K, P> {
    entries: HashMap<K, Entry<P>>,
    small: VecDeque<K>,
    main: VecDeque<K>,
    /// Keys evicted from S in insertion order, tagged with a sequence number. A key that
    /// leaves the ghost early (re-admitted to M) stays in the deque as a stale tag until
    /// it reaches the front; `ghost_index` holds only the live tags.
    ghost: VecDeque<(K, u64)>,
    ghost_index: HashMap<K, u64>,
    next_ghost_seq: u64,
    capacity: usize,
    small_capacity: usize,
    main_capacity: usize,
    ghost_capacity: usize,
}

impl<K: Eq + Hash + Clone, P> S3FifoShard<K, P> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        // Rounded share, at least one entry. Capacities are far below 2^52.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let small_capacity = ((capacity as f64 * SMALL_SHARE).round() as usize).clamp(1, capacity);
        let main_capacity = capacity - small_capacity;
        Self {
            entries: HashMap::with_capacity(capacity),
            small: VecDeque::new(),
            main: VecDeque::new(),
            ghost: VecDeque::new(),
            ghost_index: HashMap::new(),
            next_ghost_seq: 0,
            capacity,
            small_capacity,
            main_capacity,
            ghost_capacity: main_capacity.max(1),
        }
    }

    /// Makes room for one entry.
    fn evict(&mut self, evicted: &mut Vec<K>) {
        if self.small.len() >= self.small_capacity || self.main.is_empty() {
            self.evict_small(evicted);
        } else {
            self.evict_main(evicted);
        }
    }

    /// Takes entries off S's tail until one goes to the ghost. Entries hit more than once
    /// move to M instead, which may in turn evict from M.
    fn evict_small(&mut self, evicted: &mut Vec<K>) {
        while let Some(key) = self.small.pop_back() {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if entry.freq > 1 {
                entry.queue = Queue::Main;
                self.main.push_front(key);
                if self.main.len() > self.main_capacity {
                    self.evict_main(evicted);
                }
            } else {
                self.entries.remove(&key);
                self.insert_ghost(key.clone());
                evicted.push(key);
                return;
            }
        }
    }

    /// Takes entries off M's tail, reinserting those with a non-zero frequency (one
    /// less each time) at the head, until one with frequency 0 is evicted.
    fn evict_main(&mut self, evicted: &mut Vec<K>) {
        while let Some(key) = self.main.pop_back() {
            let Some(entry) = self.entries.get_mut(&key) else {
                continue;
            };
            if entry.freq > 0 {
                entry.freq -= 1;
                self.main.push_front(key);
            } else {
                self.entries.remove(&key);
                evicted.push(key);
                return;
            }
        }
    }

    fn insert_ghost(&mut self, key: K) {
        let seq = self.next_ghost_seq;
        self.next_ghost_seq += 1;
        self.ghost_index.insert(key.clone(), seq);
        self.ghost.push_back((key, seq));
        while self.ghost_index.len() > self.ghost_capacity {
            let Some((old, old_seq)) = self.ghost.pop_front() else {
                break;
            };
            if self.ghost_index.get(&old) == Some(&old_seq) {
                self.ghost_index.remove(&old);
            }
        }
        // Stale tags only pile up if keys keep leaving the ghost early; drop them once
        // they outnumber the live ones.
        if self.ghost.len() > 2 * self.ghost_capacity {
            let index = &self.ghost_index;
            self.ghost
                .retain(|(key, seq)| index.get(key).is_some_and(|live| live == seq));
        }
    }

    #[cfg(test)]
    fn queue_of(&self, key: &K) -> Option<Queue> {
        self.entries.get(key).map(|e| e.queue)
    }

    #[cfg(test)]
    fn in_ghost(&self, key: &K) -> bool {
        self.ghost_index.contains_key(key)
    }

    /// Checks that the queues and the entry map agree.
    #[cfg(test)]
    fn assert_consistent(&self) {
        assert!(self.entries.len() <= self.capacity);
        assert_eq!(self.small.len() + self.main.len(), self.entries.len());
        for key in &self.small {
            assert_eq!(self.queue_of(key), Some(Queue::Small));
        }
        for key in &self.main {
            assert_eq!(self.queue_of(key), Some(Queue::Main));
        }
        assert!(self.ghost_index.len() <= self.ghost_capacity);
        for key in self.ghost_index.keys() {
            assert!(
                !self.entries.contains_key(key),
                "a key is cached and a ghost"
            );
        }
    }
}

impl<K: Eq + Hash + Clone + Send + 'static, P: Clone + Send + 'static> ShardPolicy<K, P>
    for S3FifoShard<K, P>
{
    const NAME: &'static str = "s3fifo";

    fn with_capacity(capacity: usize) -> Self {
        Self::new(capacity)
    }

    fn get(&mut self, key: &K) -> Option<P> {
        let entry = self.entries.get_mut(key)?;
        entry.freq = (entry.freq + 1).min(MAX_FREQ);
        Some(entry.payload.clone())
    }

    fn insert(&mut self, key: K, payload: P, evicted: &mut Vec<K>) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.entries.len() >= self.capacity {
            self.evict(evicted);
        }
        let queue = if self.ghost_index.remove(&key).is_some() {
            self.main.push_front(key.clone());
            Queue::Main
        } else {
            self.small.push_front(key.clone());
            Queue::Small
        };
        self.entries.insert(
            key,
            Entry {
                payload,
                freq: 0,
                queue,
            },
        );
    }

    fn keys(&self) -> Vec<K> {
        self.entries.keys().cloned().collect()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn ghost_len(&self) -> Option<usize> {
        Some(self.ghost_index.len())
    }

    fn ghost_capacity(&self) -> Option<usize> {
        Some(self.ghost_capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests `key` the way the bench does; returns the keys it evicted.
    fn request(shard: &mut S3FifoShard<u32, ()>, key: u32) -> Vec<u32> {
        let mut evicted = Vec::new();
        if shard.get(&key).is_none() {
            shard.insert(key, (), &mut evicted);
        }
        shard.assert_consistent();
        evicted
    }

    #[test]
    fn queue_sizes_follow_the_ten_percent_split() {
        let shard = S3FifoShard::<u32, ()>::new(64);
        assert_eq!((shard.small_capacity, shard.main_capacity), (6, 58));
        assert_eq!(shard.ghost_capacity, 58);
        let tiny = S3FifoShard::<u32, ()>::new(1);
        assert_eq!((tiny.small_capacity, tiny.main_capacity), (1, 0));
    }

    #[test]
    fn a_one_hit_wonder_scan_never_reaches_main() {
        let mut shard = S3FifoShard::new(64);
        for key in 0..10_000 {
            let evicted = request(&mut shard, key);
            assert!(shard.main.is_empty(), "scan key reached M at {key}");
            assert!(evicted.len() <= 1);
        }
        assert_eq!(shard.ghost_index.len(), shard.ghost_capacity);
    }

    #[test]
    fn a_key_requested_again_after_leaving_small_enters_main_from_the_ghost() {
        // Capacity 10: S holds 1 entry, M 9.
        let mut shard = S3FifoShard::new(10);
        request(&mut shard, 1);
        // Fill the cache with keys that pass through S without hits.
        for key in 100..109 {
            request(&mut shard, key);
        }
        assert_eq!(
            request(&mut shard, 200),
            vec![1],
            "1 was S's oldest, never hit"
        );
        assert!(shard.in_ghost(&1));

        request(&mut shard, 1);
        assert_eq!(shard.queue_of(&1), Some(Queue::Main));
        assert!(!shard.in_ghost(&1));
    }

    #[test]
    fn an_entry_hit_twice_in_small_moves_to_main_on_eviction() {
        let mut shard = S3FifoShard::new(10);
        request(&mut shard, 1);
        request(&mut shard, 1);
        request(&mut shard, 1); // freq 2
        for key in 100..120 {
            request(&mut shard, key);
        }
        assert_eq!(shard.queue_of(&1), Some(Queue::Main));
    }

    /// Puts `key` into M through the ghost: in, out of S, then requested again.
    /// Returns every key evicted on the way.
    fn admit_to_main(shard: &mut S3FifoShard<u32, ()>, key: u32, filler: &mut u32) -> Vec<u32> {
        let mut evicted = request(shard, key);
        while !shard.in_ghost(&key) {
            *filler += 1;
            evicted.extend(request(shard, *filler));
        }
        evicted.extend(request(shard, key));
        assert_eq!(shard.queue_of(&key), Some(Queue::Main));
        evicted
    }

    #[test]
    fn a_frequent_main_entry_survives_reinsertion() {
        let mut shard = S3FifoShard::new(10);
        let mut filler = 1_000;
        admit_to_main(&mut shard, 1, &mut filler);
        for _ in 0..5 {
            request(&mut shard, 1);
        }
        assert_eq!(shard.entries[&1].freq, MAX_FREQ, "capped at 3");

        // Admit never-hit keys to M behind 1 until M has to evict one of them. 1 is at
        // M's tail first, so it must have been reinserted, at the cost of a unit of
        // frequency, for a younger key to go.
        let mut key = 1;
        loop {
            key += 1;
            let evicted = admit_to_main(&mut shard, key, &mut filler);
            if evicted.iter().any(|&k| (2..key).contains(&k)) {
                break;
            }
            assert!(key < 100, "M never evicted");
        }
        assert_eq!(
            shard.queue_of(&1),
            Some(Queue::Main),
            "the hot entry was evicted"
        );
        assert_eq!(shard.entries[&1].freq, MAX_FREQ - 1);
    }

    #[test]
    fn random_accesses_never_exceed_capacity() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(3);
        let mut shard = S3FifoShard::new(64);
        for _ in 0..100_000 {
            let key = rng.gen_range(0..500_u32).min(rng.gen_range(0..500));
            let evicted = request(&mut shard, key);
            assert!(!evicted.contains(&key));
        }
        assert_eq!(shard.entries.len(), 64);
        assert!(!shard.main.is_empty(), "skewed traffic should populate M");
    }

    #[test]
    fn reinserting_a_resident_key_is_a_no_op() {
        let mut shard = S3FifoShard::new(4);
        let mut evicted = Vec::new();
        shard.insert(1, (), &mut evicted);
        shard.insert(1, (), &mut evicted);
        assert_eq!(evicted, Vec::<i32>::new());
        assert_eq!(shard.small.len(), 1);
    }
}
