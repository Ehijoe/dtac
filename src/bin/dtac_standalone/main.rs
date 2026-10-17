//! Standalone binary for running the DTAC cache engine as a proxy.

mod client_ip;
#[path = "../common/mod.rs"]
mod common;

use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{any, get},
};
use clap::Parser;
use client_ip::Cidr;
use common::AsnArgs;
use dtac::dtac_cache::DtacCache;
use dtac::heuristic_router::HeuristicRouter;
use dtac::types::RequestData;
use prometheus::{Encoder, TextEncoder};
use std::net::SocketAddr;
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

    /// Port to listen on
    #[arg(short = 'p', long, default_value_t = 8080)]
    port: u16,

    /// A reverse proxy (CIDR or single address) whose `X-Forwarded-For` header is
    /// believed. Repeat for several. Without any, the TCP peer is always the client.
    #[arg(long = "trusted-proxy", value_name = "CIDR")]
    trusted_proxies: Vec<Cidr>,

    #[command(flatten)]
    asn: AsnArgs,
}

/// Handles a single proxied request.
///
/// The client IP is the TCP peer, or, when the peer is a `--trusted-proxy`, the
/// right-most untrusted `X-Forwarded-For` hop (see [`client_ip::client_ip`]). The cache
/// key is the path; the origin is asked for the path and query string.
#[axum::debug_handler]
async fn proxy_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> impl IntoResponse {
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

    state
        .cache
        .access(path, request_data, fetch_from_origin)
        .await
}

#[derive(Clone)]
struct AppState {
    cache: Arc<DtacCache<String, bytes::Bytes>>,
    target: String,
    /// One pooled client for every origin fetch.
    client: reqwest::Client,
    trusted_proxies: Arc<[Cidr]>,
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
    tracing::info!("Listening on port: {}", args.port);

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
    let cache = Arc::new(DtacCache::builder(args.capacity).router(router).build());
    if !args.trusted_proxies.is_empty() {
        tracing::info!(trusted = ?args.trusted_proxies, "Believing X-Forwarded-For from these peers");
    }
    let state = AppState {
        cache,
        target: args.target.clone(),
        client: reqwest::Client::new(),
        trusted_proxies: args.trusted_proxies.clone().into(),
        registry,
    };

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .fallback(any(proxy_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
