//! Bounded-memory tracker for IP states.

use crate::clock::{Clock, SystemClock};
use crate::sync::lock;
use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

/// State for a single IP address.
#[derive(Debug, Clone, Copy, Default)]
pub struct IpState {
    /// Timestamp (in seconds) when this IP was last seen.
    pub last_seen: u64,
    /// Timestamp (in milliseconds) of this IP's last request, used for the velocity EWMA.
    ///
    /// Kept separately from `last_seen` because velocity ticks can be shorter than a
    /// second, while the `SpaceSaver` count decay works in whole seconds.
    pub last_seen_ms: u64,
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

/// `SpaceSaver` tracks recent IPs in strictly bounded memory, evicting the lowest-volume
/// "mouse" IPs when full.
#[derive(Debug)]
pub struct SpaceSaver {
    capacity: usize,
    half_life_seconds: u64, // How long it takes for an IP's count to halve
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl SpaceSaver {
    /// Initializes a new `SpaceSaver` with a strict maximum memory capacity, reading time
    /// from the system clock.
    pub fn new(capacity: usize, half_life_seconds: u64) -> Self {
        Self::with_clock(capacity, half_life_seconds, SystemClock::shared())
    }

    /// Like [`new`](Self::new), but reads time from `clock`.
    pub fn with_clock(capacity: usize, half_life_seconds: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            capacity,
            half_life_seconds,
            clock,
            state: Mutex::new(State {
                lookup: HashMap::with_capacity(capacity),
                counts: BTreeSet::new(),
                times: BTreeSet::new(),
            }),
        }
    }

    /// Replaces the clock this tracker reads time from.
    pub(crate) fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Helper to get the current time in seconds
    fn now(&self) -> u64 {
        self.clock.now_millis() / 1000
    }

    /// `count` after `elapsed` seconds of half-life decay, rounded to the nearest integer.
    ///
    /// A count that has sat for at least one full half-life always drops by at least 1, so
    /// small counts cannot get stuck by rounding. A `half_life` of 0 disables decay.
    // Counts and timestamps are far below 2^52, so the float round trip is exact enough.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn decayed_count(count: usize, elapsed: u64, half_life: u64) -> usize {
        if half_life == 0 || elapsed == 0 {
            return count;
        }
        let half_lives = elapsed as f64 / half_life as f64;
        let decayed = ((count as f64) * (-half_lives).exp2()).round() as usize;
        if decayed == count && elapsed >= half_life && decayed > 0 {
            decayed - 1
        } else {
            decayed
        }
    }

