use crate::domain::{AggTrade, BookTop, Fixed};
use std::collections::VecDeque;

#[must_use]
pub fn order_book_imbalance<const N: usize>(book: &BookTop<N>, levels: usize) -> f64 {
    let bid_count = book.bid_count.min(levels).min(N);
    let ask_count = book.ask_count.min(levels).min(N);
    let bid_volume: f64 = book.bids[..bid_count]
        .iter()
        .map(|level| level.quantity.as_f64())
        .sum();
    let ask_volume: f64 = book.asks[..ask_count]
        .iter()
        .map(|level| level.quantity.as_f64())
        .sum();
    let total = bid_volume + ask_volume;
    if total <= f64::EPSILON {
        0.0
    } else {
        (bid_volume - ask_volume) / total
    }
}

/// Returns the largest adjacent price gap in exchange ticks.
#[must_use]
pub fn liquidity_void_ticks<const N: usize>(
    book: &BookTop<N>,
    tick_size: Fixed,
) -> (u64, u64) {
    if tick_size.0 <= 0 {
        return (0, 0);
    }
    let max_gap = |levels: &[crate::domain::Level]| {
        levels
            .windows(2)
            .map(|window| {
                window[0]
                    .price
                    .0
                    .abs_diff(window[1].price.0)
                    / tick_size.0 as u64
            })
            .max()
            .unwrap_or(0)
    };
    (
        max_gap(&book.bids[..book.bid_count.min(N)]),
        max_gap(&book.asks[..book.ask_count.min(N)]),
    )
}

#[derive(Debug, Clone, Copy, Default)]
struct VolumeBucket {
    buy: f64,
    sell: f64,
}

impl VolumeBucket {
    fn total(self) -> f64 {
        self.buy + self.sell
    }

    fn signed_imbalance(self) -> f64 {
        let total = self.total();
        if total <= f64::EPSILON {
            0.0
        } else {
            (self.buy - self.sell) / total
        }
    }
}

/// Volume-clock toxicity tracker. Work per trade is O(1), independent of history.
#[derive(Debug)]
pub struct FlowToxicity {
    target_volume: f64,
    completed_capacity: usize,
    current: VolumeBucket,
    completed: VecDeque<VolumeBucket>,
}

impl FlowToxicity {
    #[must_use]
    pub fn new(target_volume: Fixed, completed_capacity: usize) -> Self {
        assert!(target_volume.0 > 0, "volume bucket must be positive");
        assert!(completed_capacity > 0, "VPIN history must be non-zero");
        Self {
            target_volume: target_volume.as_f64(),
            completed_capacity,
            current: VolumeBucket::default(),
            completed: VecDeque::with_capacity(completed_capacity),
        }
    }

    pub fn on_trade(&mut self, trade: &AggTrade) {
        let mut remaining = trade.quantity.as_f64().max(0.0);
        while remaining > f64::EPSILON {
            let room = (self.target_volume - self.current.total()).max(0.0);
            let accepted = remaining.min(room);
            if trade.buyer_is_maker {
                self.current.sell += accepted;
            } else {
                self.current.buy += accepted;
            }
            remaining -= accepted;
            if self.current.total() + f64::EPSILON >= self.target_volume {
                if self.completed.len() == self.completed_capacity {
                    self.completed.pop_front();
                }
                self.completed.push_back(self.current);
                self.current = VolumeBucket::default();
            }
        }
    }

    /// Positive means taker-buy pressure; negative means taker-sell pressure.
    #[must_use]
    pub fn signed_pressure(&self) -> f64 {
        if self.current.total() > f64::EPSILON {
            self.current.signed_imbalance()
        } else {
            self.completed
                .back()
                .copied()
                .unwrap_or_default()
                .signed_imbalance()
        }
    }

    /// Conventional VPIN-like absolute imbalance over completed volume buckets.
    #[must_use]
    pub fn vpin(&self) -> f64 {
        if self.completed.is_empty() {
            return self.current.signed_imbalance().abs();
        }
        self.completed
            .iter()
            .map(|bucket| bucket.signed_imbalance().abs())
            .sum::<f64>()
            / self.completed.len() as f64
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpenInterestDelta {
    previous: Option<Fixed>,
    delta: Fixed,
}

impl OpenInterestDelta {
    pub fn update(&mut self, current: Fixed) {
        self.delta = self.previous.map_or(Fixed::ZERO, |old| current - old);
        self.previous = Some(current);
    }

    #[must_use]
    pub const fn value(self) -> Fixed {
        self.delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Symbol, FIXED_SCALE};

    #[test]
    fn toxicity_uses_volume_not_time() {
        let symbol = Symbol::new("BTCUSDT").expect("symbol");
        let mut flow = FlowToxicity::new(Fixed::from_f64(10.0), 4);
        flow.on_trade(&AggTrade {
            symbol,
            event_time_ns: 1,
            trade_id: 1,
            price: Fixed::from_f64(100.0),
            quantity: Fixed::from_f64(8.0),
            buyer_is_maker: true,
        });
        flow.on_trade(&AggTrade {
            symbol,
            event_time_ns: 2,
            trade_id: 2,
            price: Fixed::from_f64(100.0),
            quantity: Fixed::from_f64(2.0),
            buyer_is_maker: false,
        });
        assert!((flow.signed_pressure() + 0.6).abs() < 1e-9);
        assert!((flow.vpin() - 0.6).abs() < 1e-9);
        assert_eq!(FIXED_SCALE, 100_000_000);
    }
}
