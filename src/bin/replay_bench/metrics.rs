//! Counters collected during a replay and the final report.

use crate::Algorithm;
use prometheus::{Encoder, TextEncoder};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

/// Everything a replay measured, as printed and as written to `--json-out`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub elapsed_s: f64,
    pub throughput_rps: f64,
    pub avg_access_time_us: f64,
    pub human: ClassReport,
    pub bot: ClassReport,
    pub overall: ClassReport,
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
}
