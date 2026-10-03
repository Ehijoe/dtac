//! Standalone binary for running the DTAC cache engine as a proxy.

use axum::{
    extract::{ConnectInfo, Request, State},
    routing::{any, get},
    Router,
    response::IntoResponse,
};
use clap::Parser;
use dtac::dtac_cache::DtacCache;
use dtac::types::RequestData;
use std::sync::Arc;
use std::net::SocketAddr;
use prometheus::{Encoder, TextEncoder};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Simple standalone binary running the DTAC cache engine as a proxy.
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The total capacity of the cache.
    #[arg(short = 'c', long, default_value_t = 1024)]
    capacity: usize,

    /// The 'k' value for SpaceSaver sketch.
    #[arg(short = 'k', long, default_value_t = 100)]
    sketch_k: usize,

    /// Depth for the Count-Min Sketch.
    #[arg(short = 'd', long, default_value_t = 5)]
    cm_depth: usize,

    /// Width for the Count-Min Sketch.
    #[arg(short = 'w', long, default_value_t = 2000)]
    cm_width: usize,

    /// Upstream target URL to forward requests to
    #[arg(short = 't', long, default_value = "http://127.0.0.1:8081")]
    target: String,

    /// Port to listen on
    #[arg(short = 'p', long, default_value_t = 8080)]
    port: u16,
}

/// Handles a single proxied request.
///
/// Uses the direct TCP peer address via `ConnectInfo`. Does not parse
/// `X-Forwarded-For`/`X-Real-IP`; if DTAC is deployed behind a reverse proxy
/// or load balancer, the observed IP will be the proxy's, not the end
/// client's. Supporting forwarded-header parsing is a deliberate non-goal of
/// this change -- it requires a trusted-proxy allowlist to avoid a
/// trivially spoofable client IP, which is a separate piece of design work.
#[axum::debug_handler]
async fn proxy_handler(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> impl IntoResponse {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let mut headers_map = std::collections::HashMap::new();
    for (k, v) in req.headers() {
        if let Ok(val) = v.to_str() {
            headers_map.insert(k.to_string(), val.to_string());
        }
    }

    let ip_address = addr.ip();

    let request_data = RequestData {
        ip_address,
        path: path.clone(),
        method,
        headers: headers_map,
    };

    let target = state.target.clone();

    let fetch_from_origin = |key: &String| {
        let url = format!("{}{}", target, key);
        async move {
            let client = reqwest::Client::new();
            match client.get(&url).send().await {
                Ok(resp) => resp.bytes().await.unwrap_or_default(),
                Err(_) => bytes::Bytes::new(),
            }
        }
    };

    let payload = state.cache.access(path, request_data, fetch_from_origin).await;

    payload
}

#[derive(Clone)]
struct AppState {
    cache: Arc<DtacCache<String, bytes::Bytes>>,
    target: String,
    registry: prometheus::Registry,
}

async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let encoder = TextEncoder::new();
    let mut buffer = vec![];
    let metric_families = state.registry.gather();
    encoder.encode(&metric_families, &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}

#[tokio::main]
async fn main() {
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
        .build()
        .expect("Failed to build prometheus exporter");
    
    let meter_provider = opentelemetry_sdk::metrics::SdkMeterProvider::builder()
        .with_reader(exporter)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider);

    let args = Args::parse();
    tracing::info!("Initializing DTAC Proxy with capacity: {}", args.capacity);
    tracing::info!("Forwarding to target: {}", args.target);
    tracing::info!("Listening on port: {}", args.port);

    let cache = Arc::new(DtacCache::new(args.capacity));
    let state = AppState {
        cache,
        target: args.target.clone(),
        registry,
    };

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .fallback(any(proxy_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}
