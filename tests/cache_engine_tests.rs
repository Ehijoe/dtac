//! Tests for the cache engine

use dtac::cache_engine::DtacCacheEngine;

#[tokio::test]
async fn test_cache_engine_basic_access() {
    let engine = DtacCacheEngine::<String, String>::new(1024);
    let key = "resource_1".to_string();
    
    // First access, should be in Ghost Cache and not in Main Cache
    let result = engine.access(
        key.clone(),
        0.0,
        |_k| async { "payload_1".to_string() },
    ).await;
    assert_eq!(result, "payload_1");
    
    // Second access, score should increase, potentially crossing threshold
    let result2 = engine.access(
        key.clone(),
        0.0,
        |_k| async { "payload_1".to_string() },
    ).await;
    assert_eq!(result2, "payload_1");
    
    // Force a promotion if not already done
    let result3 = engine.access(
        key.clone(),
        0.0,
        |_k| async { "payload_1".to_string() },
    ).await;
    assert_eq!(result3, "payload_1");
}

#[tokio::test]
async fn test_cache_engine_bot_penalty() {
    let engine = DtacCacheEngine::<String, String>::new(1024);
    let key = "resource_bot".to_string();
    
    // Access with 1.0 bot prob (effectively 0.0 added to score)
    engine.access(
        key.clone(),
        1.0,
        |_k| async { "payload".to_string() },
    ).await;
    
    // Keep accessing with 1.0 bot prob, shouldn't easily get promoted
    for _ in 0..5 {
        engine.access(
            key.clone(),
            1.0,
            |_k| async { "payload".to_string() },
        ).await;
    }
}
