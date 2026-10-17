//! SIEVE (Zhang et al., "SIEVE is Simpler than LRU", NSDI '24), one shard of it.
//!
//! A FIFO queue where each entry has a `visited` bit. A hit only sets the bit. To
//! evict, a hand walks from the oldest entry toward the newest, clearing set bits,
//! and evicts the first entry whose bit was already clear. The hand stays where it
//! stopped, so entries that were visited survive one pass of the hand. New entries go
//! in at the head (newest end) and are never moved.

use crate::engines::ShardPolicy;
use std::collections::HashMap;
use std::hash::Hash;

/// A queue entry. Links are slab indices, so no `unsafe` is needed.
struct Node<K, P> {
    key: K,
    payload: P,
    visited: bool,
    /// The neighbour toward the tail (inserted earlier).
    older: Option<usize>,
    /// The neighbour toward the head (inserted later).
    newer: Option<usize>,
}

/// One SIEVE shard: a slab of nodes linked into a queue, and a key → slot map.
pub struct SieveShard<K, P> {
    map: HashMap<K, usize>,
    /// Never shrinks: once full, every eviction frees the slot the next insert reuses.
    nodes: Vec<Node<K, P>>,
    /// Newest entry.
    head: Option<usize>,
    /// Oldest entry.
    tail: Option<usize>,
    /// Where the next eviction scan starts; `None` means at the tail.
    hand: Option<usize>,
    capacity: usize,
}

impl<K: Eq + Hash + Clone, P> SieveShard<K, P> {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            map: HashMap::with_capacity(capacity),
            nodes: Vec::with_capacity(capacity),
            head: None,
            tail: None,
            hand: None,
            capacity,
        }
    }

    /// Detaches `idx` from the queue, leaving its slot in the slab.
    fn unlink(&mut self, idx: usize) {
        let (older, newer) = (self.nodes[idx].older, self.nodes[idx].newer);
        match older {
            Some(o) => self.nodes[o].newer = newer,
            None => self.tail = newer,
        }
        match newer {
            Some(n) => self.nodes[n].older = older,
            None => self.head = older,
        }
    }

    /// Links `idx` in at the head.
    fn push_head(&mut self, idx: usize) {
        self.nodes[idx].older = self.head;
        self.nodes[idx].newer = None;
        match self.head {
            Some(h) => self.nodes[h].newer = Some(idx),
            None => self.tail = Some(idx),
        }
        self.head = Some(idx);
    }

    /// Runs the hand to the first unvisited entry, unlinks it and returns its slot.
    fn evict(&mut self) -> Option<usize> {
        let mut idx = self.hand.or(self.tail)?;
        // Terminates within one pass: every visited entry passed is cleared.
        while self.nodes[idx].visited {
            self.nodes[idx].visited = false;
            idx = self.nodes[idx].newer.or(self.tail)?;
        }
        self.hand = self.nodes[idx].newer;
        self.unlink(idx);
        self.map.remove(&self.nodes[idx].key);
        Some(idx)
    }

    /// The keys from oldest to newest.
    #[cfg(test)]
    fn queue(&self) -> Vec<K> {
        let mut keys = Vec::new();
        let mut cursor = self.tail;
        while let Some(idx) = cursor {
            keys.push(self.nodes[idx].key.clone());
            cursor = self.nodes[idx].newer;
        }
        keys
    }
}