    /// Decays the counts of old IPs and removes those that drop to 0.
    fn decay_and_prune(state: &mut State, half_life_seconds: u64, now: u64) {
        if half_life_seconds == 0 {
            return;
        }

        // Nothing can be a full half-life old yet. Without this guard a saturated
        // threshold of 0 would "decay" entries seen at time 0 by a factor of 1 and
        // reinsert them at time 0, forever.
        let Some(threshold) = now.checked_sub(half_life_seconds) else {
            return;
        };

        // Repeatedly check the oldest item
        while let Some(&(oldest_time, oldest_ip)) = state.times.first() {
            if oldest_time <= threshold {
                if let Some(mut removed_state) = state.lookup.remove(&oldest_ip) {
                    state.counts.remove(&(removed_state.count, oldest_ip));
                    state.times.pop_first();

                    let new_count = Self::decayed_count(
                        removed_state.count,
                        now.saturating_sub(removed_state.last_seen),
                        half_life_seconds,
                    );

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

    /// The number of IPs currently tracked (at most the capacity).
    pub fn len(&self) -> usize {
        lock(&self.state).lookup.len()
    }

    /// Whether no IP is tracked yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fetches the state for `client_ip`. If it doesn't exist, adds it.
    /// If memory is full, evicts the lowest-volume IP.
    pub fn get_or_add_and_evict(&self, client_ip: IpAddr) -> IpState {
        let now = self.now();
        self.get_or_add_locked(&mut lock(&self.state), client_ip, now)
    }

    /// [`get_or_add_and_evict`](Self::get_or_add_and_evict) with the state already locked.
    fn get_or_add_locked(&self, state: &mut State, client_ip: IpAddr, now: u64) -> IpState {
        // 1. Clean up entries by decaying counts and removing zeros
        Self::decay_and_prune(state, self.half_life_seconds, now);

        // Case 1: The IP is already being tracked.
        if let Some(mut existing_state) = state.lookup.get(&client_ip).copied() {
            existing_state.count = Self::decayed_count(
                existing_state.count,
                now.saturating_sub(existing_state.last_seen),
                self.half_life_seconds,
            );
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
        if let Some(min_state) = state
            .counts
            .first()
            .and_then(|(_, min_ip)| state.lookup.get(min_ip))
        {
            let decayed_min_count = Self::decayed_count(
                min_state.count,
                now.saturating_sub(min_state.last_seen),
                self.half_life_seconds,
            );
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
    pub fn update(&self, client_ip: IpAddr, mut new_ip_state: IpState) {
        // Ensure the last_seen time is updated
        new_ip_state.last_seen = self.now();
        Self::update_locked(
            &mut lock(&self.state),
            self.capacity,
            client_ip,
            new_ip_state,
        );
    }

    /// [`update`](Self::update) with the state already locked.
    fn update_locked(state: &mut State, capacity: usize, client_ip: IpAddr, new_ip_state: IpState) {
        let old_state_opt = state.lookup.get(&client_ip).copied();

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
        if state.lookup.len() < capacity {
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
    // Tests hold the state lock across their assertions on purpose.
    #![allow(clippy::significant_drop_tightening)]

    use super::*;
    use crate::clock::ManualClock;
    use std::time::Duration;

    /// A saver on a manual clock that starts at `start_secs`.
    fn saver_on_manual_clock(
        capacity: usize,
        half_life_seconds: u64,
        start_secs: u64,
    ) -> (SpaceSaver, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(start_secs * 1000));
        let saver = SpaceSaver::with_clock(capacity, half_life_seconds, clock.clone());
        (saver, clock)
    }

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

    #[test]
    fn test_space_saver_basic_add_and_update() {
        let saver = SpaceSaver::new(10, 3600);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        let mut state = saver.get_or_add_and_evict(ip);
        assert_eq!(state.count, 1);

        state.count = 5;
        state.velocity = 100;
        saver.update(ip, state);

        let new_state = saver.get_or_add_and_evict(ip);
        assert_eq!(new_state.count, 5);
        assert_eq!(new_state.velocity, 100);
    }

    #[test]
    fn test_space_saver_eviction() {
        let saver = SpaceSaver::new(2, 3600);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        let mut state1 = saver.get_or_add_and_evict(ip1);
        state1.count = 10;
        saver.update(ip1, state1);

        let mut state2 = saver.get_or_add_and_evict(ip2);
        state2.count = 5;
        saver.update(ip2, state2);

        // This should evict ip2 because it has the lowest count (5) compared to ip1 (10)
        let state3 = saver.get_or_add_and_evict(ip3);
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

    #[test]
    fn decay_and_prune_before_first_half_life_terminates() {
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let mut state = State {
            lookup: HashMap::from([(
                ip,
                IpState {
                    count: 3,
                    last_seen: 0,
                    ..Default::default()
                },
            )]),
            counts: BTreeSet::from([(3, ip)]),
            times: BTreeSet::from([(0, ip)]),
        };

        // `now` is less than one half-life after the epoch: nothing may decay.
        SpaceSaver::decay_and_prune(&mut state, 200, 100);
        assert_eq!(state.lookup[&ip].count, 3);
    }

    #[test]
    fn test_update_untracked_when_full() {
        let saver = SpaceSaver::new(2, 3600);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        saver.update(
            ip1,
            IpState {
                count: 10,
                ..Default::default()
            },
        );
        saver.update(
            ip2,
            IpState {
                count: 5,
                ..Default::default()
            },
        );

        // Memory is full. Update an untracked IP.
        saver.update(
            ip3,
            IpState {
                count: 20,
                ..Default::default()
            },
        );

        let state = saver.state.lock().unwrap();
        // ip2 should be evicted since it has the lowest count (5)
        assert!(state.lookup.contains_key(&ip1));
        assert!(!state.lookup.contains_key(&ip2));
        assert!(state.lookup.contains_key(&ip3));
        assert_eq!(state.lookup.get(&ip3).unwrap().count, 20);
    }

    #[test]
    fn test_update_existing_updates_indices() {
        let saver = SpaceSaver::new(2, 3600);
        let ip1: IpAddr = "10.0.0.1".parse().unwrap();

        saver.update(
            ip1,
            IpState {
                count: 10,
                ..Default::default()
            },
        );

        {
            let state = saver.state.lock().unwrap();
            assert_eq!(state.counts.len(), 1);
            assert_eq!(state.times.len(), 1);
            assert!(state.counts.contains(&(10, ip1)));
        }

        // Update the count of the existing IP
        saver.update(
            ip1,
            IpState {
                count: 20,
                ..Default::default()
            },
        );

        {
            let state = saver.state.lock().unwrap();
            assert_eq!(state.counts.len(), 1);
            assert_eq!(state.times.len(), 1);
            assert!(state.counts.contains(&(20, ip1)));
            assert!(!state.counts.contains(&(10, ip1)));
        }
    }

    #[test]
    fn test_get_or_add_and_evict_with_decay() {
        let (saver, clock) = saver_on_manual_clock(2, 3600, 100_000);

        let ip1: IpAddr = "10.0.0.1".parse().unwrap();
        let ip2: IpAddr = "10.0.0.2".parse().unwrap();
        let ip3: IpAddr = "10.0.0.3".parse().unwrap();

        saver.update(
            ip2,
            IpState {
                count: 5,
                ..Default::default()
            },
        );

        // 10 half-lives later ip2 has decayed to 0, while ip1 is fresh.
        clock.advance(Duration::from_hours(10));
        saver.update(
            ip1,
            IpState {
                count: 10,
                ..Default::default()
            },
        );

        // Now getting ip3 should trigger decay of ip2 to 0 (so it gets pruned)
        let state3 = saver.get_or_add_and_evict(ip3);
        assert_eq!(state3.count, 1);
        saver.update(ip3, state3);

        let state = saver.state.lock().unwrap();
        assert!(!state.lookup.contains_key(&ip2));
        assert!(state.lookup.contains_key(&ip1));
        assert!(state.lookup.contains_key(&ip3));
    }

    #[test]
    fn count_halves_per_half_life_on_the_injected_clock() {
        let (saver, clock) = saver_on_manual_clock(10, 200, 1_000);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();

        saver.update(
            ip,
            IpState {
                count: 8,
                ..Default::default()
            },
        );
        assert_eq!(saver.get_or_add_and_evict(ip).count, 8);

        // 8 * 2^-2 = 2 after two half-lives.
        clock.advance(Duration::from_secs(400));
        assert_eq!(saver.get_or_add_and_evict(ip).count, 2);
    }

    #[test]
    fn count_decayed_to_zero_is_pruned() {
        let (saver, clock) = saver_on_manual_clock(10, 200, 1_000);
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "10.0.0.2".parse().unwrap();

        saver.update(
            ip,
            IpState {
                count: 1,
                ..Default::default()
            },
        );

        // After one half-life a count of 1 must strictly decrease, to 0.
        clock.advance(Duration::from_secs(200));
        let _ = saver.get_or_add_and_evict(other);

        let state = saver.state.lock().unwrap();
        assert!(!state.lookup.contains_key(&ip));
        assert!(state.counts.is_empty());
        assert!(state.times.is_empty());
    }
}
