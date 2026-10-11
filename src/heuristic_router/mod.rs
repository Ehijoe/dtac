//! Module for assigning AI-score based on the DTAC heuristics.
//!
//! This module contains the `HeuristicRouter` which evaluates incoming
//! requests and applies various heuristics to determine a caching and routing score.

pub mod asn_db;
pub mod config;
/// Module defining the structure and logic for AI scores.
pub mod score;
pub mod space_saver;

use crate::error::Result;
use crate::types::RequestData;
use asn_db::AsnDatabase;
use config::RouterConfig;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use score::AiScore;
use space_saver::SpaceSaver;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, instrument};

/// File extensions treated as auxiliary (non-page) assets by the "Shy Guys" heuristic.
const AUXILIARY_ASSET_EXTENSIONS: &[&str] = &[
    "ico", "css", "js", "mjs", "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "woff", "woff2",
    "ttf",
];

/// The main router holding internal state across requests.
///
/// It uses a combination of heuristics and a base weight to determine
/// the final score for a given request.
#[derive(Debug)]
pub struct HeuristicRouter {
    /// Weights and thresholds of the log-odds model.
    config: RouterConfig,
    /// Baseline prior log-odds (`l_0`), derived once from `config.prior_bot_rate`.
    l_0: f64,
    /// IP → ASN lookup used by the datacenter heuristic.
    asn_db: AsnDatabase,
    /// Bounded-memory tracker for IP state.
    space_saver: SpaceSaver,

    // Telemetry
    eval_requests_counter: Counter<u64>,
    eval_duration: Histogram<f64>,
}

impl HeuristicRouter {
    /// Creates a new `HeuristicRouter` with the default [`RouterConfig`] and an empty
    /// ASN database (so the datacenter heuristic never fires).
    pub fn new() -> Self {
        Self::build(RouterConfig::default(), AsnDatabase::empty())
    }

    /// Creates a router with the default [`RouterConfig`] that flags datacenter IPs
    /// using `asn_db`.
    pub fn with_asn_db(asn_db: AsnDatabase) -> Self {
        Self::build(RouterConfig::default(), asn_db)
    }

    /// Creates a router with a custom [`RouterConfig`] and ASN database.
    ///
    /// Fails with [`DTACError::ConfigError`](crate::error::DTACError::ConfigError) if
    /// `config` does not pass [`RouterConfig::validate`].
    pub fn with_config(config: RouterConfig, asn_db: AsnDatabase) -> Result<Self> {
        config.validate()?;
        Ok(Self::build(config, asn_db))
    }

    /// The configuration this router scores requests with.
    pub const fn config(&self) -> &RouterConfig {
        &self.config
    }

    /// Shared constructor; `config` must already be valid.
    fn build(config: RouterConfig, asn_db: AsnDatabase) -> Self {
        let meter = global::meter("dtac");

        Self {
            l_0: config.prior_log_odds(),
            config,
            asn_db,
            space_saver: SpaceSaver::new(10000, 3600), // Default capacity 10000 IPs, 1 hour TTL
            eval_requests_counter: meter
                .u64_counter("dtac.router.evaluations")
                .with_description("Number of requests evaluated by heuristic router")
                .init(),
            eval_duration: meter
                .f64_histogram("dtac.router.eval_duration")
                .with_description("Time spent evaluating a request in seconds")
                .init(),
        }
    }

