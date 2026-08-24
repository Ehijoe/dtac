use crate::cache_engine::DtacCacheEngine;
use crate::heuristic_router::HeuristicRouter;
use crate::types::RequestData;
use std::hash::Hash;

/// Combined DTAC component containing both the Cache Engine and Heuristic Router
#[derive(Debug)]
pub struct DtacCache<K, P> {
    engine: DtacCacheEngine<K, P>,
    router: HeuristicRouter,
}

impl<K: Eq + Hash + Clone, P: Clone> DtacCache<K, P> {
    /// Creates a new `DtacCache` with specified cache capacity and SpaceSaver parameters.
    pub fn new(capacity: usize) -> Self {
        Self {
            engine: DtacCacheEngine::new(capacity),
            router: HeuristicRouter::new(),
        }
    }

    /// Accesses a resource from the cache.
    ///
    /// It first evaluates the request using the heuristic router to get a bot score,
    /// then calls the cache engine's `access` method.
    pub async fn access<F>(
        &self,
        requested_resource: K,
        request_data: RequestData,
        fetch_from_origin: F,
    ) -> P
    where
        F: FnOnce(&K) -> P,
    {
        // Calculate the score proxying for AI-Score
        let request_bot_prob = match self.router.evaluate_request(&request_data).await {
            Ok(score) => score.value() as f32,
            Err(_) => 0.5, // fallback if evaluation fails
        };
        
        // Use the score in the caching engine
        self.engine.access(requested_resource, request_bot_prob, fetch_from_origin)
    }
}
