//! Eviction-by-origin counts and the Cache Pollution Index (CPI), from the log's
//! ground-truth labels. Engine-agnostic: it only needs every origin fetch, every human
//! request, every main-cache eviction and, now and then, the resident keys.
//!
//! The label of a key's most recent fetch identifies the request that *triggered* its
//! admission for every engine (for LRU a fetch is the admission; for DTAC the promotion
//! fetch is the last one before the key enters the main cache, and hits never fetch). For
//! DTAC that request isn't necessarily responsible for the admission, though: humans may
//! have built up most of the score and a bot tipped it over the threshold. So the CPI
//! counts entries no human has requested since they were admitted, not bot-triggered ones.

use crate::metrics::{lock, ratio};
use serde::Serialize;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const LEDGER_SHARDS: usize = 64;

/// Who sent the request that triggered a key's most recent fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Human,
    Bot,
}

#[derive(Debug, Clone, Copy)]
struct KeyState {
    admitted_by: Origin,
    /// Whether any human requested the key since its most recent fetch.
    human_since_admit: bool,
    size: u64,
}

/// Pollution of the resident set at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct CpiSample {
    pub resident: u64,
    /// Share of resident keys no human has requested since they were admitted.
    pub cpi: f64,
    /// Share of resident keys whose admission a bot request triggered.
    pub bot_triggered_share: f64,
    /// Sum of `size_bytes` over the resident keys.
    pub resident_bytes: u64,
}

/// Per-key admission bookkeeping, shared by every replay task.
pub struct Ledger {
    shards: Vec<Mutex<HashMap<String, KeyState>>>,
    /// Evictions are only counted once the measurement phase starts.
    measuring: AtomicBool,
    evictions_human_admitted: AtomicU64,
    evictions_bot_admitted: AtomicU64,
    samples: Mutex<Vec<CpiSample>>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            shards: (0..LEDGER_SHARDS).map(|_| Mutex::default()).collect(),
            measuring: AtomicBool::new(false),
            evictions_human_admitted: AtomicU64::new(0),
            evictions_bot_admitted: AtomicU64::new(0),
            samples: Mutex::default(),
        }
    }
}

impl Ledger {
    // Only the hash's low bits matter for picking a shard.
    #[allow(clippy::cast_possible_truncation)]
    fn shard(&self, key: &str) -> &Mutex<HashMap<String, KeyState>> {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        &self.shards[(hasher.finish() as usize) % self.shards.len()]
    }

    /// Starts counting evictions.
    pub fn start_measuring(&self) {
        self.measuring.store(true, Ordering::Relaxed);
    }

    /// Records an origin fetch of `key` for a request from `origin`.
    pub fn on_fetch(&self, key: &str, origin: Origin, size: u64) {
        lock(self.shard(key)).insert(
            key.to_string(),
            KeyState {
                admitted_by: origin,
                human_since_admit: origin == Origin::Human,
                size,
            },
        );
    }

    /// Records that a human requested `key` (hit or miss).
    pub fn on_human_request(&self, key: &str) {
        if let Some(state) = lock(self.shard(key)).get_mut(key) {
            state.human_since_admit = true;
        }
    }

    /// Records a main-cache eviction of `key`, attributed to whoever triggered its
    /// admission.
    pub fn on_evict(&self, key: &str) {
        let Some(state) = lock(self.shard(key)).remove(key) else {
            return;
        };
        if self.measuring.load(Ordering::Relaxed) {
            match state.admitted_by {
                Origin::Human => &self.evictions_human_admitted,
                Origin::Bot => &self.evictions_bot_admitted,
            }
            .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Computes the pollution of `resident` keys without recording it.
    // Counts are far below 2^52, so f64 is exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn measure(&self, resident: &[String]) -> CpiSample {
        let (mut polluted, mut bot_triggered, mut resident_bytes) = (0, 0, 0);
        for key in resident {
            // A resident key the ledger has never seen was fetched before tracking began;
            // it can't be attributed, so it counts as neither.
            if let Some(state) = lock(self.shard(key)).get(key) {
                polluted += u64::from(!state.human_since_admit);
                bot_triggered += u64::from(state.admitted_by == Origin::Bot);
                resident_bytes += state.size;
            }
        }
        let n = resident.len() as u64;
        CpiSample {
            resident: n,
            cpi: ratio(polluted, n),
            bot_triggered_share: ratio(bot_triggered, n),
            resident_bytes,
        }
    }

    /// Measures and records one CPI sample.
    pub fn sample(&self, resident: &[String]) -> CpiSample {
        let sample = self.measure(resident);
        lock(&self.samples).push(sample);
        sample
    }
}

/// Eviction-by-origin counts and CPI over the measurement phase.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PollutionReport {
    /// Main-cache evictions of entries a human request had admitted.
    pub evictions_human_admitted: u64,
    /// Main-cache evictions of entries a bot request had admitted.
    pub evictions_bot_admitted: u64,
    pub cpi_samples: usize,
    /// CPI (share of resident entries no human has requested since admission), averaged
    /// over the samples taken every `--cpi-sample-every` requests and at the end.
    pub cpi_mean: f64,
    pub cpi_final: f64,
    /// Share of resident entries whose admission a bot triggered (the literal thesis
    /// §3.7.2 definition; penalizes LRU whenever bots happen to fetch popular items).
    pub bot_triggered_share_mean: f64,
    pub bot_triggered_share_final: f64,
    /// Logical bytes resident at the end (sum of `size_bytes`).
    pub resident_bytes_final: u64,
}

