//! Tests for the heuristic router

use dtac::heuristic_router::HeuristicRouter;
use dtac::types::RequestData;
use std::collections::HashMap;
use std::net::IpAddr;

#[tokio::test]
async fn test_heuristic_router_new() {
    let router = HeuristicRouter::new();
    assert!(format!("{:?}", router).contains("HeuristicRouter"));
}

#[tokio::test]
async fn test_heuristic_router_evaluate_request() {
    let router = HeuristicRouter::new();
    let ip: IpAddr = "192.168.1.1".parse().unwrap();
    let mut headers = HashMap::new();
    headers.insert("accept-language".to_string(), "en-US".to_string());

    let req = RequestData {
        ip_address: ip,
        path: "/api/test".to_string(),
        method: "GET".to_string(),
        headers,
    };

    let score_result = router.evaluate_request(&req).await;
    assert!(score_result.is_ok());
    let score = score_result.unwrap();

    // Check that value is within limits
    assert!(score.value() >= 0.0 && score.value() <= 1.0);
}

mod asn_and_shy_guys {
    #![allow(clippy::unwrap_used)]

    use dtac::heuristic_router::HeuristicRouter;
    use dtac::heuristic_router::asn_db::AsnDatabase;
    use dtac::heuristic_router::config::RouterConfig;
    use dtac::types::RequestData;
    use std::collections::HashMap;
    use std::net::IpAddr;
    use std::path::{Path, PathBuf};

    const DATACENTER_IP: &str = "104.248.33.7";
    const ISP_IP: &str = "24.1.2.3";

