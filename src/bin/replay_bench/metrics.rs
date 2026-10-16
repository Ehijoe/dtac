//! Counters collected during a replay and the final report.

use crate::Algorithm;
use crate::classifier::{ClassifierReport, Sample};
use hdrhistogram::Histogram;
use prometheus::{Encoder, TextEncoder};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Number of independently locked copies [`Striped`] keeps.
const STRIPES: usize = 32;

/// Locks `mutex`, recovering the guard if a replay task panicked while holding it.
pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// [`STRIPES`] separately locked copies of `T`. Each task records into the copy picked
/// by its record's sequence number, so recording rarely contends; the copies are merged
/// once the replay is done.
pub struct Striped<T> {
    stripes: Vec<Mutex<T>>,
}

impl<T: Default> Default for Striped<T> {
    fn default() -> Self {
        Self {
            stripes: (0..STRIPES).map(|_| Mutex::default()).collect(),
        }
    }
}

impl<T> Striped<T> {
    /// Runs `f` on the copy for sequence number `seq`.
    pub fn with<R>(&self, seq: usize, f: impl FnOnce(&mut T) -> R) -> R {
        f(&mut lock(&self.stripes[seq % self.stripes.len()]))
    }

    /// Runs `f` on every copy in turn.
    pub fn for_each(&self, mut f: impl FnMut(&T)) {
        for stripe in &self.stripes {
            f(&lock(stripe));
        }
    }
}

/// Latency histograms in nanoseconds for one stripe.
pub struct PhaseHistograms {
    /// `T_proxy`: time in the router. Empty for engines without one.
    pub proxy: Histogram<u64>,
    /// `T_cache`: time in the cache engine, including the (instant) origin fetch.
    pub cache: Histogram<u64>,
    /// The whole access as the bench sees it.
    pub total: Histogram<u64>,
}

/// A histogram from 1 ns to 60 s at 2 significant digits (1% resolution), which keeps
/// each one small enough to stripe.
fn latency_histogram() -> Histogram<u64> {
    // Fixed, valid bounds: `new_with_bounds` only fails for low < 1, high < 2 * low or
    // more than 5 significant digits.
    #[allow(clippy::expect_used)]
    Histogram::new_with_bounds(1, 60_000_000_000, 2).expect("valid histogram bounds")
}

impl Default for PhaseHistograms {
    fn default() -> Self {
        Self {
            proxy: latency_histogram(),
            cache: latency_histogram(),
            total: latency_histogram(),
        }
    }
}

/// Saturates at `u64::MAX` ns (about 584 years).
fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl PhaseHistograms {
    /// Records one access. `proxy` is `None` for engines without a router.
    pub fn record(&mut self, proxy: Option<Duration>, cache: Duration, total: Duration) {
        if let Some(proxy) = proxy {
            self.proxy.saturating_record(nanos(proxy));
        }
        self.cache.saturating_record(nanos(cache));
        self.total.saturating_record(nanos(total));
    }

    /// Adds `other`'s counts into `self`. Both use the same bounds, so this can't fail.
    pub fn merge(&mut self, other: &Self) {
        for (into, from) in [
            (&mut self.proxy, &other.proxy),
            (&mut self.cache, &other.cache),
            (&mut self.total, &other.total),
        ] {
            let _ = into.add(from);
        }
    }
}

/// Measurement-phase counters, shared by every replay task.
#[derive(Default)]
pub struct BenchMetrics {
    // Total counters
    pub total_requests: AtomicU64,
    pub cache_miss_fetches: AtomicU64, // Increment when fetch_from_origin is called

    // Disaggregated human vs bot counters
    pub human_requests: AtomicU64,
    pub human_misses: AtomicU64,
    pub bot_requests: AtomicU64,
    pub bot_misses: AtomicU64,

    // Bytes requested and bytes fetched from origin, for the byte hit rate
    pub total_bytes: AtomicU64,
    pub total_miss_bytes: AtomicU64,
    pub human_bytes: AtomicU64,
    pub human_miss_bytes: AtomicU64,

