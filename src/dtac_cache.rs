use crate::cache_engine::DtacCacheEngine;
use crate::cache_engine::config::EngineConfig;
use crate::clock::Clock;
use crate::error::Result;
use crate::heuristic_router::HeuristicRouter;
use crate::types::RequestData;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use std::fmt;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;
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
            clock: None,
            engine_config: EngineConfig::default(),
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

    /// The cache engine, e.g. to read its configuration or entry counts.
    pub const fn engine(&self) -> &DtacCacheEngine<K, P> {
        &self.engine
    }

    /// The router that scores requests for this cache.
    pub const fn router(&self) -> &HeuristicRouter {
        &self.router
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

        // Calculate the score proxying for AI-Score; 0.5 is the fallback if evaluation fails.
        // A probability in [0, 1] loses nothing that matters when narrowed to f32.
        #[allow(clippy::cast_possible_truncation)]
        let request_bot_prob = self
            .router
            .evaluate_request(&request_data)
            .await
            .map_or(0.5, |score| score.value() as f32);

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
    clock: Option<Arc<dyn Clock>>,
    engine_config: EngineConfig,
    // `fn() -> _` keeps the builder `Send + Sync` regardless of `K` and `P`.
    _types: PhantomData<fn() -> (K, P)>,
}

impl<K, P> fmt::Debug for DtacCacheBuilder<K, P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DtacCacheBuilder")
            .field("capacity", &self.capacity)
            .field("router", &self.router)
            .field("clock", &self.clock)
            .field("engine_config", &self.engine_config)
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

    /// Makes both the router and the engine read time from `clock`.
    ///
    /// Without this, the engine uses the router's clock (the system clock unless the
    /// router was given another with [`HeuristicRouter::with_clock`]).
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Runs the cache engine with `config` instead of [`EngineConfig::default`].
    ///
    /// Fails with [`DTACError::ConfigError`](crate::error::DTACError::ConfigError) if
    /// `config` does not pass [`EngineConfig::validate`].
    pub fn engine_config(mut self, config: EngineConfig) -> Result<Self> {
        config.validate()?;
        self.engine_config = config;
        Ok(self)
    }

    /// Builds the cache. The router and the engine share one clock.
    pub fn build(self) -> DtacCache<K, P> {
        let mut router = self.router.unwrap_or_default();
        let clock = match self.clock {
            Some(clock) => {
                router = router.with_clock(clock.clone());
                clock
            }
            None => router.clock().clone(),
        };
        DtacCache::from_parts(
            DtacCacheEngine::build(self.capacity, self.engine_config, clock),
            router,
        )
    }
}
