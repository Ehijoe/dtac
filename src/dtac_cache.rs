use crate::cache_engine::DtacCacheEngine;
use crate::heuristic_router::HeuristicRouter;
use crate::types::RequestData;
use std::hash::Hash;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use std::time::Instant;
use tracing::{info, debug};

/// Combined DTAC component containing both the Cache Engine and Heuristic Router
#[derive(Debug)]
pub struct DtacCache<K, P> {
    engine: DtacCacheEngine<K, P>,
    router: HeuristicRouter,
    hits_counter: Counter<u64>,
    misses_counter: Counter<u64>,
    processing_time: Histogram<f64>,
}

impl<K: Eq + Hash + Clone, P: Clone> DtacCache<K, P> {
    /// Creates a new `DtacCache` with specified cache capacity and SpaceSaver parameters.
    pub fn new(capacity: usize) -> Self {
        let meter = global::meter("dtac");
        
        Self {
            engine: DtacCacheEngine::new(capacity),
            router: HeuristicRouter::new(),
            hits_counter: meter.u64_counter("dtac.cache.hits").with_description("Number of cache hits").init(),
            misses_counter: meter.u64_counter("dtac.cache.misses").with_description("Number of cache misses").init(),
            processing_time: meter.f64_histogram("dtac.request.duration").with_description("Processing time for a request in seconds").init(),
        }
    }

    /// Accesses a resource from the cache.
    ///
    /// It first evaluates the request using the heuristic router to get a bot score,
    /// then calls the cache engine's `access` method.

    pub async fn access<F, Fut>(
        &self,
        requested_resource: K,
        request_data: RequestData,
        fetch_from_origin: F,
    ) -> P
    where
        F: FnOnce(&K) -> Fut,
        Fut: Future<Output = P>,
    {
        let start_time = Instant::now();
        
        debug!("Processing request for resource");

        // Calculate the score proxying for AI-Score
        let request_bot_prob = match self.router.evaluate_request(&request_data).await {
            Ok(score) => score.value() as f32,
            Err(_) => 0.5, // fallback if evaluation fails
        };
        
        // Track whether it was a hit or miss
        let mut was_hit = true;
        
        // Use the score in the caching engine
        let result = self.engine.access(requested_resource, request_bot_prob, |k| {
            was_hit = false;
            fetch_from_origin(k)
        }).await;
        
        let duration = start_time.elapsed().as_secs_f64();
        
        if was_hit {
            self.hits_counter.add(1, &[]);
            debug!("Cache hit");
        } else {
            self.misses_counter.add(1, &[]);
            debug!("Cache miss");
        }
        
        self.processing_time.record(duration, &[]);
        
        info!(
            hit = was_hit,
            bot_prob = request_bot_prob,
            duration_sec = duration,
            "Request processed"
        );
        
        result
    }
}
