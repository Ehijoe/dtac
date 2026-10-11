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
    ///
    /// Derived from `page_requests` and `has_fetched_asset` by
    /// [`record_request`](Self::record_request).
    pub missing_auxiliary_assets: bool,
    /// Counter to keep track of how many times the IP has been requested.
    pub count: usize,
    /// Number of non-asset (page) requests seen from this IP (saturating).
    pub page_requests: u16,
    /// Whether this IP has ever fetched an auxiliary asset (favicon, CSS, JS, ...).
    pub has_fetched_asset: bool,
}

impl IpState {
    /// Records one request for the "Shy Guys" heuristic and refreshes
    /// `missing_auxiliary_assets`.
    ///
    /// Browsers fetch auxiliary assets once and then cache them, so the IP is flagged
    /// only if it has made at least `shy_page_threshold` page requests and has *never*
    /// fetched an asset.
    pub const fn record_request(&mut self, is_auxiliary_asset: bool, shy_page_threshold: u16) {
        if is_auxiliary_asset {
            self.has_fetched_asset = true;
        } else {
            self.page_requests = self.page_requests.saturating_add(1);
        }
        self.missing_auxiliary_assets =
            !self.has_fetched_asset && self.page_requests >= shy_page_threshold;
    }
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
    half_life_seconds: u64, // How long it takes for an IP's count to halve
    state: Mutex<State>,
}

