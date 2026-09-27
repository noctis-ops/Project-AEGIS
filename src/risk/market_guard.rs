use crate::domain::{Fixed, Side};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub struct FundingGuard {
    pub extreme_negative: f64,
    pub extreme_positive: f64,
    pub required_edge_multiple: f64,
}

impl Default for FundingGuard {
    fn default() -> Self {
        Self {
            extreme_negative: -0.0005,
            extreme_positive: 0.0005,
            required_edge_multiple: 3.0,
        }
    }
}

impl FundingGuard {
    #[must_use]
    pub fn permits(self, side: Side, funding_rate: f64, expected_edge: f64) -> bool {
        let adverse_cost = match side {
            Side::Buy if funding_rate < self.extreme_negative => funding_rate.abs(),
            Side::Sell if funding_rate > self.extreme_positive => funding_rate.abs(),
            _ => return true,
        };
        expected_edge >= adverse_cost * self.required_edge_multiple
    }
}

#[derive(Debug)]
pub struct VolatilityDetector {
    fast_window: usize,
    baseline_window: usize,
    returns: VecDeque<f64>,
    previous_price: Option<f64>,
}

impl VolatilityDetector {
    #[must_use]
    pub fn new(fast_window: usize, baseline_window: usize) -> Self {
        assert!(fast_window >= 2 && baseline_window > fast_window);
        Self {
            fast_window,
            baseline_window,
            returns: VecDeque::with_capacity(baseline_window),
            previous_price: None,
        }
    }

    pub fn update(&mut self, price: Fixed) -> f64 {
        let price = price.as_f64();
        if let Some(previous) = self.previous_price {
            if previous > 0.0 {
                if self.returns.len() == self.baseline_window {
                    self.returns.pop_front();
                }
                self.returns.push_back((price / previous).ln());
            }
        }
        self.previous_price = Some(price);
        self.multiple()
    }

    #[must_use]
    pub fn multiple(&self) -> f64 {
        if self.returns.len() < self.fast_window * 2 {
            return 1.0;
        }
        let values: Vec<f64> = self.returns.iter().copied().collect();
        let fast = standard_deviation(&values[values.len() - self.fast_window..]);
        let baseline = standard_deviation(&values);
        if baseline <= f64::EPSILON {
            1.0
        } else {
            fast / baseline
        }
    }
}

fn standard_deviation(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    variance.sqrt()
}

#[derive(Debug, Default)]
pub struct TimeWeightedStop {
    breached_at_ns: Option<u64>,
}

impl TimeWeightedStop {
    /// Returns true when the hard software exit is confirmed. Server-side hard
    /// stops remain installed independently as the final disaster boundary.
    pub fn update(
        &mut self,
        side: Side,
        price: Fixed,
        stop: Fixed,
        timestamp_ns: u64,
        volume_spike: bool,
        high_liquidity_market: bool,
    ) -> bool {
        let breached = match side {
            Side::Buy => price <= stop,
            Side::Sell => price >= stop,
        };
        if !breached {
            self.breached_at_ns = None;
            return false;
        }
        if volume_spike || !high_liquidity_market {
            return true;
        }
        let started = *self.breached_at_ns.get_or_insert(timestamp_ns);
        timestamp_ns.saturating_sub(started) >= 200_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wick_must_persist_without_volume_spike() {
        let mut stop = TimeWeightedStop::default();
        let level = Fixed::from_f64(99.0);
        assert!(!stop.update(Side::Buy, Fixed::from_f64(98.9), level, 1_000, false, true,));
        assert!(stop.update(
            Side::Buy,
            Fixed::from_f64(98.8),
            level,
            200_001_000,
            false,
            true,
        ));
    }
}
