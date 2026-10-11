//! Tests for the combined `DtacCache` and its builder

#![allow(clippy::unwrap_used)]

use dtac::cache_engine::config::EngineConfig;
use dtac::clock::ManualClock;
use dtac::dtac_cache::DtacCache;
use dtac::heuristic_router::HeuristicRouter;
use dtac::types::RequestData;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

fn request() -> RequestData {
    RequestData {
        ip_address: "24.1.2.3".parse().unwrap(),
        path: "/item/1".to_string(),
        method: "GET".to_string(),
        headers: HashMap::from([("accept-language".to_string(), "en-US".to_string())]),
    }
}

#[test]
fn builder_shares_one_clock_between_router_and_engine() {
    let clock = Arc::new(ManualClock::new(42_000));
    let cache = DtacCache::<String, String>::builder(1024)
        .router(HeuristicRouter::new())
        .clock(clock.clone())
        .build();
    clock.advance(Duration::from_secs(1));
    assert_eq!(cache.router().clock().now_millis(), 43_000);
    assert_eq!(cache.engine().clock().now_millis(), 43_000);
}

#[test]
fn engine_uses_the_routers_clock_by_default() {
    let clock = Arc::new(ManualClock::new(7_000));
    let cache = DtacCache::<String, String>::builder(1024)
        .router(HeuristicRouter::new().with_clock(clock))
        .build();
    assert_eq!(cache.engine().clock().now_millis(), 7_000);
}

#[test]
fn engine_config_is_applied_and_validated() {
    let config = EngineConfig {
        n_samples: 14,
        ..Default::default()
    };
    let cache = DtacCache::<String, String>::builder(1024)
        .engine_config(config)
        .unwrap()
        .build();
    assert_eq!(cache.engine().config(), &config);

    let invalid = EngineConfig {
        decay_half_life_ms: 0,
        ..Default::default()
    };
    assert!(
        DtacCache::<String, String>::builder(1024)
            .engine_config(invalid)
            .is_err()
    );
}

#[tokio::test]
async fn access_decays_on_the_shared_clock() {
    let clock = Arc::new(ManualClock::new(0));
    let cache = DtacCache::<String, String>::builder(1024)
        .clock(clock.clone())
        .build();
    let key = "/item/1".to_string();
    let _ = cache
        .access(key.clone(), request(), |_k| async { "p".to_string() })
        .await;
    let fresh = cache.engine().debug_score(&key).unwrap();
    assert!(fresh > 0.0);

    clock.advance(Duration::from_secs(1));
    let halved = cache.engine().debug_score(&key).unwrap();
    assert!((halved - fresh / 2.0).abs() < f32::EPSILON);
}