impl SpaceSaver {
    /// Initializes a new SpaceSaver with a strict maximum memory capacity.
    pub fn new(capacity: usize, half_life_seconds: u64) -> Self {
        Self {
            capacity,
            half_life_seconds,
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

    /// Decays the counts of old IPs and removes those that drop to 0.
    fn decay_and_prune(state: &mut State, half_life_seconds: u64, now: u64) {
        if half_life_seconds == 0 {
            return;
        }

        let threshold = now.saturating_sub(half_life_seconds);

        // Repeatedly check the oldest item
        while let Some(&(oldest_time, oldest_ip)) = state.times.first() {
            if oldest_time <= threshold {
                if let Some(mut removed_state) = state.lookup.remove(&oldest_ip) {
                    state.counts.remove(&(removed_state.count, oldest_ip));
                    state.times.pop_first();

                    let elapsed = now.saturating_sub(removed_state.last_seen) as f64;
                    let hl = half_life_seconds as f64;
                    let factor = 2.0_f64.powf(-(elapsed / hl));
                    let mut new_count = ((removed_state.count as f64) * factor).round() as usize;

                    // Ensure count strictly decreases if at least one half-life has passed
                    if new_count == removed_state.count && elapsed >= hl && new_count > 0 {
                        new_count -= 1;
                    }

                    if new_count > 0 {
                        removed_state.count = new_count;
                        removed_state.last_seen = now;
                        state.counts.insert((new_count, oldest_ip));
                        state.times.insert((now, oldest_ip));
                        state.lookup.insert(oldest_ip, removed_state);
                    }
                } else {
                    state.times.pop_first();
                }
            } else {
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

        // 1. Clean up entries by decaying counts and removing zeros
        Self::decay_and_prune(&mut state, self.half_life_seconds, now);

        // Case 1: The IP is already being tracked.
        if let Some(mut existing_state) = state.lookup.get(&client_ip).cloned() {
            let elapsed = now.saturating_sub(existing_state.last_seen) as f64;
            let hl = self.half_life_seconds as f64;
            if hl > 0.0 && elapsed > 0.0 {
                let factor = 2.0_f64.powf(-(elapsed / hl));
                let mut new_count = ((existing_state.count as f64) * factor).round() as usize;

                if new_count == existing_state.count && elapsed >= hl && new_count > 0 {
                    new_count -= 1;
                }

                existing_state.count = new_count;
            }
            return existing_state;
        }

        // Case 2: The IP is new, and we have not hit our memory limit.
        if state.lookup.len() < self.capacity {
            return IpState {
                count: 1,
                last_seen: now,
                ..Default::default()
            };
        }

        // Case 3: Memory is FULL.
        // Evict the item with the minimum stored count, but compute its true decayed count
        if let Some(&(min_count, min_ip)) = state.counts.first() {
            let min_state = state.lookup.get(&min_ip).unwrap();
            let elapsed = now.saturating_sub(min_state.last_seen) as f64;
            let hl = self.half_life_seconds as f64;
            let factor = if hl > 0.0 {
                2.0_f64.powf(-(elapsed / hl))
            } else {
                1.0
            };
            let mut decayed_min_count = ((min_count as f64) * factor).round() as usize;

            if decayed_min_count == min_count && elapsed >= hl && decayed_min_count > 0 {
                decayed_min_count -= 1;
            }

            let inherited_count = decayed_min_count + 1;
            return IpState {
                count: inherited_count,
                last_seen: now,
                ..Default::default()
            };
        }

        IpState {
            count: 1,
            last_seen: now,
            ..Default::default()
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_request_flags_only_after_threshold_pages_without_assets() {
        let mut state = IpState::default();
        for _ in 0..4 {
            state.record_request(false, 5);
            assert!(!state.missing_auxiliary_assets);
        }
        state.record_request(false, 5);
        assert!(state.missing_auxiliary_assets);
        assert_eq!(state.page_requests, 5);

        // A single asset fetch clears the flag for good.
        state.record_request(true, 5);
        assert!(!state.missing_auxiliary_assets);
        state.record_request(false, 5);
        assert!(!state.missing_auxiliary_assets);
    }

    #[test]
    fn record_request_page_count_saturates() {
        let mut state = IpState {
            page_requests: u16::MAX,
            ..Default::default()
        };
        state.record_request(false, 5);
        assert_eq!(state.page_requests, u16::MAX);
    }

    #[tokio::test]
    async fn test_space_saver_basic_add_and_update() {
        let saver = SpaceSaver::new(10, 3600);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        let mut state = saver.get_or_add_and_evict(ip).await;
        assert_eq!(state.count, 1);

        state.count = 5;
        state.velocity = 100;
        saver.update(ip, state).await;

        let new_state = saver.get_or_add_and_evict(ip).await;
        assert_eq!(new_state.count, 5);
        assert_eq!(new_state.velocity, 100);
    }

    #[tokio::test]
    async fn test_space_saver_eviction() {
        let saver = SpaceSaver::new(2, 3600);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        let mut state1 = saver.get_or_add_and_evict(ip1).await;
        state1.count = 10;
        saver.update(ip1, state1).await;

        let mut state2 = saver.get_or_add_and_evict(ip2).await;
        state2.count = 5;
        saver.update(ip2, state2).await;

        // This should evict ip2 because it has the lowest count (5) compared to ip1 (10)
        let state3 = saver.get_or_add_and_evict(ip3).await;
        assert_eq!(state3.count, 6); // Inherits min_count + 1 = 5 + 1
    }

    #[test]
    fn test_decay_and_prune_logic() {
        let mut state = State {
            lookup: HashMap::new(),
            counts: BTreeSet::new(),
            times: BTreeSet::new(),
        };

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        // IP1 is fresh (now = 1000, last_seen = 1000)
        state.lookup.insert(
            ip1,
            IpState {
                count: 10,
                last_seen: 1000,
                ..Default::default()
            },
        );
        state.counts.insert((10, ip1));
        state.times.insert((1000, ip1));

        // IP2 is old enough to decay significantly (last_seen = 600, half_life = 200) -> 2 half lives
        // Original count = 10. Decayed = 10 * 2^(-2) = 2.5 -> 3
        state.lookup.insert(
            ip2,
            IpState {
                count: 10,
                last_seen: 600,
                ..Default::default()
            },
        );
        state.counts.insert((10, ip2));
        state.times.insert((600, ip2));

        // IP3 is very old (last_seen = 0), will decay to 0
        state.lookup.insert(
            ip3,
            IpState {
                count: 1,
                last_seen: 0,
                ..Default::default()
            },
        );
        state.counts.insert((1, ip3));
        state.times.insert((0, ip3));

        // Decay with half_life = 200, now = 1000. Threshold = 800.
        // IP3 (0) should be removed. IP2 (600) should be decayed and kept. IP1 (1000) should stay.
        SpaceSaver::decay_and_prune(&mut state, 200, 1000);

        assert!(state.lookup.contains_key(&ip1));
        assert!(state.lookup.contains_key(&ip2));
        assert!(!state.lookup.contains_key(&ip3));
        assert_eq!(state.counts.len(), 2);
        assert_eq!(state.times.len(), 2);

        let ip2_state = state.lookup.get(&ip2).unwrap();
        assert_eq!(ip2_state.count, 3);
        assert_eq!(ip2_state.last_seen, 1000);
    }

    #[tokio::test]
    async fn test_update_untracked_when_full() {
        let saver = SpaceSaver::new(2, 3600);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        saver
            .update(
                ip1,
                IpState {
                    count: 10,
                    ..Default::default()
                },
            )
            .await;
        saver
            .update(
                ip2,
                IpState {
                    count: 5,
                    ..Default::default()
                },
            )
            .await;

        // Memory is full. Update an untracked IP.
        saver
            .update(
                ip3,
                IpState {
                    count: 20,
                    ..Default::default()
                },
            )
            .await;

        let state = saver.state.lock().unwrap();
        // ip2 should be evicted since it has the lowest count (5)
        assert!(state.lookup.contains_key(&ip1));
        assert!(!state.lookup.contains_key(&ip2));
        assert!(state.lookup.contains_key(&ip3));
        assert_eq!(state.lookup.get(&ip3).unwrap().count, 20);
    }

    #[tokio::test]
    async fn test_update_existing_updates_indices() {
        let saver = SpaceSaver::new(2, 3600);
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();

        saver
            .update(
                ip1,
                IpState {
                    count: 10,
                    ..Default::default()
                },
            )
            .await;

        {
            let state = saver.state.lock().unwrap();
            assert_eq!(state.counts.len(), 1);
            assert_eq!(state.times.len(), 1);
            assert!(state.counts.contains(&(10, ip1)));
        }

        // Update the count of the existing IP
        saver
            .update(
                ip1,
                IpState {
                    count: 20,
                    ..Default::default()
                },
            )
            .await;

        {
            let state = saver.state.lock().unwrap();
            assert_eq!(state.counts.len(), 1);
            assert_eq!(state.times.len(), 1);
            assert!(state.counts.contains(&(20, ip1)));
            assert!(!state.counts.contains(&(10, ip1)));
        }
    }

    #[tokio::test]
    async fn test_get_or_add_and_evict_with_decay() {
        let saver = SpaceSaver::new(2, 3600);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        saver
            .update(
                ip1,
                IpState {
                    count: 10,
                    ..Default::default()
                },
            )
            .await;
        saver
            .update(
                ip2,
                IpState {
                    count: 5,
                    ..Default::default()
                },
            )
            .await;

        // Manually make ip2 very old so it decays to 0
        {
            let mut state = saver.state.lock().unwrap();
            let mut ip2_state = state.lookup.get(&ip2).unwrap().clone();
            state.times.remove(&(ip2_state.last_seen, ip2));
            ip2_state.last_seen = SpaceSaver::now() - 36000; // 10 half-lives
            state.times.insert((ip2_state.last_seen, ip2));
            state.lookup.insert(ip2, ip2_state);
        }

        // Now getting ip3 should trigger decay of ip2 to 0 (so it gets pruned)
        let state3 = saver.get_or_add_and_evict(ip3).await;
        assert_eq!(state3.count, 1);
        saver.update(ip3, state3).await;

        let state = saver.state.lock().unwrap();
        assert!(!state.lookup.contains_key(&ip2));
        assert!(state.lookup.contains_key(&ip1));
        assert!(state.lookup.contains_key(&ip3));
    }
}
