//! Tunable constants of the cache engine.

use super::{
    GHOST_CAPACITY_MULTIPLIER, MIN_ENTRIES_PER_SHARD, N_SAMPLES, NUM_SHARDS, PROMOTION_THRESHOLD,
};
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
    /// Ghost-cache capacity per shard, as a multiple of the main-cache capacity per shard.
    pub ghost_multiplier: usize,
    /// How many independently locked shards the capacity is split across.
    pub shards: Shards,
}

/// The number of lock shards a cache splits its capacity across.
///
/// In JSON, `"auto"` or a positive integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(from = "ShardsRepr", into = "ShardsRepr")]
pub enum Shards {
    /// [`auto_shard_count`] of the capacity: [`NUM_SHARDS`] for large caches, fewer for
    /// small ones so each shard keeps at least [`MIN_ENTRIES_PER_SHARD`] entries.
    #[default]
    Auto,
    /// Exactly this many shards.
    Fixed(usize),
}

impl Shards {
    /// The shard count for a cache of `capacity` entries.
    pub const fn resolve(self, capacity: usize) -> usize {
        match self {
            Self::Auto => auto_shard_count(capacity),
            Self::Fixed(n) => n,
        }
    }
}

/// The largest power of two up to [`NUM_SHARDS`] that leaves at least
/// [`MIN_ENTRIES_PER_SHARD`] entries per shard, and at least 1.
///
/// Tiny shards distort per-shard policies: a 5-sample tournament over 4 entries is an
/// exhaustive search, and a 10% queue of 4 entries rounds to nothing. Capacities of
/// `NUM_SHARDS * MIN_ENTRIES_PER_SHARD` (16 384) and up get all [`NUM_SHARDS`].
pub const fn auto_shard_count(capacity: usize) -> usize {
    let mut shards = NUM_SHARDS;
    while shards > 1 && capacity / shards < MIN_ENTRIES_PER_SHARD {
        shards /= 2;
    }
    shards
}

/// The JSON form of [`Shards`]: `"auto"` or a number.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
enum ShardsRepr {
    Fixed(usize),
    Named(ShardsName),
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ShardsName {
    Auto,
}

impl From<ShardsRepr> for Shards {
    fn from(repr: ShardsRepr) -> Self {
        match repr {
            ShardsRepr::Fixed(n) => Self::Fixed(n),
            ShardsRepr::Named(ShardsName::Auto) => Self::Auto,
        }
    }
}

impl From<Shards> for ShardsRepr {
    fn from(shards: Shards) -> Self {
        match shards {
            Shards::Auto => Self::Named(ShardsName::Auto),
            Shards::Fixed(n) => Self::Fixed(n),
        }
    }
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            decay_half_life_ms: 1000,
            decay_cutoff_half_lives: 32,
            promotion_threshold: PROMOTION_THRESHOLD,
            n_samples: N_SAMPLES,
            ghost_multiplier: GHOST_CAPACITY_MULTIPLIER,
            shards: Shards::Auto,
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
        if self.ghost_multiplier == 0 {
            return invalid("ghost_multiplier must be at least 1".to_string());
        }
        if self.shards == Shards::Fixed(0) {
            return invalid("shards must be \"auto\" or at least 1".to_string());
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
            EngineConfig {
                ghost_multiplier: 0,
                ..Default::default()
            },
            EngineConfig {
                shards: Shards::Fixed(0),
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
    fn shards_are_auto_or_a_number_in_json() {
        let auto: EngineConfig = serde_json::from_str(r#"{"shards": "auto"}"#).unwrap();
        assert_eq!(auto.shards, Shards::Auto);
        let fixed: EngineConfig = serde_json::from_str(r#"{"shards": 16}"#).unwrap();
        assert_eq!(fixed.shards, Shards::Fixed(16));
        assert!(serde_json::from_str::<EngineConfig>(r#"{"shards": "many"}"#).is_err());

        let json = serde_json::to_value(EngineConfig::default()).unwrap();
        assert_eq!(json["shards"], "auto");
        let back: EngineConfig = serde_json::from_value(json).unwrap();
        assert_eq!(back, EngineConfig::default());
    }

    #[test]
    fn auto_shards_keep_at_least_64_entries_per_shard() {
        assert_eq!(auto_shard_count(1_000), 8);
        assert_eq!(auto_shard_count(5_000), 64);
        assert_eq!(auto_shard_count(10_000), 128);
        // The library's original 256 shards from 16 384 entries up.
        assert_eq!(auto_shard_count(16_383), 128);
        assert_eq!(auto_shard_count(16_384), NUM_SHARDS);
        assert_eq!(auto_shard_count(1_000_000), NUM_SHARDS);
        assert_eq!(auto_shard_count(10), 1);
        assert_eq!(auto_shard_count(0), 1);
        for capacity in [64, 100, 1_000, 5_000, 10_000, 20_000] {
            let shards = auto_shard_count(capacity);
            assert!(shards.is_power_of_two());
            assert!(capacity / shards >= MIN_ENTRIES_PER_SHARD, "{capacity}");
        }
    }

    #[test]
    fn unknown_json_fields_are_rejected() {
        assert!(serde_json::from_str::<EngineConfig>(r#"{"half_life": 1}"#).is_err());
    }
}
