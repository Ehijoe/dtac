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

