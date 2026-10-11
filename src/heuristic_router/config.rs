//! Tunable constants of the heuristic router's log-odds model.

use crate::error::{DTACError, Result};
use serde::{Deserialize, Serialize};

/// Configuration of the router's log-odds scoring model.
///
/// A request's bot score is `sigmoid(l_0 + Σ contributions)`, where `l_0` is derived from
/// [`prior_bot_rate`](Self::prior_bot_rate) and each heuristic that fires adds its weight.
///
/// Deserializes from JSON; missing fields take their [`Default`] values, so a config file
/// only has to list the constants it changes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    /// Assumed base rate of bot traffic before any evidence, in `(0, 1)`.
    ///
    /// The baseline log-odds is `l_0 = ln(p / (1 − p))`. The default of `0.1`
    /// (`l_0 ≈ −2.20`) means "assume human until the evidence says otherwise": a clean
    /// human request scores `bot_prob ≈ 0.1`. A value of `0.5` gives `l_0 = 0`, the
    /// router's original behaviour.
    pub prior_bot_rate: f64,
    /// Log-odds added when the client IP belongs to a datacenter ASN (Heuristic B).
    pub w_asn: f64,
    /// Log-odds added when the IP never fetches auxiliary assets — "Shy Guys" (Heuristic C).
    pub w_fav: f64,
    /// Log-odds added when the `Accept-Language` header is missing (Heuristic D).
    pub w_hdr: f64,
    /// Per-IP request velocity (EWMA, requests) above which the velocity penalty applies.
    pub burst_threshold: f64,
    /// Scale of the graded velocity penalty `alpha · ln(velocity / burst_threshold)`.
    pub alpha: f64,
    /// Page requests an IP may make without ever fetching an auxiliary asset before the
    /// "Shy Guys" heuristic fires.
    ///
    /// This is a grace period: a short human visit that never pulls the favicon (for
    /// example because the browser already cached it) must not be penalized.
    pub shy_page_threshold: u16,
    /// Length of one velocity tick, in milliseconds.
    ///
    /// The per-IP velocity EWMA is halved (right-shifted) once per tick boundary
    /// crossed since the IP's previous request. Ticks are aligned to the clock's epoch,
    /// so with the default of 1000 a request at 0.9 s and one at 1.1 s are one tick apart.
    pub velocity_tick_ms: u64,
    /// Number of elapsed ticks after which the velocity resets to a single request
    /// instead of being decayed.
    pub velocity_reset_ticks: u64,
    /// Half-life, in seconds, of the per-IP request counts kept by the bounded IP
    /// tracker (`SpaceSaver`). `0` disables decay.
    pub space_saver_half_life_s: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            prior_bot_rate: 0.1,
            w_asn: 1.5,
            w_fav: 1.2,
            w_hdr: 1.0,
            burst_threshold: 10.0,
            alpha: 2.0,
            shy_page_threshold: 5,
            velocity_tick_ms: 1000,
            velocity_reset_ticks: 10,
            space_saver_half_life_s: 3600,
        }
    }
}

impl RouterConfig {
    /// Baseline prior log-odds `l_0 = ln(p / (1 − p))` for `p = prior_bot_rate`.
    pub fn prior_log_odds(&self) -> f64 {
        let p = self.prior_bot_rate;
        (p / (1.0 - p)).ln()
    }

    /// Checks that every constant is in its valid range.
    pub fn validate(&self) -> Result<()> {
        let invalid = |msg: String| Err(DTACError::ConfigError(msg));

        if !(self.prior_bot_rate > 0.0 && self.prior_bot_rate < 1.0) {
            return invalid(format!(
                "prior_bot_rate must be in (0, 1), got {}",
                self.prior_bot_rate
            ));
        }
        for (name, weight) in [
            ("w_asn", self.w_asn),
            ("w_fav", self.w_fav),
            ("w_hdr", self.w_hdr),
            ("alpha", self.alpha),
        ] {
            if !weight.is_finite() {
                return invalid(format!("{name} must be finite, got {weight}"));
            }
        }
        if !(self.burst_threshold.is_finite() && self.burst_threshold > 0.0) {
            return invalid(format!(
                "burst_threshold must be positive, got {}",
                self.burst_threshold
            ));
        }
        if self.shy_page_threshold == 0 {
            return invalid("shy_page_threshold must be at least 1".to_string());
        }
        if self.velocity_tick_ms == 0 {
            return invalid("velocity_tick_ms must be at least 1".to_string());
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
        RouterConfig::default().validate().unwrap();
    }

    #[test]
    fn prior_log_odds_matches_rate() {
        let even = RouterConfig {
            prior_bot_rate: 0.5,
            ..Default::default()
        };
        assert!(even.prior_log_odds().abs() < 1e-12);
        let low = RouterConfig::default();
        assert!((low.prior_log_odds() - (1.0_f64 / 9.0).ln()).abs() < 1e-12);
    }

    #[test]
    fn rejects_out_of_range_values() {
        for bad in [
            RouterConfig {
                prior_bot_rate: 0.0,
                ..Default::default()
            },
            RouterConfig {
                prior_bot_rate: 1.0,
                ..Default::default()
            },
            RouterConfig {
                prior_bot_rate: f64::NAN,
                ..Default::default()
            },
            RouterConfig {
                w_asn: f64::INFINITY,
                ..Default::default()
            },
            RouterConfig {
                burst_threshold: 0.0,
                ..Default::default()
            },
            RouterConfig {
                shy_page_threshold: 0,
                ..Default::default()
            },
            RouterConfig {
                velocity_tick_ms: 0,
                ..Default::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn partial_json_fills_in_defaults() {
        let cfg: RouterConfig =
            serde_json::from_str(r#"{"prior_bot_rate": 0.3, "w_hdr": 2.5}"#).unwrap();
        assert_eq!(
            cfg,
            RouterConfig {
                prior_bot_rate: 0.3,
                w_hdr: 2.5,
                ..Default::default()
            }
        );
    }

    #[test]
    fn unknown_json_fields_are_rejected() {
        assert!(serde_json::from_str::<RouterConfig>(r#"{"w_typo": 1.0}"#).is_err());
    }
}
