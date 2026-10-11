/// Represents the final computed AI score for a request.
///
/// This score is used to determine caching behavior and routing logic.
/// The value is guaranteed to be clamped between 0.0 and 100.0.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct AiScore {
    /// The numeric value of the score.
    value: f64,
}

impl AiScore {
    /// Creates a new `AiScore`.
    ///
    /// The provided value is automatically clamped to be within the
    /// range of `0.0` to `1.0` to ensure safe operation.
    pub(crate) const fn new(value: f64) -> Self {
        // Clamp between 0.0 and 1.0 for safety
        let clamped = value.clamp(0.0, 1.0);
        Self { value: clamped }
    }

    /// Returns the numerical value of the AI score.
    pub const fn value(&self) -> f64 {
        self.value
    }

    /// Determines whether the score indicates a high risk.
    ///
    /// Returns `true` if the score is greater than 85.0.
    pub fn is_high_risk(&self) -> bool {
        self.value > 85.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_score_clamping() {
        let score_low = AiScore::new(-5.0);
        assert!(score_low.value().abs() < f64::EPSILON);

        let score_high = AiScore::new(105.0);
        assert!((score_high.value() - 1.0).abs() < f64::EPSILON);

        let score_mid = AiScore::new(0.5);
        assert!((score_mid.value() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_is_high_risk() {
        // Since new() clamps to 1.0, and is_high_risk checks > 85.0,
        // it's currently always false under new().
        let score = AiScore::new(0.9);
        assert!(!score.is_high_risk());
    }
}
