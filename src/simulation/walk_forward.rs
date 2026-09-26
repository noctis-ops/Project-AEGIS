/// Chronological in-sample/out-of-sample split. The function never shuffles
/// time-series data, preserving causality.
#[must_use]
pub fn chronological_split<T>(values: &[T], in_sample_fraction: f64) -> (&[T], &[T]) {
    let fraction = in_sample_fraction.clamp(0.0, 1.0);
    let split = ((values.len() as f64) * fraction).floor() as usize;
    values.split_at(split.min(values.len()))
}

#[derive(Debug, Clone, Copy)]
pub struct RobustnessResult {
    pub baseline_return: f64,
    pub lower_neighbor_return: f64,
    pub upper_neighbor_return: f64,
}

impl RobustnessResult {
    /// A sharp one-point optimum is rejected even when its baseline backtest is
    /// profitable. Both neighboring parameter values must retain positive EV.
    #[must_use]
    pub fn is_stable(self) -> bool {
        self.baseline_return > 0.0
            && self.lower_neighbor_return > 0.0
            && self.upper_neighbor_return > 0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_is_strictly_chronological() {
        let values: Vec<u8> = (0..10).collect();
        let (training, blind) = chronological_split(&values, 0.7);
        assert_eq!(training, &[0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(blind, &[7, 8, 9]);
    }

    #[test]
    fn rejects_parameter_cliff() {
        assert!(!RobustnessResult {
            baseline_return: 0.20,
            lower_neighbor_return: -0.01,
            upper_neighbor_return: 0.01,
        }
        .is_stable());
    }
}
