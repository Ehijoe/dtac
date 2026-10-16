//! Counters collected during a replay and the final report.

use crate::Algorithm;
use prometheus::{Encoder, TextEncoder};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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

    // Timing
    pub total_duration_us: AtomicU64,
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
}

impl ClassReport {
    pub fn new(requests: u64, misses: u64) -> Self {
        let hits = requests.saturating_sub(misses);
        Self {
            requests,
            hits,
            misses,
            hit_rate: ratio(hits, requests),
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
            human: ClassReport::new(load(&m.human_requests), load(&m.human_misses)),
            bot: ClassReport::new(load(&m.bot_requests), load(&m.bot_misses)),
            overall: ClassReport::new(total_requests, load(&m.cache_miss_fetches)),
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
        println!("--------------------------------------------------------");
        println!("FOR REFERENCE ONLY -- not an optimization target");
        println!("Bot Requests:           {}", bot.requests);
        println!(
            "Bot Cache Hit Rate:     {:.2}% ({} hits / {} misses)",
            pct(bot.hit_rate),
            bot.hits,
            bot.misses
        );
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
        let r = ClassReport::new(10, 4);
        assert_eq!((r.hits, r.misses), (6, 4));
        assert!((r.hit_rate - 0.6).abs() < 1e-12);
        assert!(ClassReport::new(0, 0).hit_rate.abs() < f64::EPSILON);
    }
}