    // Timing
    pub total_duration_us: AtomicU64,
    pub latency: Striped<PhaseHistograms>,

    // Router scores against ground truth (DTAC only)
    pub classifier: Striped<Vec<Sample>>,
}

impl BenchMetrics {
    /// Counts one measurement request for an item of `size` bytes.
    pub fn record_request(&self, is_bot: bool, size: u64) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_add(size, Ordering::Relaxed);
        if is_bot {
            self.bot_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            self.human_requests.fetch_add(1, Ordering::Relaxed);
            self.human_bytes.fetch_add(size, Ordering::Relaxed);
        }
    }

    /// Counts one origin fetch of `size` bytes for a measurement request.
    pub fn record_miss(&self, is_bot: bool, size: u64) {
        self.cache_miss_fetches.fetch_add(1, Ordering::Relaxed);
        self.total_miss_bytes.fetch_add(size, Ordering::Relaxed);
        if is_bot {
            self.bot_misses.fetch_add(1, Ordering::Relaxed);
        } else {
            self.human_misses.fetch_add(1, Ordering::Relaxed);
            self.human_miss_bytes.fetch_add(size, Ordering::Relaxed);
        }
    }
}

/// Routes the library's OpenTelemetry metrics into a fresh Prometheus registry.
pub fn init_metrics() -> Result<prometheus::Registry, Box<dyn std::error::Error>> {
    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(exporter)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider);
    Ok(registry)
}

/// `part / whole`, or 0 when `whole` is 0.
// Request counts and byte totals are far below 2^52, so f64 is exact here.
#[allow(clippy::cast_precision_loss)]
pub fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// Request outcomes for one traffic class (human, bot or all).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ClassReport {
    pub requests: u64,
    pub hits: u64,
    pub misses: u64,
    /// `hits / requests`, in `[0, 1]`.
    pub hit_rate: f64,
    /// Bytes requested.
    pub bytes: u64,
    /// Bytes fetched from origin.
    pub miss_bytes: u64,
    /// Byte hit rate, `1 - miss_bytes / bytes`. Capacity counts entries, not bytes, and no
    /// engine is size-aware, so this only reflects how hits happen to correlate with size.
    pub byte_hit_rate: f64,
}

impl ClassReport {
    pub fn new(requests: u64, misses: u64, bytes: u64, miss_bytes: u64) -> Self {
        let hits = requests.saturating_sub(misses);
        Self {
            requests,
            hits,
            misses,
            hit_rate: ratio(hits, requests),
            bytes,
            miss_bytes,
            byte_hit_rate: if bytes == 0 {
                0.0
            } else {
                1.0 - ratio(miss_bytes, bytes)
            },
        }
    }
}

/// Latency percentiles of one phase, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Percentiles {
    pub count: u64,
    pub mean_us: f64,
    pub p50_us: f64,
    pub p90_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
}

impl Percentiles {
    /// Summarizes a nanosecond histogram, or `None` if it is empty.
    // Nanosecond latencies are far below 2^52, so f64 is exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn of(h: &Histogram<u64>) -> Option<Self> {
        if h.is_empty() {
            return None;
        }
        let us = |ns: u64| ns as f64 / 1_000.0;
        Some(Self {
            count: h.len(),
            mean_us: h.mean() / 1_000.0,
            p50_us: us(h.value_at_quantile(0.50)),
            p90_us: us(h.value_at_quantile(0.90)),
            p99_us: us(h.value_at_quantile(0.99)),
            max_us: us(h.max()),
        })
    }
}

/// Per-phase latency: `T_proxy` (router), `T_cache` (engine) and the whole access.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct LatencyReport {
    /// `None` for the baselines, which have no router.
    pub proxy: Option<Percentiles>,
    pub cache: Option<Percentiles>,
    pub total: Option<Percentiles>,
}

