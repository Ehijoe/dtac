//! Standalone binary for running the DTAC cache engine as a proxy.

use axum::{
    extract::{Request, State},
    routing::any,
    Router,
    response::IntoResponse,
};
use clap::Parser;
use dtac::dtac_cache::DtacCache;
use dtac::types::RequestData;
use std::sync::Arc;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

#[derive(Clone)]
struct AppState {
    cache: Arc<DtacCache<String, bytes::Bytes>>,
    target: String,
}

async fn proxy_handler(State(state): State<AppState>, req: Request) -> impl IntoResponse {
    let path = req.uri().path().to_string();
    let method = req.method().to_string();
    let mut headers_map = std::collections::HashMap::new();
    for (k, v) in req.headers() {
        if let Ok(val) = v.to_str() {
            headers_map.insert(k.to_string(), val.to_string());
        }
    }

    let ip_address = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    
    let request_data = RequestData {
        ip_address,
        path: path.clone(),
        method,
        headers: headers_map,
    };

    let target = state.target.clone();

    let fetch_from_origin = |key: &String| -> bytes::Bytes {
        let url = format!("{}{}", target, key);
        let client = reqwest::blocking::Client::new();
        match client.get(&url).send() {
            Ok(resp) => resp.bytes().unwrap_or_default(),
            Err(_) => bytes::Bytes::new(),
        }
    };

    let payload = state.cache.access(path, request_data, fetch_from_origin).await;

    payload
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    println!("Initializing DTAC Proxy with capacity: {}", args.capacity);
    println!("Forwarding to target: {}", args.target);
    println!("Listening on port: {}", args.port);

    let cache = Arc::new(DtacCache::new(args.capacity));
    let state = AppState {
        cache,
        target: args.target.clone(),
    };

    let app = Router::new()
        .fallback(any(proxy_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
