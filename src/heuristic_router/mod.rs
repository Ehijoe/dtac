//! Module for assigning AI-score based on the DTAC heuristics.
//!
//! This module contains the `HeuristicRouter` which evaluates incoming
//! requests and applies various heuristics to determine a caching and routing score.

pub mod space_saver;
pub mod asn_db;
/// Module defining the structure and logic for AI scores.
pub mod score;

use crate::types::RequestData;
use crate::error::Result;
use score::AiScore;
use space_saver::SpaceSaver;
use asn_db::AsnDatabase;
use tracing::{debug, instrument, info};
use std::time::{SystemTime, UNIX_EPOCH, Instant};
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};

const BURST_THRESHOLD: f64 = 10.0;
const ALPHA: f64 = 2.0;

/// Page requests an IP may make without ever fetching an auxiliary asset before the
/// "Shy Guys" heuristic fires. This is a grace period: a short human visit that never
/// pulls the favicon (for example because the browser already cached it) must not be
/// penalized.
const SHY_PAGE_THRESHOLD: u16 = 5;

/// File extensions treated as auxiliary (non-page) assets by the "Shy Guys" heuristic.
const AUXILIARY_ASSET_EXTENSIONS: &[&str] = &[
    "ico", "css", "js", "mjs", "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "woff", "woff2", "ttf",
];

/// The main router holding internal state across requests.
///
/// It uses a combination of heuristics and a base weight to determine
/// the final score for a given request.
#[derive(Debug)]
pub struct HeuristicRouter {
    /// Baseline Prior Log-Odds (L_0)
    l_0: f64,
    /// Datacenter ASN log-likelihood weight (W_asn)
    w_asn: f64,
    /// "Shy Guys" (missing assets) log-likelihood weight (W_fav)
    w_fav: f64,
    /// Anomalous header log-likelihood weight (W_hdr)
    w_hdr: f64,
    /// IP → ASN lookup used by the datacenter heuristic.
    asn_db: AsnDatabase,
    /// Bounded-memory tracker for IP state.
    space_saver: SpaceSaver,
    
    // Telemetry
    eval_requests_counter: Counter<u64>,
    eval_duration: Histogram<f64>,
}

impl HeuristicRouter {
    /// Creates a new `HeuristicRouter` with default weights and an empty ASN database
    /// (so the datacenter heuristic never fires).
    pub fn new() -> Self {
        Self::build(AsnDatabase::empty())
    }

    /// Creates a router with default weights that flags datacenter IPs using `asn_db`.
    pub fn with_asn_db(asn_db: AsnDatabase) -> Self {
        Self::build(asn_db)
    }

    /// Shared constructor.
    fn build(asn_db: AsnDatabase) -> Self {
        let meter = global::meter("dtac");

        Self {
            l_0: 0.0,
            w_asn: 1.5,
            w_fav: 1.2,
            w_hdr: 1.0,
            asn_db,
            space_saver: SpaceSaver::new(10000, 3600), // Default capacity 10000 IPs, 1 hour TTL
            eval_requests_counter: meter.u64_counter("dtac.router.evaluations").with_description("Number of requests evaluated by heuristic router").init(),
            eval_duration: meter.f64_histogram("dtac.router.eval_duration").with_description("Time spent evaluating a request in seconds").init(),
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
        ip_state.record_request(is_auxiliary_asset(&req.path), SHY_PAGE_THRESHOLD);
        self.space_saver.update(client_ip, ip_state.clone()).await;

        // --- PHASE 3: Log-Odds Heuristic Evaluation ---
        let mut score = self.l_0;

        // Heuristic A: Continuous Graded Velocity Penalty
        let float_v = new_velocity as f64 / 1024.0;
        if float_v > BURST_THRESHOLD {
            score += ALPHA * (float_v / BURST_THRESHOLD).ln();
        }

        // Heuristic B: Infrastructure Origin (Datacenter ASN)
        let is_datacenter = self.asn_db.is_datacenter(&client_ip);
        if is_datacenter {
            score += self.w_asn;
        }

        // Heuristic C: "Shy Guys" Passive Behavioral Check
        let missing_assets = ip_state.missing_auxiliary_assets;
        if missing_assets {
            score += self.w_fav;
        }

        // Heuristic D: Anomalous/Missing Headers
        // In hyper/http crates, headers are usually lowercased or case-insensitive.
        // Assuming req.headers keys might need to match "accept-language" here.
        let has_accept_lang = req.headers.keys().any(|k| k.eq_ignore_ascii_case("accept-language"));
        if !has_accept_lang {
            score += self.w_hdr;
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
    let path = path.split(['?', '#']).next().unwrap_or_default().to_ascii_lowercase();
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
        for path in ["/static/site.css", "/app.js", "/m.mjs", "/img/a.WEBP", "/fonts/x.woff2", "/logo.svg"] {
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
        for path in ["/", "/item/42", "/api/test", "/about.html", "/v1.2/item", "/favicon.ico.html"] {
            assert!(!is_auxiliary_asset(path), "{path}");
        }
    }
}