    fn fixture_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("data/asn/ip2asn-fixture.tsv")
    }

    fn fixture_db() -> AsnDatabase {
        AsnDatabase::from_ip2asn_file(&fixture_path()).unwrap()
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// A request carrying `Accept-Language`, so the header heuristic stays quiet.
    fn human_request(ip_address: &str, path: &str) -> RequestData {
        RequestData {
            ip_address: ip(ip_address),
            path: path.to_string(),
            method: "GET".to_string(),
            headers: HashMap::from([("accept-language".to_string(), "en-US".to_string())]),
        }
    }

    async fn score(router: &HeuristicRouter, req: &RequestData) -> f64 {
        router.evaluate_request(req).await.unwrap().value()
    }

    /// Sends `paths` from `ip_address` in order and returns the last request's score.
    async fn last_score(router: &HeuristicRouter, ip_address: &str, paths: &[&str]) -> f64 {
        let mut last = f64::NAN;
        for path in paths {
            last = score(router, &human_request(ip_address, path)).await;
        }
        last
    }

    fn sigmoid(x: f64) -> f64 {
        1.0 / (1.0 + (-x).exp())
    }

    #[test]
    fn committed_fixture_parses() {
        let db = fixture_db();
        assert!(db.v4_range_count() > 0);
        assert!(db.v6_range_count() > 0);
        for dc in [
            "45.33.12.4",
            "104.248.33.44",
            "167.172.9.201",
            "2600:3c01::1",
        ] {
            assert!(db.is_datacenter(&ip(dc)), "{dc} should be a datacenter IP");
        }
        for not_dc in [
            "198.51.100.23",
            "203.0.113.14",
            ISP_IP,
            "86.1.2.3",
            "2a02:c7d::1",
        ] {
            assert!(
                !db.is_datacenter(&ip(not_dc)),
                "{not_dc} should not be a datacenter IP"
            );
        }
    }

    #[test]
    fn separate_v4_and_v6_files_are_merged() {
        let text = std::fs::read_to_string(fixture_path()).unwrap();
        let (v6_rows, v4_rows): (Vec<&str>, Vec<&str>) = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .partition(|l| l.contains(':'));

        let dir = std::env::temp_dir().join(format!("dtac-asn-merge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let v4_path = dir.join("ip2asn-v4.tsv");
        let v6_path = dir.join("ip2asn-v6.tsv");
        std::fs::write(&v4_path, v4_rows.join("\n")).unwrap();
        std::fs::write(&v6_path, v6_rows.join("\n")).unwrap();

        let db = AsnDatabase::from_ip2asn_files([&v4_path, &v6_path]);
        std::fs::remove_dir_all(&dir).unwrap();
        let db = db.unwrap();

        assert!(db.is_datacenter(&ip(DATACENTER_IP)));
        assert!(db.is_datacenter(&ip("2600:3c01::1")));
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(AsnDatabase::from_ip2asn_file(Path::new("/nonexistent/ip2asn.tsv")).is_err());
    }

    #[tokio::test]
    async fn datacenter_ip_scores_higher_with_asn_db() {
        let router = HeuristicRouter::with_asn_db(fixture_db());
        let dc = score(&router, &human_request(DATACENTER_IP, "/item/1")).await;
        let isp = score(&router, &human_request(ISP_IP, "/item/1")).await;
        assert!(dc > isp, "datacenter {dc} should exceed ISP {isp}");
    }

    #[tokio::test]
    async fn datacenter_ip_is_ignored_without_asn_db() {
        let router = HeuristicRouter::new();
        let dc = score(&router, &human_request(DATACENTER_IP, "/item/1")).await;
        let isp = score(&router, &human_request(ISP_IP, "/item/1")).await;
        assert!((dc - isp).abs() < 1e-12);
    }

    #[tokio::test]
    async fn shy_guys_flags_ips_that_never_fetch_assets() {
        let router = HeuristicRouter::new();
        let pages = ["/item/1", "/item/2", "/item/3", "/item/4", "/item/5"];

        // Same request count (6) for A and B, so velocity is equal.
        let mut with_favicon = vec!["/favicon.ico"];
        with_favicon.extend(pages);
        let mut six_pages = pages.to_vec();
        six_pages.push("/item/6");

        let a = last_score(&router, "10.0.0.1", &with_favicon).await;
        let b = last_score(&router, "10.0.0.2", &six_pages).await;
        let under_threshold = last_score(&router, "10.0.0.3", &pages[..4]).await;

        assert!(b > a, "shy IP {b} should exceed favicon IP {a}");
        assert!(
            (under_threshold - a).abs() < 1e-12,
            "4 pages must not be flagged"
        );
    }

    #[tokio::test]
    async fn clean_human_scores_at_the_prior() {
        let router = HeuristicRouter::new();
        let got = score(&router, &human_request(ISP_IP, "/item/1")).await;
        assert!((got - sigmoid((1.0_f64 / 9.0).ln())).abs() < 1e-6);
        assert!((got - 0.1).abs() < 1e-6);
    }

    #[tokio::test]
    async fn even_prior_reproduces_original_scores() {
        let config = RouterConfig {
            prior_bot_rate: 0.5,
            ..Default::default()
        };
        let router = HeuristicRouter::with_config(config, AsnDatabase::empty()).unwrap();

        let clean = score(&router, &human_request(ISP_IP, "/item/1")).await;
        assert!((clean - 0.5).abs() < 1e-12);

        let mut no_header = human_request("24.9.9.9", "/item/1");
        no_header.headers.clear();
        let header_only = score(&router, &no_header).await;
        assert!((header_only - sigmoid(1.0)).abs() < 1e-12);
    }

    #[test]
    fn invalid_config_is_rejected() {
        let config = RouterConfig {
            prior_bot_rate: 1.5,
            ..Default::default()
        };
        assert!(HeuristicRouter::with_config(config, AsnDatabase::empty()).is_err());
    }
}

/// Velocity heuristic driven by an injected [`ManualClock`] instead of wall-clock time.
mod simulated_clock {
    #![allow(clippy::unwrap_used)]

    use dtac::clock::ManualClock;
    use dtac::heuristic_router::HeuristicRouter;
    use dtac::heuristic_router::asn_db::AsnDatabase;
    use dtac::heuristic_router::config::RouterConfig;
    use dtac::types::RequestData;
    use std::collections::HashMap;
    use std::sync::Arc;

    const PRIOR: f64 = 0.1;

    fn router(config: RouterConfig) -> (HeuristicRouter, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(0));
        let router = HeuristicRouter::with_config(config, AsnDatabase::empty())
            .unwrap()
            .with_clock(clock.clone());
        (router, clock)
    }

    /// Sends one request per timestamp (in milliseconds) from a single IP and returns the
    /// highest score seen.
    ///
    /// Requests fetch a stylesheet and carry `Accept-Language`, so velocity is the only
    /// heuristic that can move the score off the prior.
    async fn max_score(router: &HeuristicRouter, clock: &ManualClock, times_ms: &[u64]) -> f64 {
        let req = RequestData {
            ip_address: "24.1.2.3".parse().unwrap(),
            path: "/static/site.css".to_string(),
            method: "GET".to_string(),
            headers: HashMap::from([("accept-language".to_string(), "en-US".to_string())]),
        };
        let mut max = f64::MIN;
        for &t in times_ms {
            clock.set_millis(t);
            max = max.max(router.evaluate_request(&req).await.unwrap().value());
        }
        max
    }

    /// The benchmark's naive bot stream: one IP at about 3.6 req/s for 60 seconds.
    fn benchmark_bot_rate_times() -> Vec<u64> {
        (0..216).map(|i| i * 1000 * 10 / 36).collect()
    }

    #[tokio::test]
    async fn burst_within_one_second_trips_velocity() {
        let (router, clock) = router(RouterConfig::default());
        let times: Vec<u64> = (0..20).map(|i| i * 50).collect();
        let max = max_score(&router, &clock, &times).await;
        assert!(
            max > PRIOR + 1e-6,
            "20 req in 1 s should exceed the prior, got {max}"
        );
    }

    #[tokio::test]
    async fn same_requests_spread_over_twenty_seconds_do_not() {
        let (router, clock) = router(RouterConfig::default());
        let times: Vec<u64> = (0..20).map(|i| i * 1000).collect();
        let max = max_score(&router, &clock, &times).await;
        assert!(
            (max - PRIOR).abs() < 1e-6,
            "1 req/s should stay at the prior, got {max}"
        );
    }

    /// Documents a known gap rather than desired behaviour: on log time, the default
    /// velocity constants never flag the benchmark's per-IP bot rate (its EWMA settles
    /// around 7, below `burst_threshold = 10`). Expected to change when plan 004 picks
    /// calibrated defaults.
    #[tokio::test]
    async fn default_config_misses_benchmark_bot_rate() {
        let (router, clock) = router(RouterConfig::default());
        let max = max_score(&router, &clock, &benchmark_bot_rate_times()).await;
        assert!((max - PRIOR).abs() < 1e-6, "expected no penalty, got {max}");
    }

    #[tokio::test]
    async fn longer_velocity_tick_flags_benchmark_bot_rate() {
        let (router, clock) = router(RouterConfig {
            velocity_tick_ms: 5000,
            ..Default::default()
        });
        let max = max_score(&router, &clock, &benchmark_bot_rate_times()).await;
        assert!(
            max > PRIOR + 1e-6,
            "5 s ticks should flag 3.6 req/s, got {max}"
        );
    }
}