impl LatencyReport {
    /// Merges every stripe's histograms and summarizes them.
    pub fn new(latency: &Striped<PhaseHistograms>) -> Self {
        let mut merged = PhaseHistograms::default();
        latency.for_each(|stripe| merged.merge(stripe));
        Self {
            proxy: Percentiles::of(&merged.proxy),
            cache: Percentiles::of(&merged.cache),
            total: Percentiles::of(&merged.total),
        }
    }

    fn print(&self) {
        println!("LATENCY (µs)            p50       p90       p99       max");
        for (label, phase) in [
            ("T_proxy (router)", self.proxy),
            ("T_cache (engine)", self.cache),
            ("Total access", self.total),
        ] {
            if let Some(p) = phase {
                println!(
                    "{label:<18} {:>9.2} {:>9.2} {:>9.2} {:>9.2}",
                    p.p50_us, p.p90_us, p.p99_us, p.max_us
                );
            }
        }
    }
}

/// Everything a replay measured, as printed and as written to `--json-out`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub elapsed_s: f64,
    pub throughput_rps: f64,
    pub avg_access_time_us: f64,
    pub human: ClassReport,
    pub bot: ClassReport,
    pub overall: ClassReport,
    pub latency: LatencyReport,
    /// `None` for the baselines, which don't score requests.
    pub classifier: Option<ClassifierReport>,
}

impl Report {
    /// Summarizes the measurement-phase counters.
    // Microsecond totals are far below 2^52, so f64 is exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn new(m: &BenchMetrics, elapsed: Duration) -> Self {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let total_requests = load(&m.total_requests);
        Self {
            elapsed_s: elapsed.as_secs_f64(),
            throughput_rps: total_requests as f64 / elapsed.as_secs_f64(),
            avg_access_time_us: ratio(load(&m.total_duration_us), total_requests),
            human: ClassReport::new(
                load(&m.human_requests),
                load(&m.human_misses),
                load(&m.human_bytes),
                load(&m.human_miss_bytes),
            ),
            bot: ClassReport::new(
                load(&m.bot_requests),
                load(&m.bot_misses),
                load(&m.total_bytes).saturating_sub(load(&m.human_bytes)),
                load(&m.total_miss_bytes).saturating_sub(load(&m.human_miss_bytes)),
            ),
            overall: ClassReport::new(
                total_requests,
                load(&m.cache_miss_fetches),
                load(&m.total_bytes),
                load(&m.total_miss_bytes),
            ),
            latency: LatencyReport::new(&m.latency),
            classifier: {
                let mut samples = Vec::new();
                m.classifier
                    .for_each(|stripe| samples.extend_from_slice(stripe));
                ClassifierReport::new(&samples)
            },
        }
    }

    /// Prints the human-readable report.
    pub fn print(&self, algorithm: Algorithm) {
        let pct = |rate: f64| rate * 100.0;
        let report_label = match algorithm {
            Algorithm::Dtac => "DTAC",
            Algorithm::Lru => "STANDARD LRU (BASELINE)",
        };
        let (human, bot, overall) = (&self.human, &self.bot, &self.overall);
        println!("\n========================================================");
        println!("          {report_label} DIRECT BENCHMARK REPORT          ");
        println!("          (post-warm-up, steady-state measurement)        ");
        println!("========================================================");
        println!(
            "Elapsed Time:           {:.3?}",
            Duration::from_secs_f64(self.elapsed_s)
        );
        println!("Throughput:             {:.2} ops/sec", self.throughput_rps);
        println!("Average Access Time:    {:.3} µs", self.avg_access_time_us);
        println!("--------------------------------------------------------");
        println!("PRIMARY METRIC");
        println!("Human Requests:         {}", human.requests);
        println!(
            "Human Cache Hit Rate:   {:.2}% ({} hits / {} misses)",
            pct(human.hit_rate),
            human.hits,
            human.misses
        );
        println!("Human Byte Hit Rate:    {:.2}%", pct(human.byte_hit_rate));
        println!("--------------------------------------------------------");
        println!("SECONDARY METRIC");
        println!("Total Requests:         {}", overall.requests);
        println!(
            "Overall Cache Hit Rate: {} ({:.2}%)",
            overall.hits,
            pct(overall.hit_rate)
        );
        println!(
            "Total Cache Misses:     {} ({:.2}%)",
            overall.misses,
            100.0 - pct(overall.hit_rate)
        );
        println!("Overall Byte Hit Rate:  {:.2}%", pct(overall.byte_hit_rate));
        println!("--------------------------------------------------------");
        println!("FOR REFERENCE ONLY -- not an optimization target");
        println!("Bot Requests:           {}", bot.requests);
        println!(
            "Bot Cache Hit Rate:     {:.2}% ({} hits / {} misses)",
            pct(bot.hit_rate),
            bot.hits,
            bot.misses
        );
        println!("Bot Byte Hit Rate:      {:.2}%", pct(bot.byte_hit_rate));
        println!("(byte hit rates: capacity counts entries and no engine is size-aware)");
        println!("--------------------------------------------------------");
        self.latency.print();
        if let Some(classifier) = &self.classifier {
            println!("--------------------------------------------------------");
            classifier.print();
        }
        println!("========================================================");
    }
}

