//! Tunable constants of the cache engine.

use super::{N_SAMPLES, PROMOTION_THRESHOLD};
use crate::error::{DTACError, Result};
use serde::{Deserialize, Serialize};

/// Configuration of the cache engine's score decay, promotion and eviction.
///
/// The defaults reproduce the engine's original hard-coded behaviour. They are
/// provisional: on log-timestamp replays they decay scores far too quickly, and plan
/// 004 calibrates them.
///
/// Deserializes from JSON; missing fields take their [`Default`] values, so a config file
/// only has to list the constants it changes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    /// Time, in milliseconds, for a resource's score to halve without accesses.
    ///
    /// Decay is stepwise: the score is halved once per *whole* half-life elapsed since
    /// the last access.
    pub decay_half_life_ms: u64,
    /// Number of elapsed half-lives after which a score is treated as exactly 0.
    pub decay_cutoff_half_lives: u32,
    /// Score a ghost-cache entry must reach to be promoted into the main cache.
    pub promotion_threshold: f32,
    /// Number of random entries sampled in each eviction tournament.
    pub n_samples: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            decay_half_life_ms: 1000,
            decay_cutoff_half_lives: 32,
            promotion_threshold: PROMOTION_THRESHOLD,
            n_samples: N_SAMPLES,
        }
    }
}

impl EngineConfig {
    /// Checks that every constant is in its valid range.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(DTACError::ConfigError(msg));

        if self.decay_half_life_ms == 0 {
            return invalid("decay_half_life_ms must be at least 1".to_string());
        }
        if !self.promotion_threshold.is_finite() {
            return invalid(format!(
                "promotion_threshold must be finite, got {}",
                self.promotion_threshold
            ));
        }
        if self.n_samples == 0 {
            return invalid("n_samples must be at least 1".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn default_is_valid() {
        EngineConfig::default().validate().unwrap();
    }

    #[test]
    fn rejects_out_of_range_values() {
        for bad in [
            EngineConfig {
                decay_half_life_ms: 0,
                ..Default::default()
            },
            EngineConfig {
                promotion_threshold: f32::NAN,
                ..Default::default()
            },
            EngineConfig {
                n_samples: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn partial_json_fills_in_defaults() {
        let cfg: EngineConfig =
            serde_json::from_str(r#"{"decay_half_life_ms": 10000, "n_samples": 14}"#).unwrap();
        assert_eq!(
            cfg,
            EngineConfig {
                decay_half_life_ms: 10_000,
                n_samples: 14,
                ..Default::default()
            }
        );
    }

    #[test]
    fn unknown_json_fields_are_rejected() {
        assert!(serde_json::from_str::<EngineConfig>(r#"{"half_life": 1}"#).is_err());
    }
}
