use super::{
    liquidity_void_ticks, order_book_imbalance, CapitalSizer, FlowToxicity, OpenInterestDelta,
};
use crate::domain::{AggTrade, BookTop, Fixed, Side, Symbol, TradeIntent};

#[derive(Debug, Clone)]
pub struct AlphaConfig {
    pub obi_threshold: f64,
    pub toxic_pressure_threshold: f64,
    pub minimum_score: f64,
    pub tick_size: Fixed,
    pub stop_ticks: i64,
    pub target_ticks: i64,
    pub maker_fee_bps: f64,
    pub emergency_exit_fee_bps: f64,
    pub estimated_slippage_bps: f64,
    pub ttl_ns: u64,
    pub volume_bucket: Fixed,
    pub volume_history: usize,
}

impl Default for AlphaConfig {
    fn default() -> Self {
        Self {
            obi_threshold: 0.40,
            toxic_pressure_threshold: 0.80,
            minimum_score: 0.55,
            tick_size: Fixed::from_f64(0.1),
            stop_ticks: 8,
            target_ticks: 12,
            maker_fee_bps: 2.0,
            emergency_exit_fee_bps: 5.0,
            estimated_slippage_bps: 2.0,
            ttl_ns: 200_000_000,
            volume_bucket: Fixed::from_f64(10.0),
            volume_history: 20,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaState {
    Idle,
    Armed { side: Side, since_ns: u64 },
    Executing { side: Side, intent_expires_ns: u64 },
    Managing { side: Side },
}

#[derive(Debug, Clone, Copy)]
pub struct SignalContext {
    pub obi: f64,
    pub vpin: f64,
    pub signed_flow: f64,
    pub bid_void_ticks: u64,
    pub ask_void_ticks: u64,
    pub oi_delta: Fixed,
    pub score: f64,
}

pub struct AlphaEngine {
    symbol: Symbol,
    config: AlphaConfig,
    sizer: CapitalSizer,
    toxicity: FlowToxicity,
    open_interest: OpenInterestDelta,
    state: AlphaState,
    last_context: Option<SignalContext>,
}

impl AlphaEngine {
    #[must_use]
    pub fn new(symbol: Symbol, config: AlphaConfig, sizer: CapitalSizer) -> Self {
        let toxicity = FlowToxicity::new(config.volume_bucket, config.volume_history);
        Self {
            symbol,
            config,
            sizer,
            toxicity,
            open_interest: OpenInterestDelta::default(),
            state: AlphaState::Idle,
            last_context: None,
        }
    }

    pub fn on_trade(&mut self, trade: &AggTrade) {
        if trade.symbol == self.symbol {
            self.toxicity.on_trade(trade);
        }
    }

    pub fn on_open_interest(&mut self, symbol: Symbol, value: Fixed) {
        if symbol == self.symbol {
            self.open_interest.update(value);
        }
    }

    /// Evaluates one causally available book state. A setup must survive two
    /// consecutive evaluations: Idle -> Armed -> Executing.
    pub fn on_book<const N: usize>(
        &mut self,
        book: &BookTop<N>,
        live_equity: Fixed,
    ) -> Option<TradeIntent> {
        if book.symbol != self.symbol || book.bid_count == 0 || book.ask_count == 0 {
            return None;
        }
        let obi = order_book_imbalance(book, 5);
        let signed_flow = self.toxicity.signed_pressure();
        let vpin = self.toxicity.vpin();
        let (bid_void_ticks, ask_void_ticks) = liquidity_void_ticks(book, self.config.tick_size);
        let oi_delta = self.open_interest.value();
        let side = if obi >= self.config.obi_threshold {
            Some(Side::Buy)
        } else if obi <= -self.config.obi_threshold {
            Some(Side::Sell)
        } else {
            None
        };

        let score = side.map_or(0.0, |direction| {
            let obi_component = obi.abs().min(1.0) * 0.55;
            let flow_alignment = match direction {
                Side::Buy => signed_flow,
                Side::Sell => -signed_flow,
            }
            .clamp(-1.0, 1.0);
            let flow_component = ((flow_alignment + 1.0) / 2.0) * 0.25;
            let void_component = match direction {
                Side::Buy if ask_void_ticks > bid_void_ticks => 0.10,
                Side::Sell if bid_void_ticks > ask_void_ticks => 0.10,
                _ => 0.0,
            };
            let oi_component = if oi_delta.0 > 0 { 0.10 } else { 0.0 };
            obi_component + flow_component + void_component + oi_component
        });
        self.last_context = Some(SignalContext {
            obi,
            vpin,
            signed_flow,
            bid_void_ticks,
            ask_void_ticks,
            oi_delta,
            score,
        });

        let Some(side) = side else {
            self.state = AlphaState::Idle;
            return None;
        };
        let adverse_toxicity = match side {
            Side::Buy => signed_flow <= -self.config.toxic_pressure_threshold,
            Side::Sell => signed_flow >= self.config.toxic_pressure_threshold,
        };
        if adverse_toxicity || score < self.config.minimum_score {
            self.state = AlphaState::Idle;
            return None;
        }

        match self.state {
            AlphaState::Idle => {
                self.state = AlphaState::Armed {
                    side,
                    since_ns: book.timestamp_ns,
                };
                None
            }
            AlphaState::Armed { side: armed, .. } if armed == side => {
                let intent = self.build_intent(book, side, live_equity)?;
                self.state = AlphaState::Executing {
                    side,
                    intent_expires_ns: intent.expires_at_ns,
                };
                Some(intent)
            }
            AlphaState::Armed { .. } => {
                self.state = AlphaState::Armed {
                    side,
                    since_ns: book.timestamp_ns,
                };
                None
            }
            AlphaState::Executing {
                intent_expires_ns, ..
            } if book.timestamp_ns > intent_expires_ns => {
                self.state = AlphaState::Idle;
                None
            }
            AlphaState::Executing { .. } | AlphaState::Managing { .. } => None,
        }
    }

    fn build_intent<const N: usize>(
        &self,
        book: &BookTop<N>,
        side: Side,
        equity: Fixed,
    ) -> Option<TradeIntent> {
        // One tick behind top-of-book reduces immediate adverse selection.
        let entry = match side {
            Side::Buy => book.bids[0].price - self.config.tick_size,
            Side::Sell => book.asks[0].price + self.config.tick_size,
        };
        let stop_distance = Fixed::from_raw(
            self.config
                .tick_size
                .0
                .saturating_mul(self.config.stop_ticks),
        );
        let target_distance = Fixed::from_raw(
            self.config
                .tick_size
                .0
                .saturating_mul(self.config.target_ticks),
        );
        let (stop_loss, take_profit) = match side {
            Side::Buy => (entry - stop_distance, entry + target_distance),
            Side::Sell => (entry + stop_distance, entry - target_distance),
        };
        let quantity = self.sizer.size(equity, entry, stop_loss).ok()?;
        let notional = entry.checked_mul(quantity)?;
        let gross = target_distance.checked_mul(quantity)?;
        let cost_fraction = (self.config.maker_fee_bps
            + self.config.emergency_exit_fee_bps
            + self.config.estimated_slippage_bps)
            / 10_000.0;
        let costs = Fixed::from_f64(notional.as_f64() * cost_fraction);
        let expected_net_value = gross - costs;
        if expected_net_value.0 <= 0 {
            return None;
        }
        Some(TradeIntent {
            symbol: self.symbol,
            side,
            price: entry,
            quantity,
            stop_loss,
            take_profit,
            created_at_ns: book.timestamp_ns,
            expires_at_ns: book.timestamp_ns.saturating_add(self.config.ttl_ns),
            expected_net_value,
        })
    }

    pub fn set_risk_fraction(&mut self, fraction: f64) -> bool {
        if !(0.0..=0.05).contains(&fraction) || fraction == 0.0 {
            return false;
        }
        self.sizer.risk_fraction = fraction;
        true
    }

    pub fn mark_filled(&mut self, side: Side) {
        self.state = AlphaState::Managing { side };
    }

    pub fn mark_flat(&mut self) {
        self.state = AlphaState::Idle;
    }

    #[must_use]
    pub const fn state(&self) -> AlphaState {
        self.state
    }

    #[must_use]
    pub const fn last_context(&self) -> Option<SignalContext> {
        self.last_context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Level;

    #[test]
    fn signal_requires_persistence_and_positive_net_value() {
        let symbol = Symbol::new("BTCUSDT").expect("symbol");
        let config = AlphaConfig {
            minimum_score: 0.3,
            ..AlphaConfig::default()
        };
        let sizer = CapitalSizer {
            risk_fraction: 0.01,
            min_notional: Fixed::from_f64(5.0),
            max_notional: Fixed::from_f64(100_000.0),
            quantity_step: Fixed::from_f64(0.001),
        };
        let mut engine = AlphaEngine::new(symbol, config, sizer);
        let mut bids = [Level::EMPTY; 5];
        let mut asks = [Level::EMPTY; 5];
        for index in 0..5 {
            bids[index] = Level {
                price: Fixed::from_f64(100.0 - index as f64 * 0.1),
                quantity: Fixed::from_f64(10.0),
            };
            asks[index] = Level {
                price: Fixed::from_f64(100.1 + index as f64 * 0.1),
                quantity: Fixed::from_f64(1.0),
            };
        }
        let mut book = BookTop {
            symbol,
            timestamp_ns: 1_000,
            bids,
            asks,
            bid_count: 5,
            ask_count: 5,
        };
        assert!(engine
            .on_book(&book, Fixed::from_f64(1_000.0))
            .is_none());
        book.timestamp_ns += 1;
        let intent = engine
            .on_book(&book, Fixed::from_f64(1_000.0))
            .expect("persistent signal");
        assert_eq!(intent.side, Side::Buy);
        assert!(intent.expected_net_value.0 > 0);
    }
}