/// Prints the OpenTelemetry metrics the library recorded, as Prometheus text.
pub fn print_prometheus(registry: &prometheus::Registry) {
    println!("\n--- Raw Prometheus Metrics Registry Output ---");
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    let metric_families = registry.gather();
    if encoder.encode(&metric_families, &mut buffer).is_ok()
        && let Ok(text) = String::from_utf8(buffer)
    {
        println!("{text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_report_derives_hits_and_rate() {
        let r = ClassReport::new(10, 4, 0, 0);
        assert_eq!((r.hits, r.misses), (6, 4));
        assert!((r.hit_rate - 0.6).abs() < 1e-12);
        assert!(ClassReport::new(0, 0, 0, 0).hit_rate.abs() < f64::EPSILON);
    }

    #[test]
    fn byte_hit_rate_weights_by_size() {
        // Two hits on 100-byte items, one miss on a 1000-byte item: 2/3 of requests hit,
        // but only 200 of 1200 bytes did.
        let m = BenchMetrics::default();
        m.record_request(false, 100);
        m.record_request(false, 100);
        m.record_request(true, 1000);
        m.record_miss(true, 1000);
        let report = Report::new(&m, Duration::from_secs(1));
        assert!((report.overall.hit_rate - 2.0 / 3.0).abs() < 1e-12);
        assert!((report.overall.byte_hit_rate - 200.0 / 1200.0).abs() < 1e-12);
        assert!((report.human.byte_hit_rate - 1.0).abs() < 1e-12);
        assert!(report.bot.byte_hit_rate.abs() < 1e-12);
        assert_eq!(report.bot.bytes, 1000);
    }

    #[test]
    fn percentiles_come_from_the_merged_stripes() {
        let latency = Striped::<PhaseHistograms>::default();
        // 1..=100 µs spread across the stripes, with no router phase (like the baselines).
        for (seq, us) in (1..=100_u64).enumerate() {
            let d = Duration::from_micros(us);
            latency.with(seq, |h| h.record(None, d, d));
        }
        let report = LatencyReport::new(&latency);
        assert_eq!(report.proxy, None);
        let total = report.total.unwrap();
        assert_eq!(total.count, 100);
        // 2 significant digits: within 1% of the exact value.
        for (got, want) in [
            (total.p50_us, 50.0),
            (total.p99_us, 99.0),
            (total.max_us, 100.0),
        ] {
            assert!((got / want - 1.0).abs() <= 0.01, "{got} vs {want}");
        }
        assert!((total.mean_us - 50.5).abs() < 0.6);
    }
}
