//! Tests for the heuristic router

use dtac::heuristic_router::HeuristicRouter;
use dtac::types::RequestData;
use std::net::IpAddr;
use std::collections::HashMap;

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


mod asn_lookup {
    #![allow(clippy::unwrap_used)]

    use dtac::heuristic_router::HeuristicRouter;
    use dtac::heuristic_router::asn_db::AsnDatabase;
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

    #[test]
    fn committed_fixture_parses() {
        let db = fixture_db();
        assert!(db.v4_range_count() > 0);
        assert!(db.v6_range_count() > 0);
        for dc in ["45.33.12.4", "104.248.33.44", "167.172.9.201", "2600:3c01::1"] {
            assert!(db.is_datacenter(&ip(dc)), "{dc} should be a datacenter IP");
        }
        for not_dc in ["198.51.100.23", "203.0.113.14", ISP_IP, "86.1.2.3", "2a02:c7d::1"] {
            assert!(!db.is_datacenter(&ip(not_dc)), "{not_dc} should not be a datacenter IP");
        }
    }

    #[test]
    fn separate_v4_and_v6_files_are_merged() {
        let text = std::fs::read_to_string(fixture_path()).unwrap();
        let (v6_rows, v4_rows): (Vec<&str>, Vec<&str>) =
            text.lines().filter(|l| !l.starts_with('#')).partition(|l| l.contains(':'));

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
}
