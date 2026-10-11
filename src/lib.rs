//! Library for the Dynamic Traffic-Aware Cache (DTAC)
//! 
//! Can be used to integrate DTAC into existing applications

/// Contains the heuristic routing logic and router implementation.
pub mod heuristic_router;
/// Contains the randomized caching algorithm considering AI scores.
pub mod cache_engine;
/// Defines the error types and results used across the library.
pub mod error;
/// Defines the common data structures and types used across the library.
pub mod types;
/// Defines the combined DTAC struct.
pub mod dtac_cache;

pub use types::RequestData;
pub use heuristic_router::HeuristicRouter;
pub use heuristic_router::asn_db::AsnDatabase;
pub use heuristic_router::config::RouterConfig;
pub use dtac_cache::{DtacCache, DtacCacheBuilder};
