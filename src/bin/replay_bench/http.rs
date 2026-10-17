//! `--target-url`: replays the log over HTTP against a running `dtac_standalone`
//! instead of an in-process engine.
//!
//! Each record becomes a request for `<target><path>?size=<size_bytes>` with the
//! record's headers, `X-Forwarded-For: <record ip>` and, on the log clock,
//! `X-Replay-Time-Ms: <record time>`. Start the proxy with `--trusted-proxy` for the
//! bench's address (so it believes both headers) and `--clock replay-header`, in front
//! of `origin_stub`. The proxy's `X-Cache: HIT|MISS` decides whether the record counts
//! as a miss, and its `X-Bot-Probability` (with `--debug-headers`) feeds the classifier
//! report. Residency isn't visible over HTTP, so there is no CPI.

use crate::engines::{BenchEngine, Outcome, Structure};
use bytes::Bytes;
use dtac::cache_engine::EvictionListener;
use dtac::clock::{Clock, ManualClock};
use dtac::types::RequestData;
use reqwest::header::{HeaderName, HeaderValue};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Header carrying the record's timestamp, for `dtac_standalone --clock replay-header`.
const REPLAY_TIME_HEADER: &str = "x-replay-time-ms";

/// Headers the bench sets itself or that describe the original connection.
const SKIPPED_HEADERS: &[&str] = &["host", "content-length", "connection", "x-forwarded-for"];

/// A `dtac_standalone` proxy at the other end of an HTTP connection.
pub struct HttpEngine {
    client: reqwest::Client,
    target: String,
    log_clock: Option<Arc<ManualClock>>,
    failed: AtomicU64,
}

impl HttpEngine {
    /// Talks to the proxy at `target` (e.g. `http://127.0.0.1:8080`). With `log_clock`,
    /// each request carries the record's timestamp.
    pub fn new(target: &str, log_clock: Option<Arc<ManualClock>>) -> Self {
        Self {
            client: reqwest::Client::new(),
            target: target.trim_end_matches('/').to_string(),
            log_clock,
            failed: AtomicU64::new(0),
        }
    }

    fn url(&self, path: &str, size: u64) -> String {
        let separator = if path.contains('?') { '&' } else { '?' };
        format!("{}{path}{separator}size={size}", self.target)
    }

    /// Sends one record; `Ok((hit, bot_prob))` from the response headers.
    async fn send(&self, request: RequestData, size: u64) -> reqwest::Result<(bool, Option<f32>)> {
        let method =
            reqwest::Method::from_bytes(request.method.as_bytes()).unwrap_or(reqwest::Method::GET);
        let mut builder = self
            .client
            .request(method, self.url(&request.path, size))
            .header("x-forwarded-for", request.ip_address.to_string());
        for (name, value) in &request.headers {
            if SKIPPED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                builder = builder.header(name, value);
            }
        }
        if let Some(clock) = &self.log_clock {
            builder = builder.header(REPLAY_TIME_HEADER, clock.now_millis());
        }
        let response = builder.send().await?.error_for_status()?;
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let hit = header("x-cache").is_some_and(|v| v.eq_ignore_ascii_case("HIT"));
        let bot_prob = header("x-bot-probability").and_then(|v| v.parse().ok());
        // Read the body so the timing includes the transfer and the connection is reused.
        response.bytes().await?;
        Ok((hit, bot_prob))
    }
}

impl BenchEngine for HttpEngine {
    fn name(&self) -> &'static str {
        "dtac-http"
    }

    async fn access<R, F, Fut>(&self, key: String, size: u64, request: R, fetch: F) -> Outcome
    where
        R: FnOnce() -> RequestData + Send,
        F: FnOnce(&String) -> Fut + Send,
        Fut: Future<Output = Bytes> + Send,
    {
        let (hit, bot_prob) = match self.send(request(), size).await {
            Ok(result) => result,
            Err(err) => {
                // Counted as a miss; the run fails at the end if any request failed.
                if self.failed.fetch_add(1, Ordering::Relaxed) == 0 {
                    eprintln!("request for {key} failed: {err}");
                }
                (false, None)
            }
        };
        if !hit {
            // Records the miss in the bench's metrics; the payload is discarded.
            fetch(&key).await;
        }
        Outcome {
            bot_prob,
            ..Outcome::default()
        }
    }

    fn resident_keys(&self) -> Vec<String> {
        Vec::new()
    }

    fn set_eviction_listener(&mut self, _listener: EvictionListener<String>) {}

    fn structure(&self) -> Structure {
        Structure::default()
    }

    fn observes_residency(&self) -> bool {
        false
    }

    fn failed_requests(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_goes_in_the_query_string() {
        let engine = HttpEngine::new("http://127.0.0.1:8080/", None);
        assert_eq!(
            engine.url("/item/7", 4096),
            "http://127.0.0.1:8080/item/7?size=4096"
        );
        assert_eq!(
            engine.url("/search?q=x", 10),
            "http://127.0.0.1:8080/search?q=x&size=10"
        );
    }
}
