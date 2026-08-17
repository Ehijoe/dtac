//! Bounded-memory tracker for IP states.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// State for a single IP address.
#[derive(Debug, Clone, Copy, Default)]
pub struct IpState {
    /// Timestamp (in seconds) when this IP was last seen.
    pub last_seen: u64,
    /// EWMA Velocity, stored with 10 fractional bits (i.e. * 1024).
    pub velocity: u32,
    /// Flag indicating whether this IP is suspected of missing auxiliary assets.
    pub missing_auxiliary_assets: bool,
    /// Counter to keep track of how many times the IP has been requested.
    pub count: usize,
}

#[derive(Debug)]
struct State {
    // Maps IP address to its current estimated state.
    lookup: HashMap<IpAddr, IpState>,
    // Ordered set of (Count, IP) to easily pop the minimum count.
    // BTreeSet naturally orders tuples by the first element, then the second.
    counts: BTreeSet<(usize, IpAddr)>,
    // Ordered set of (last_seen, IP) to easily pop the oldest IPs.
    times: BTreeSet<(u64, IpAddr)>,
}

/// SpaceSaver tracks recent IPs in strictly bounded memory, evicting the lowest-volume
/// "mouse" IPs when full.
#[derive(Debug)]
pub struct SpaceSaver {
    capacity: usize,
    ttl_seconds: u64, // How long an IP can be inactive before it is evicted
    state: Mutex<State>,
}

impl SpaceSaver {
    /// Initializes a new SpaceSaver with a strict maximum memory capacity.
    pub fn new(capacity: usize, ttl_seconds: u64) -> Self {
        Self {
            capacity,
            ttl_seconds,
            state: Mutex::new(State {
                lookup: HashMap::with_capacity(capacity),
                counts: BTreeSet::new(),
                times: BTreeSet::new(),
            }),
        }
    }

    /// Helper to get the current time in seconds
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Removes all IPs that haven't been seen within `ttl_seconds`
    fn prune_stale(state: &mut State, ttl_seconds: u64, now: u64) {
        let expiration_threshold = now.saturating_sub(ttl_seconds);

        // Repeatedly check the oldest item
        while let Some(&(oldest_time, oldest_ip)) = state.times.first() {
            if oldest_time < expiration_threshold {
                // The IP is stale, remove it from all three collections
                if let Some(removed_state) = state.lookup.remove(&oldest_ip) {
                    state.counts.remove(&(removed_state.count, oldest_ip));
                }
                state.times.pop_first();
            } else {
                // The oldest IP is still valid, so all subsequent IPs are valid too
                break;
            }
        }
    }

    /// Fetches the state for `client_ip`. If it doesn't exist, adds it.
    /// If memory is full, evicts the lowest-volume IP.
    pub async fn get_or_add_and_evict(&self, client_ip: IpAddr) -> IpState {
        // Note: Made `state` mutable so we can prune it
        let mut state = self.state.lock().unwrap();
        let now = Self::now();

        // 1. Clean up stale entries first to free up capacity
        Self::prune_stale(&mut state, self.ttl_seconds, now);

        // Case 1: The IP is already being tracked.
        if let Some(existing_state) = state.lookup.get(&client_ip).cloned() {
            return existing_state;
        }

        // Case 2: The IP is new, and we have not hit our memory limit.
        if state.lookup.len() < self.capacity {
            return IpState { count: 1, last_seen: now, ..Default::default() };
        }

        // Case 3: Memory is FULL.
        // Standard SpaceSaver eviction (because we already removed expired ones)
        if let Some(&(min_count, _)) = state.counts.first() {
            let inherited_count = min_count + 1;
            return IpState { count: inherited_count, last_seen: now, ..Default::default() };
        }

        IpState { count: 1, last_seen: now, ..Default::default() }
    }

    /// Updates the state for the given `client_ip`.
    /// This acts as the commit phase of the state, adjusting indices as required.
    pub async fn update(&self, client_ip: IpAddr, mut new_ip_state: IpState) {
        let mut state = self.state.lock().unwrap();
        
        // Ensure the last_seen time is updated
        new_ip_state.last_seen = Self::now();

        let old_state_opt = state.lookup.get(&client_ip).cloned();
        
        if let Some(old_state) = old_state_opt {
            // Remove old entries from both BTreeSets
            state.counts.remove(&(old_state.count, client_ip));
            state.times.remove(&(old_state.last_seen, client_ip));
            
            // Insert updated state
            state.counts.insert((new_ip_state.count, client_ip));
            state.times.insert((new_ip_state.last_seen, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
            return;
        }

        // If not tracked and under capacity, insert it.
        if state.lookup.len() < self.capacity {
            state.counts.insert((new_ip_state.count, client_ip));
            state.times.insert((new_ip_state.last_seen, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
            return;
        }

        // If memory is full, evict the lowest-count item to make space.
        if let Some((_min_count, min_ip)) = state.counts.pop_first() {
            if let Some(evicted_state) = state.lookup.remove(&min_ip) {
                // Must also remove from times index!
                state.times.remove(&(evicted_state.last_seen, min_ip));
            }
            
            // Insert the new IP
            state.counts.insert((new_ip_state.count, client_ip));
            state.times.insert((new_ip_state.last_seen, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
        }
    }
}