    /// Helper to get current timestamp in seconds.
    fn current_timestamp_seconds() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Evaluates a request and assigns an AI score.
    ///
    /// Implements the Ingress Proxy Pipeline & Log-Odds AI-Score Generation algorithm.
    #[instrument(skip(self, req))]
    pub async fn evaluate_request(&self, req: &RequestData) -> Result<AiScore> {
        let start = Instant::now();
        self.eval_requests_counter.add(1, &[]);

        debug!(ip = %req.ip_address, path = %req.path, "Evaluating incoming request");

        let client_ip = req.ip_address;
        let current_time = Self::current_timestamp_seconds();

        // --- PHASE 1: State Retrieval & Bounded Memory Management ---
        let mut ip_state = self.space_saver.get_or_add_and_evict(client_ip).await;

        // --- PHASE 2: EWMA Velocity Calculation (Bitwise) ---
        let delta_t = current_time.saturating_sub(ip_state.last_seen);

        let new_velocity = if delta_t > 10 {
            // Decayed to 0, plus 1 request (scaled by 1024)
            1024
        } else {
            // Bitwise right-shift for exponential decay
            let decayed_v = ip_state.velocity >> delta_t;
            decayed_v + 1024
        };

        // Commit updated state back to memory
        ip_state.last_seen = current_time;
        ip_state.velocity = new_velocity;
        ip_state.record_request(
            is_auxiliary_asset(&req.path),
            self.config.shy_page_threshold,
        );
        self.space_saver.update(client_ip, ip_state.clone()).await;

        // --- PHASE 3: Log-Odds Heuristic Evaluation ---
        let mut score = self.l_0;

        // Heuristic A: Continuous Graded Velocity Penalty
        let float_v = new_velocity as f64 / 1024.0;
        if float_v > self.config.burst_threshold {
            score += self.config.alpha * (float_v / self.config.burst_threshold).ln();
        }

        // Heuristic B: Infrastructure Origin (Datacenter ASN)
        let is_datacenter = self.asn_db.is_datacenter(&client_ip);
        if is_datacenter {
            score += self.config.w_asn;
        }

        // Heuristic C: "Shy Guys" Passive Behavioral Check
        let missing_assets = ip_state.missing_auxiliary_assets;
        if missing_assets {
            score += self.config.w_fav;
        }

        // Heuristic D: Anomalous/Missing Headers
        // In hyper/http crates, headers are usually lowercased or case-insensitive.
        // Assuming req.headers keys might need to match "accept-language" here.
        let has_accept_lang = req
            .headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("accept-language"));
        if !has_accept_lang {
            score += self.config.w_hdr;
        }

        // --- PHASE 4: Sigmoid Conversion & Forwarding ---
        let bot_probability = 1.0 / (1.0 + (-score).exp());

        let duration = start.elapsed().as_secs_f64();
        self.eval_duration.record(duration, &[]);

        info!(
            ip = %client_ip,
            bot_probability = bot_probability,
            is_datacenter = is_datacenter,
            missing_assets = missing_assets,
            page_requests = ip_state.page_requests,
            has_accept_lang = has_accept_lang,
            velocity = float_v,
            duration_sec = duration,
            "Request evaluated"
        );

        Ok(AiScore::new(bot_probability))
    }
}

impl Default for HeuristicRouter {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `path` names an auxiliary asset (favicon, stylesheet, script, image, font)
/// rather than a page, ignoring any `?query` / `#fragment` and letter case.
fn is_auxiliary_asset(path: &str) -> bool {
    let path = path
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if path == "/favicon.ico" || path.starts_with("/apple-touch-icon") {
        return true;
    }
    let file_name = path.rsplit('/').next().unwrap_or_default();
    file_name
        .rsplit_once('.')
        .is_some_and(|(_, ext)| AUXILIARY_ASSET_EXTENSIONS.contains(&ext))
}

#[cfg(test)]
mod tests {
    use super::is_auxiliary_asset;

    #[test]
    fn favicon_and_touch_icons_are_assets() {
        assert!(is_auxiliary_asset("/favicon.ico"));
        assert!(is_auxiliary_asset("/apple-touch-icon.png"));
        assert!(is_auxiliary_asset("/apple-touch-icon-precomposed"));
    }

    #[test]
    fn static_extensions_are_assets() {
        for path in [
            "/static/site.css",
            "/app.js",
            "/m.mjs",
            "/img/a.WEBP",
            "/fonts/x.woff2",
            "/logo.svg",
        ] {
            assert!(is_auxiliary_asset(path), "{path}");
        }
    }

    #[test]
    fn query_fragment_and_case_are_ignored() {
        assert!(is_auxiliary_asset("/FAVICON.ICO"));
        assert!(is_auxiliary_asset("/favicon.ico?v=3"));
        assert!(is_auxiliary_asset("/style.css#top"));
        assert!(!is_auxiliary_asset("/item/42?format=.css"));
    }

    #[test]
    fn pages_are_not_assets() {
        for path in [
            "/",
            "/item/42",
            "/api/test",
            "/about.html",
            "/v1.2/item",
            "/favicon.ico.html",
        ] {
            assert!(!is_auxiliary_asset(path), "{path}");
        }
    }
}
