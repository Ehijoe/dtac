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

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_cache_engine_concurrent_promotion_no_orphans() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    // capacity_per_shard = total_capacity / NUM_SHARDS; keep it comfortably
    // larger than the number of racers below so eviction never triggers and
    // can't accidentally mask (or fake) an orphan.
    let engine = Arc::new(DtacCacheEngine::<String, String>::new(256 * 64));
    let key = "hot_resource".to_string();

    // Seed: first-ever access on a fresh key is always a Miss -> inserts into
    // ghost_cache with score = 1.0 (bot_prob = 0.0).
    let _ = engine.access(key.clone(), 0.0, |_k| async { "seed".to_string() }).await;

    let fetch_count = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    // Every one of these sees a ghost-cache score that's already >=
    // PROMOTION_THRESHOLD after one more increment (bot_prob = 0.0 adds a full
    // +1.0 each pass), so each independently decides Action::Promote in Phase
    // 1/2, then sleeps inside fetch_from_origin to hold the race window open
    // long enough for the other racers to pile in.
    for i in 0..32 {
        let engine = engine.clone();
        let key = key.clone();
        let fetch_count = fetch_count.clone();
        handles.push(tokio::spawn(async move {
            engine.access(key, 0.0, move |_k| {
                let fetch_count = fetch_count.clone();
                async move {
                    fetch_count.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    format!("payload-from-racer-{i}")
                }
            }).await
        }));
    }

    for h in handles {
        let _ = h.await;
    }

    // Sanity guard: make sure this test actually exercised concurrent
    // promotions and isn't passing vacuously (e.g. if scheduling changes in
    // the future reduce overlap to nothing).
    assert!(
        fetch_count.load(Ordering::SeqCst) >= 2,
        "test setup did not produce concurrent promotions; race window was not exercised"
    );

    let (total_entries, total_mapped) = engine.debug_integrity_counts();
    assert_eq!(
        total_entries, total_mapped,
        "orphaned cache entry detected: entries={total_entries}, mapped_keys={total_mapped}"
    );
}
