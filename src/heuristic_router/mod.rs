//! Module for assigning AI-score based on the DTAC heuristics.
//!
//! This module contains the `HeuristicRouter` which evaluates incoming
//! requests and applies various heuristics to determine a caching and routing score.

mod ip_heuristic;
/// Module defining the structure and logic for AI scores.
pub mod score;

use crate::types::RequestData;
use crate::error::Result;
use score::AiScore;
use tracing::{debug, instrument};

/// The main router holding internal state across requests.
///
/// It uses a combination of heuristics and a base weight to determine
/// the final score for a given request.
#[derive(Debug)]
pub struct HeuristicRouter {
    /// The base weight applied to calculated heuristic scores.
    base_weight: f64,
}

impl HeuristicRouter {
    /// Creates a new `HeuristicRouter` with the specified base weight.
    ///
    /// # Arguments
    ///
    /// * `base_weight` - The base multiplier to apply to heuristic scores.
    pub fn new(base_weight: f64) -> Self {
        Self { base_weight }
    }

    /// Evaluates a request and assigns an AI score.
    ///
    /// This method analyzes the request asynchronously and applies
    /// internal heuristic submodules to produce a final `AiScore`.
    ///
    /// # Arguments
    ///
    /// * `req` - A reference to the `RequestData` to evaluate.
    ///
    /// # Returns
    ///
    /// Returns an `Ok(AiScore)` representing the computed score.
    #[instrument(skip(self, req))]
    pub async fn evaluate_request(&self, req: &RequestData) -> Result<AiScore> {
        debug!(ip = %req.ip_address, path = %req.path, "Evaluating incoming request");

        // Delegate to private heuristic submodules
        let ip_score = ip_heuristic::analyze_ip(&req.ip_address);

        // Combine scores with internal state
        let final_value = (ip_score) * self.base_weight;

        Ok(AiScore::new(final_value))
    }
}
