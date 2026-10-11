use crate::cache_engine::DtacCacheEngine;
use crate::heuristic_router::HeuristicRouter;
use crate::types::RequestData;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::time::Instant;
use tracing::{debug, info};

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
    /// Creates a new `DtacCache` with the given capacity and default components.
    ///
    /// Shorthand for `DtacCache::builder(capacity).build()`.
    pub fn new(capacity: usize) -> Self {
        Self::builder(capacity).build()
    }

    /// Starts building a `DtacCache` with the given cache capacity.
    pub fn builder(capacity: usize) -> DtacCacheBuilder<K, P> {
        DtacCacheBuilder {
            capacity,
            router: None,
            _types: PhantomData,
        }
    }

    fn from_parts(engine: DtacCacheEngine<K, P>, router: HeuristicRouter) -> Self {
        let meter = global::meter("dtac");

        Self {
            engine,
            router,
            hits_counter: meter
                .u64_counter("dtac.cache.hits")
                .with_description("Number of cache hits")
                .init(),
            misses_counter: meter
                .u64_counter("dtac.cache.misses")
                .with_description("Number of cache misses")
                .init(),
            processing_time: meter
                .f64_histogram("dtac.request.duration")
                .with_description("Processing time for a request in seconds")
                .init(),
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
        let result = self
            .engine
            .access(requested_resource, request_bot_prob, |k| {
                was_hit = false;
                fetch_from_origin(k)
            })
            .await;

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

/// Builder for [`DtacCache`], created by [`DtacCache::builder`].
///
/// Components that are not set explicitly use their defaults.
pub struct DtacCacheBuilder<K, P> {
    capacity: usize,
    router: Option<HeuristicRouter>,
    // `fn() -> _` keeps the builder `Send + Sync` regardless of `K` and `P`.
    _types: PhantomData<fn() -> (K, P)>,
}

impl<K, P> fmt::Debug for DtacCacheBuilder<K, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtacCacheBuilder")
            .field("capacity", &self.capacity)
            .field("router", &self.router)
            .finish()
    }
}

impl<K: Eq + Hash + Clone, P: Clone> DtacCacheBuilder<K, P> {
    /// Uses `router` to score requests instead of [`HeuristicRouter::new`].
    #[must_use]
    pub fn router(mut self, router: HeuristicRouter) -> Self {
        self.router = Some(router);
        self
    }

    /// Builds the cache.
    pub fn build(self) -> DtacCache<K, P> {
        DtacCache::from_parts(
            DtacCacheEngine::new(self.capacity),
            self.router.unwrap_or_default(),
        )
    }
}
