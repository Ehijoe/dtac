//! Counters collected during a replay and the final report.

use crate::Algorithm;
use prometheus::{Encoder, TextEncoder};
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

// Request counts and microsecond totals are far below 2^52, so f64 is exact here.
#[allow(clippy::cast_precision_loss)]
pub fn print_results(
    m: &BenchMetrics,
    total_time: Duration,
    registry: &prometheus::Registry,
    algorithm: Algorithm,
) {
    let total_reqs = m.total_requests.load(Ordering::Relaxed);
    let total_misses = m.cache_miss_fetches.load(Ordering::Relaxed);
    let total_hits = total_reqs.saturating_sub(total_misses);

    let human_reqs = m.human_requests.load(Ordering::Relaxed);
    let human_misses = m.human_misses.load(Ordering::Relaxed);
    let human_hits = human_reqs.saturating_sub(human_misses);

    let bot_reqs = m.bot_requests.load(Ordering::Relaxed);
    let bot_misses = m.bot_misses.load(Ordering::Relaxed);
    let bot_hits = bot_reqs.saturating_sub(bot_misses);

    let total_us = m.total_duration_us.load(Ordering::Relaxed);
    let avg_latency_us = if total_reqs > 0 {
        total_us as f64 / total_reqs as f64
    } else {
        0.0
    };
    let rps = total_reqs as f64 / total_time.as_secs_f64();

    let overall_hit_rate = if total_reqs > 0 {
        (total_hits as f64 / total_reqs as f64) * 100.0
    } else {
        0.0
    };
    let human_hit_rate = if human_reqs > 0 {
        (human_hits as f64 / human_reqs as f64) * 100.0
    } else {
        0.0
    };
    let bot_hit_rate = if bot_reqs > 0 {
        (bot_hits as f64 / bot_reqs as f64) * 100.0
    } else {
        0.0
    };

    let report_label = match algorithm {
        Algorithm::Dtac => "DTAC",
        Algorithm::Lru => "STANDARD LRU (BASELINE)",
    };
    println!("\n========================================================");
    println!("          {report_label} DIRECT BENCHMARK REPORT          ");
    println!("          (post-warm-up, steady-state measurement)        ");
    println!("========================================================");
    println!("Elapsed Time:           {total_time:.3?}");
    println!("Throughput:             {rps:.2} ops/sec");
    println!("Average Access Time:    {avg_latency_us:.3} µs");
    println!("--------------------------------------------------------");
    println!("PRIMARY METRIC");
    println!("Human Requests:         {human_reqs}");
    println!(
        "Human Cache Hit Rate:   {human_hit_rate:.2}% ({human_hits} hits / {human_misses} misses)"
    );
    println!("--------------------------------------------------------");
    println!("SECONDARY METRIC");
    println!("Total Requests:         {total_reqs}");
    println!("Overall Cache Hit Rate: {total_hits} ({overall_hit_rate:.2}%)");
    println!(
        "Total Cache Misses:     {} ({:.2}%)",
        total_misses,
        100.0 - overall_hit_rate
    );
    println!("--------------------------------------------------------");
    println!("FOR REFERENCE ONLY -- not an optimization target");
    println!("Bot Requests:           {bot_reqs}");
    println!("Bot Cache Hit Rate:     {bot_hit_rate:.2}% ({bot_hits} hits / {bot_misses} misses)");
    println!("========================================================");

    // Print internal Prometheus metrics gathered by DTAC / OpenTelemetry
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
