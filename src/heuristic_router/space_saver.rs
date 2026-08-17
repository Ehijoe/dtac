//! Bounded-memory tracker for IP states.

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;

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
}

/// SpaceSaver tracks recent IPs in strictly bounded memory, evicting the lowest-volume
/// "mouse" IPs when full.
#[derive(Debug)]
pub struct SpaceSaver {
    capacity: usize,
    state: Mutex<State>,
}

impl SpaceSaver {
    /// Initializes a new SpaceSaver with a strict maximum memory capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(State {
                lookup: HashMap::with_capacity(capacity),
                counts: BTreeSet::new(),
            }),
        }
    }

    /// Fetches the state for `client_ip`. If it doesn't exist, adds it.
    /// If memory is full, evicts the lowest-volume IP.
    pub async fn get_or_add_and_evict(&self, client_ip: IpAddr) -> IpState {
        let state = self.state.lock().unwrap();

        // Case 1: The IP is already being tracked.
        if let Some(existing_state) = state.lookup.get(&client_ip).cloned() {
            return existing_state;
        }

        // Case 2: The IP is new, and we have not hit our memory limit.
        if state.lookup.len() < self.capacity {
            // Return an initialized state and let `update` commit it.
            return IpState { count: 1, ..Default::default() };
        }

        // Case 3: Memory is FULL.
        // We evict the lowest count, increment it, and start a new state with that base count to bound the error rate.
        if let Some(&(min_count, _)) = state.counts.first() {
            // We just return a new state inheriting the `min_count` as the base.
            let inherited_count = min_count + 1;
            return IpState { count: inherited_count, ..Default::default() };
        }

        // Fallback for an empty capacity bounds (which shouldn't happen unless capacity=0)
        IpState { count: 1, ..Default::default() }
    }

    /// Updates the state for the given `client_ip`.
    /// This acts as the commit phase of the state, adjusting indices as required.
    pub async fn update(&self, client_ip: IpAddr, new_ip_state: IpState) {
        let mut state = self.state.lock().unwrap();

        // If the IP is already tracked, remove the old count entry from the BTreeSet.
        // Needs cloned to avoid borrowing `state` during removal
        let old_count_opt = state.lookup.get(&client_ip).map(|s| s.count);
        
        if let Some(old_count) = old_count_opt {
            state.counts.remove(&(old_count, client_ip));
            state.counts.insert((new_ip_state.count, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
            return;
        }

        // If not tracked and under capacity, insert it.
        if state.lookup.len() < self.capacity {
            state.counts.insert((new_ip_state.count, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
            return;
        }

        // If memory is full, we must evict the lowest-count item to make space.
        if let Some((_min_count, min_ip)) = state.counts.pop_first() {
            state.lookup.remove(&min_ip);
            
            // Insert the new IP with its new state
            state.counts.insert((new_ip_state.count, client_ip));
            state.lookup.insert(client_ip, new_ip_state);
        }
    }
}