impl PollutionReport {
    /// Summarizes the ledger, or `None` if no CPI sample was taken.
    // Sample counts are tiny, so f64 is exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn new(ledger: &Ledger) -> Option<Self> {
        let samples = lock(&ledger.samples).clone();
        let last = *samples.last()?;
        let mean =
            |f: fn(&CpiSample) -> f64| samples.iter().map(f).sum::<f64>() / samples.len() as f64;
        Some(Self {
            evictions_human_admitted: ledger.evictions_human_admitted.load(Ordering::Relaxed),
            evictions_bot_admitted: ledger.evictions_bot_admitted.load(Ordering::Relaxed),
            cpi_samples: samples.len(),
            cpi_mean: mean(|s| s.cpi),
            cpi_final: last.cpi,
            bot_triggered_share_mean: mean(|s| s.bot_triggered_share),
            bot_triggered_share_final: last.bot_triggered_share,
            resident_bytes_final: last.resident_bytes,
        })
    }

    pub fn print(&self) {
        let pct = |rate: f64| rate * 100.0;
        println!("FOR REFERENCE -- cache pollution (ground-truth labels)");
        println!(
            "Evictions by admitter:  {} human-admitted, {} bot-admitted",
            self.evictions_human_admitted, self.evictions_bot_admitted
        );
        println!(
            "Cache Pollution Index:  mean {:.2}%, final {:.2}% ({} samples)",
            pct(self.cpi_mean),
            pct(self.cpi_final),
            self.cpi_samples
        );
        println!(
            "Bot-triggered share:    mean {:.2}%, final {:.2}%",
            pct(self.bot_triggered_share_mean),
            pct(self.bot_triggered_share_final)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn bot_admitted_key_a_human_later_hits_is_not_pollution() {
        let ledger = Ledger::default();
        ledger.on_fetch("human", Origin::Human, 10);
        ledger.on_fetch("bot", Origin::Bot, 20);
        ledger.on_fetch("bot-then-human", Origin::Bot, 30);
        ledger.on_human_request("bot-then-human");

        let s = ledger.measure(&keys(&["human", "bot", "bot-then-human"]));
        assert_eq!(s.resident, 3);
        assert!((s.cpi - 1.0 / 3.0).abs() < 1e-12);
        assert!((s.bot_triggered_share - 2.0 / 3.0).abs() < 1e-12);
        assert_eq!(s.resident_bytes, 60);
    }

    #[test]
    fn refetch_resets_the_human_flag() {
        let ledger = Ledger::default();
        ledger.on_fetch("k", Origin::Bot, 1);
        ledger.on_human_request("k");
        ledger.on_fetch("k", Origin::Bot, 1); // evicted and re-admitted by a bot
        assert!((ledger.measure(&keys(&["k"])).cpi - 1.0).abs() < 1e-12);
    }

    #[test]
    fn evictions_count_by_admitter_only_while_measuring() {
        let ledger = Ledger::default();
        ledger.on_fetch("warm", Origin::Human, 1);
        ledger.on_evict("warm");
        ledger.start_measuring();
        ledger.on_fetch("a", Origin::Human, 1);
        ledger.on_fetch("b", Origin::Bot, 1);
        ledger.on_fetch("c", Origin::Bot, 1);
        for key in ["a", "b", "c", "never-fetched"] {
            ledger.on_evict(key);
        }
        ledger.sample(&[]);
        let r = PollutionReport::new(&ledger).unwrap();
        assert_eq!(
            (r.evictions_human_admitted, r.evictions_bot_admitted),
            (1, 2)
        );
    }

    #[test]
    fn report_averages_samples() {
        let ledger = Ledger::default();
        assert_eq!(PollutionReport::new(&ledger), None);
        ledger.on_fetch("h", Origin::Human, 1);
        ledger.on_fetch("b", Origin::Bot, 1);
        ledger.sample(&keys(&["h", "b"])); // CPI 0.5
        ledger.sample(&keys(&["b"])); // CPI 1.0
        let r = PollutionReport::new(&ledger).unwrap();
        assert!((r.cpi_mean - 0.75).abs() < 1e-12);
        assert!((r.cpi_final - 1.0).abs() < 1e-12);
        assert_eq!(r.cpi_samples, 2);
    }
}
