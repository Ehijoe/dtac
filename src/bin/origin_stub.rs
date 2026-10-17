//! A stand-in origin server for the networked benchmark: answers every `GET` with
//! `size` zero bytes, where `size` is the `?size=N` query parameter (default
//! `--default-size`). Payloads are slices of one preallocated buffer, so the origin
//! costs almost nothing and the benchmark measures the proxy.

use axum::{
    Router,
    extract::{Query, State},
    routing::any,
};
use bytes::Bytes;
use clap::Parser;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddr};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Origin stub for the networked DTAC benchmark"
)]
struct Args {
    /// Port to listen on; 0 picks a free one. The bound address is printed to stdout as
    /// `Listening on <addr>`.
    #[arg(short = 'p', long, default_value_t = 8081)]
    port: u16,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,

    /// Bytes returned when the request has no `size` parameter.
    #[arg(long, default_value_t = 1024)]
    default_size: usize,

    /// Largest payload served; bigger sizes are cut to this.
    #[arg(long, default_value_t = 4 * 1024 * 1024)]
    max_size: usize,
}

#[derive(Deserialize)]
struct SizeQuery {
    size: Option<usize>,
}

#[derive(Clone)]
struct AppState {
    zeros: Bytes,
    default_size: usize,
}

/// `size` zero bytes, cut to the buffer's length.
fn payload(state: &AppState, size: Option<usize>) -> Bytes {
    let len = size.unwrap_or(state.default_size).min(state.zeros.len());
    state.zeros.slice(..len)
}

async fn handler(State(state): State<AppState>, Query(query): Query<SizeQuery>) -> Bytes {
    payload(&state, query.size)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let state = AppState {
        zeros: Bytes::from(vec![0_u8; args.max_size]),
        default_size: args.default_size,
    };
    let app = Router::new().fallback(any(handler)).with_state(state);
    let listener = tokio::net::TcpListener::bind(SocketAddr::new(args.bind, args.port)).await?;
    println!("Listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_size_comes_from_the_query_and_is_capped() {
        let state = AppState {
            zeros: Bytes::from(vec![0_u8; 100]),
            default_size: 10,
        };
        assert_eq!(payload(&state, Some(42)).len(), 42);
        assert_eq!(payload(&state, None).len(), 10);
        assert_eq!(payload(&state, Some(1_000)).len(), 100);
        assert!(payload(&state, Some(5)).iter().all(|&b| b == 0));
    }
}