impl<K: Eq + Hash + Clone + Send + 'static, P: Clone + Send + 'static> ShardPolicy<K, P>
    for SieveShard<K, P>
{
    const NAME: &'static str = "sieve";

    fn with_capacity(capacity: usize) -> Self {
        Self::new(capacity)
    }

    fn get(&mut self, key: &K) -> Option<P> {
        let &idx = self.map.get(key)?;
        self.nodes[idx].visited = true;
        Some(self.nodes[idx].payload.clone())
    }

    fn insert(&mut self, key: K, payload: P, evicted: &mut Vec<K>) {
        if self.map.contains_key(&key) {
            return;
        }
        let node = Node {
            key: key.clone(),
            payload,
            visited: false,
            older: None,
            newer: None,
        };
        let idx = if self.map.len() >= self.capacity
            && let Some(slot) = self.evict()
        {
            evicted.push(std::mem::replace(&mut self.nodes[slot], node).key);
            slot
        } else {
            self.nodes.push(node);
            self.nodes.len() - 1
        };
        self.push_head(idx);
        self.map.insert(key, idx);
    }

    fn keys(&self) -> Vec<K> {
        self.map.keys().cloned().collect()
    }

    fn len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(shard: &mut SieveShard<char, ()>, key: char) -> Vec<char> {
        let mut evicted = Vec::new();
        shard.insert(key, (), &mut evicted);
        evicted
    }

    /// Requests `key` the way the bench does: a hit, or a miss followed by an insert.
    fn request(shard: &mut SieveShard<char, ()>, key: char) -> Vec<char> {
        if shard.get(&key).is_some() {
            Vec::new()
        } else {
            insert(shard, key)
        }
    }

    #[test]
    fn visited_entries_survive_one_pass_of_the_hand() {
        // The paper's walk-through, at capacity 4: A B C D are cached (A oldest) and
        // A and C have been hit since insertion.
        let mut shard = SieveShard::new(4);
        for key in ['A', 'B', 'C', 'D'] {
            assert!(request(&mut shard, key).is_empty());
        }
        request(&mut shard, 'A');
        request(&mut shard, 'C');

        // E: the hand starts at the tail, clears A's bit and evicts B.
        assert_eq!(request(&mut shard, 'E'), vec!['B']);
        assert_eq!(shard.queue(), vec!['A', 'C', 'D', 'E']);
        // F: the hand resumes after B's old spot, clears C's bit and evicts D. A is
        // not looked at again, even though its bit is now clear.
        assert_eq!(request(&mut shard, 'F'), vec!['D']);
        assert_eq!(shard.queue(), vec!['A', 'C', 'E', 'F']);
        // G: E was never hit, so it goes next, still without wrapping round to A.
        assert_eq!(request(&mut shard, 'G'), vec!['E']);
        assert_eq!(shard.queue(), vec!['A', 'C', 'F', 'G']);
        // H, after hits on F and G: the hand clears both, wraps past the head to the
        // tail, and finds A, whose bit the first pass cleared.
        request(&mut shard, 'F');
        request(&mut shard, 'G');
        assert_eq!(request(&mut shard, 'H'), vec!['A']);
        assert_eq!(shard.queue(), vec!['C', 'F', 'G', 'H']);
    }

    #[test]
    fn all_visited_falls_back_to_fifo_after_one_pass() {
        let mut shard = SieveShard::new(3);
        for key in ['A', 'B', 'C'] {
            request(&mut shard, key);
        }
        for key in ['A', 'B', 'C'] {
            request(&mut shard, key);
        }
        // Every bit is set: the hand clears them all, wraps and evicts the oldest.
        assert_eq!(request(&mut shard, 'D'), vec!['A']);
        assert_eq!(shard.queue(), vec!['B', 'C', 'D']);
    }

    #[test]
    fn a_hit_does_not_move_the_entry() {
        let mut shard = SieveShard::new(3);
        for key in ['A', 'B', 'C', 'A'] {
            request(&mut shard, key);
        }
        assert_eq!(shard.queue(), vec!['A', 'B', 'C']);
    }

    #[test]
    fn random_accesses_never_exceed_capacity_or_break_the_links() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(7);
        let mut shard: SieveShard<u32, u32> = SieveShard::new(64);
        for _ in 0..100_000 {
            // Skewed toward small keys so there are both hits and evictions.
            let key = rng.gen_range(0..500_u32).min(rng.gen_range(0..500));
            if shard.get(&key).is_none() {
                let mut evicted = Vec::new();
                shard.insert(key, key, &mut evicted);
                assert!(evicted.len() <= 1);
                assert!(!evicted.contains(&key));
            }
            assert!(shard.len() <= 64);
        }
        let mut queue = shard.queue();
        assert_eq!(queue.len(), shard.len());
        for (key, &idx) in &shard.map {
            assert_eq!(shard.nodes[idx].key, *key);
            assert_eq!(shard.nodes[idx].payload, *key);
        }
        queue.sort_unstable();
        queue.dedup();
        assert_eq!(queue.len(), shard.len(), "a key is linked twice");
    }

    #[test]
    fn reinserting_a_resident_key_is_a_no_op() {
        let mut shard = SieveShard::new(2);
        insert(&mut shard, 'A');
        assert!(insert(&mut shard, 'A').is_empty());
        assert_eq!(shard.queue(), vec!['A']);
    }
}
