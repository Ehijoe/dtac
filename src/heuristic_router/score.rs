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
    pub(crate) fn new(value: f64) -> Self {
        // Clamp between 0.0 and 1.0 for safety
        let clamped = value.clamp(0.0, 1.0);
        Self { value: clamped }
    }

    /// Returns the numerical value of the AI score.
    pub fn value(&self) -> f64 {
        self.value
    }
    
    /// Determines whether the score indicates a high risk.
    ///
    /// Returns `true` if the score is greater than 85.0.
    pub fn is_high_risk(&self) -> bool {
        self.value > 85.0
    }
}
