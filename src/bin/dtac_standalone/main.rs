//! Standalone binary for running the DTAC cache engine as a proxy.

mod client_ip;
#[path = "../common/mod.rs"]
mod common;

use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{any, get},
};
use clap::Parser;
use client_ip::Cidr;
use common::AsnArgs;
use dtac::clock::ManualClock;
use dtac::dtac_cache::DtacCache;
use dtac::heuristic_router::HeuristicRouter;
use dtac::types::RequestData;
use prometheus::{Encoder, TextEncoder};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Simple standalone binary running the DTAC cache engine as a proxy.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The total capacity of the cache.
    #[arg(short = 'c', long, default_value_t = 1024)]
    capacity: usize,

    /// Upstream target URL to forward requests to
    #[arg(short = 't', long, default_value = "http://127.0.0.1:8081")]
    target: String,

    /// Port to listen on; 0 picks a free one. The bound address is printed to stdout as
    /// `Listening on <addr>`.
    #[arg(short = 'p', long, default_value_t = 8080)]
    port: u16,

    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0")]
    bind: IpAddr,

    /// A reverse proxy (CIDR or single address) whose `X-Forwarded-For` header is
    /// believed. Repeat for several. Without any, the TCP peer is always the client.
    #[arg(long = "trusted-proxy", value_name = "CIDR")]
    trusted_proxies: Vec<Cidr>,

    /// Where the cache reads time from. `replay-header` is for benchmarking only: time
    /// is whatever the latest `X-Replay-Time-Ms` header from a trusted proxy said, so
    /// `replay_bench --target-url` can replay a log on its own timestamps, as it does
    /// in process.
    #[arg(long, value_enum, default_value_t = ClockMode::System)]
    clock: ClockMode,

    /// Add `X-Bot-Probability: <score>` to every response. Benchmarking only: it tells
    /// clients how bot-like they look.
    #[arg(long)]
    debug_headers: bool,

    #[command(flatten)]
    asn: AsnArgs,
}

/// Time source for the cache, `--clock`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum ClockMode {
    /// The system clock.
    System,
    /// Simulated time from trusted `X-Replay-Time-Ms` headers.
    ReplayHeader,
}

/// The header `--clock replay-header` takes time from, in milliseconds.
const REPLAY_TIME_HEADER: &str = "x-replay-time-ms";

/// Handles a single proxied request.
///
/// The client IP is the TCP peer, or, when the peer is a `--trusted-proxy`, the
/// right-most untrusted `X-Forwarded-For` hop (see [`client_ip::client_ip`]). The cache
/// key is the path; the origin is asked for the path and query string. The response
/// says `X-Cache: HIT` or `X-Cache: MISS`.
#[axum::debug_handler]
async fn proxy_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    if let Some(clock) = &state.replay_clock
        && client_ip::is_trusted(addr.ip(), &state.trusted_proxies)
        && let Some(millis) = req
            .headers()
            .get(REPLAY_TIME_HEADER)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
    {
        clock.set_millis(millis);
    }

    let path = req.uri().path().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map_or_else(|| path.clone(), ToString::to_string);
    let method = req.method().to_string();
    let mut headers_map = std::collections::HashMap::new();
    for (k, v) in req.headers() {
        if let Ok(val) = v.to_str() {
            headers_map.insert(k.to_string(), val.to_string());
        }
    }

    let forwarded_for = req
        .headers()
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok());
    let ip_address = client_ip::client_ip(addr.ip(), forwarded_for, &state.trusted_proxies);

    let request_data = RequestData {
        ip_address,
        path: path.clone(),
        method,
        headers: headers_map,
    };

    let url = format!("{}{path_and_query}", state.target);
    let client = state.client.clone();
    let fetch_from_origin = |_key: &String| async move {
        match client.get(&url).send().await {
            Ok(resp) => resp.bytes().await.unwrap_or_default(),
            Err(_) => bytes::Bytes::new(),
        }
    };

    let (payload, access) = state
        .cache
        .access_with_stats(path, request_data, fetch_from_origin)
        .await;
    let mut response = payload.into_response();
    let headers = response.headers_mut();
    headers.insert(
        "x-cache",
        HeaderValue::from_static(if access.hit { "HIT" } else { "MISS" }),
    );
    if state.debug_headers
        && let Ok(score) = HeaderValue::from_str(&access.bot_prob.to_string())
    {
        headers.insert("x-bot-probability", score);
    }
    response
}

#[derive(Clone)]
struct AppState {
    cache: Arc<DtacCache<String, bytes::Bytes>>,
    target: String,
    /// One pooled client for every origin fetch.
    client: reqwest::Client,
    trusted_proxies: Arc<[Cidr]>,
    /// The cache's clock under `--clock replay-header`.
    replay_clock: Option<Arc<ManualClock>>,
    debug_headers: bool,
    registry: prometheus::Registry,
}

async fn metrics_handler(State(state): State<AppState>) -> Result<String, (StatusCode, String)> {
    let mut buffer = vec![];
    TextEncoder::new()
        .encode(&state.registry.gather(), &mut buffer)
        .map_err(internal_error)?;
    String::from_utf8(buffer).map_err(internal_error)
}

/// Reports `err` to the client as a 500 response.
fn internal_error(err: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Initialize metrics
    let registry = prometheus::Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()?;

    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(exporter)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider);

    let args = Args::parse();
    tracing::info!("Initializing DTAC Proxy with capacity: {}", args.capacity);
    tracing::info!("Forwarding to target: {}", args.target);

    let router = match args.asn.load() {
        Ok(Some(asn_db)) => {
            tracing::info!(
                v4_ranges = asn_db.v4_range_count(),
                v6_ranges = asn_db.v6_range_count(),
                "Loaded ASN database"
            );
            HeuristicRouter::with_asn_db(asn_db)
        }
        Ok(None) => HeuristicRouter::new(),
        Err(err) => {
            tracing::error!(%err, "Failed to load ASN database");
            std::process::exit(1);
        }
    };
    let replay_clock =
        (args.clock == ClockMode::ReplayHeader).then(|| Arc::new(ManualClock::new(0)));
    let mut builder = DtacCache::builder(args.capacity).router(router);
    if let Some(clock) = &replay_clock {
        if args.trusted_proxies.is_empty() {
            return Err("--clock replay-header needs a --trusted-proxy to take time from".into());
        }
        builder = builder.clock(clock.clone());
    }
    let cache = Arc::new(builder.build());
    if !args.trusted_proxies.is_empty() {
        tracing::info!(trusted = ?args.trusted_proxies, "Believing X-Forwarded-For from these peers");
    }
    let state = AppState {
        cache,
        target: args.target.clone(),
        client: reqwest::Client::new(),
        trusted_proxies: args.trusted_proxies.clone().into(),
        replay_clock,
        debug_headers: args.debug_headers,
        registry,
    };

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .fallback(any(proxy_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(SocketAddr::new(args.bind, args.port)).await?;
    // Printed (not logged) so scripts and tests can find an ephemeral port.
    println!("Listening on {}", listener.local_addr()?);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
