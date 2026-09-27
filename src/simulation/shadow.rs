use crate::{
    domain::{AggTrade, BookTop, Fixed, Level, Side, Symbol},
    execution::{
        ClientOrderId, ExecutionMode, ExecutionUpdate, ExecutionVenue, Fill, OrderAck, OrderKind,
        OrderRequest, OrderStatus, Position, TimeInForce, VenueError,
    },
};
use async_trait::async_trait;
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};

#[derive(Debug, Clone)]
pub struct ShadowConfig {
    pub starting_equity: Fixed,
    pub maker_fee_bps: f64,
    pub taker_fee_bps: f64,
    pub queue_multiplier: f64,
    pub min_latency_ns: u64,
    pub max_latency_ns: u64,
    /// A single simulated order can consume at most this fraction of a trade.
    pub max_trade_participation: f64,
    pub fallback_queue: Fixed,
    pub random_seed: u64,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        Self {
            starting_equity: Fixed::from_f64(1_000.0),
            maker_fee_bps: 2.0,
            taker_fee_bps: 5.0,
            queue_multiplier: 1.0,
            min_latency_ns: 2_000_000,
            max_latency_ns: 10_000_000,
            max_trade_participation: 0.10,
            fallback_queue: Fixed::from_f64(1.0),
            random_seed: 0xa3e1_5eed_cafe_babe,
        }
    }
}

#[derive(Debug, Clone)]
struct ShadowOrder {
    request: OrderRequest,
    status: OrderStatus,
    activation_ns: u64,
    queue_ahead: Fixed,
    filled: Fixed,
}

#[derive(Debug, Clone, Copy, Default)]
struct VirtualPosition {
    signed_quantity: f64,
    average_price: f64,
    entry_fees: f64,
}

#[derive(Debug)]
struct ShadowState {
    config: ShadowConfig,
    books: HashMap<Symbol, BookTop<5>>,
    orders: HashMap<ClientOrderId, ShadowOrder>,
    updates: VecDeque<ExecutionUpdate>,
    realized_outcomes: VecDeque<Fixed>,
    positions: HashMap<Symbol, VirtualPosition>,
    equity: f64,
    peak_equity: f64,
    max_drawdown: f64,
    realized_pnl: f64,
    total_fees: f64,
    fills: u64,
    maker_fills: u64,
    rng: u64,
}

impl ShadowState {
    fn jitter_ns(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        let width = self
            .config
            .max_latency_ns
            .saturating_sub(self.config.min_latency_ns);
        self.config.min_latency_ns + self.rng % width.saturating_add(1)
    }

    fn queue_at(&self, request: &OrderRequest) -> Fixed {
        let Some(book) = self.books.get(&request.symbol) else {
            return self.config.fallback_queue;
        };
        let levels = match request.side {
            Side::Buy => &book.bids[..book.bid_count],
            Side::Sell => &book.asks[..book.ask_count],
        };
        request.price.map_or(self.config.fallback_queue, |price| {
            levels
                .iter()
                .find(|level| level.price == price)
                .map_or(self.config.fallback_queue, |level| {
                    Fixed::from_f64(level.quantity.as_f64() * self.config.queue_multiplier)
                })
        })
    }

    fn would_cross(&self, request: &OrderRequest) -> bool {
        let Some(book) = self.books.get(&request.symbol) else {
            return false;
        };
        let Some(price) = request.price else {
            return false;
        };
        match request.side {
            Side::Buy => book.ask_count > 0 && price >= book.asks[0].price,
            Side::Sell => book.bid_count > 0 && price <= book.bids[0].price,
        }
    }

    fn record_fill(&mut self, fill: Fill) {
        let fee = fill.fee.as_f64();
        let quantity = fill.quantity.as_f64();
        let price = fill.price.as_f64();
        let delta = if fill.side == Side::Buy {
            quantity
        } else {
            -quantity
        };
        let position = self.positions.entry(fill.symbol).or_default();
        let old_quantity = position.signed_quantity;
        let same_direction = old_quantity == 0.0 || old_quantity.signum() == delta.signum();
        let mut realized = 0.0;
        let mut closed_outcome = None;
        if same_direction {
            let total = old_quantity.abs() + delta.abs();
            position.average_price = if total <= f64::EPSILON {
                0.0
            } else {
                (position.average_price * old_quantity.abs() + price * delta.abs()) / total
            };
            position.signed_quantity += delta;
            position.entry_fees += fee;
        } else {
            let closed = old_quantity.abs().min(delta.abs());
            realized = if old_quantity > 0.0 {
                (price - position.average_price) * closed
            } else {
                (position.average_price - price) * closed
            };
            let entry_fee_share = if old_quantity.abs() <= f64::EPSILON {
                0.0
            } else {
                position.entry_fees * closed / old_quantity.abs()
            };
            let exit_fee_share = if delta.abs() <= f64::EPSILON {
                0.0
            } else {
                fee * closed / delta.abs()
            };
            position.entry_fees = (position.entry_fees - entry_fee_share).max(0.0);
            closed_outcome = Some(realized - entry_fee_share - exit_fee_share);
            position.signed_quantity += delta;
            if position.signed_quantity.abs() <= f64::EPSILON {
                *position = VirtualPosition::default();
            } else if position.signed_quantity.signum() == delta.signum() {
                position.average_price = price;
                position.entry_fees = (fee - exit_fee_share).max(0.0);
            }
        }
        if let Some(outcome) = closed_outcome {
            self.realized_outcomes.push_back(Fixed::from_f64(outcome));
        }
        self.realized_pnl += realized;
        self.total_fees += fee;
        self.equity += realized - fee;
        self.peak_equity = self.peak_equity.max(self.equity);
        if self.peak_equity > 0.0 {
            self.max_drawdown = self
                .max_drawdown
                .max((self.peak_equity - self.equity) / self.peak_equity);
        }
        self.fills += 1;
        if fill.maker {
            self.maker_fills += 1;
        }
    }

    fn best_level(&self, symbol: Symbol, side: Side) -> Option<Level> {
        let book = self.books.get(&symbol)?;
        match side {
            Side::Buy => book.asks.first().copied().filter(|_| book.ask_count > 0),
            Side::Sell => book.bids.first().copied().filter(|_| book.bid_count > 0),
        }
    }
}

pub struct ShadowExecutionEngine {
    state: Mutex<ShadowState>,
}

impl ShadowExecutionEngine {
    #[must_use]
    pub fn new(config: ShadowConfig) -> Self {
        assert!(config.starting_equity.0 > 0);
        assert!((0.0..=1.0).contains(&config.max_trade_participation));
        let starting = config.starting_equity.as_f64();
        let seed = config.random_seed;
        Self {
            state: Mutex::new(ShadowState {
                config,
                books: HashMap::new(),
                orders: HashMap::new(),
                updates: VecDeque::new(),
                realized_outcomes: VecDeque::new(),
                positions: HashMap::new(),
                equity: starting,
                peak_equity: starting,
                max_drawdown: 0.0,
                realized_pnl: 0.0,
                total_fees: 0.0,
                fills: 0,
                maker_fills: 0,
                rng: seed,
            }),
        }
    }

    pub fn on_book(&self, book: BookTop<5>) {
        self.state
            .lock()
            .expect("shadow state")
            .books
            .insert(book.symbol, book);
    }

    /// Applies real tick volume to virtual queue positions. Merely touching a
    /// limit never fills it; volume ahead must first be depleted.
    pub fn on_trade(&self, trade: &AggTrade) -> Vec<ExecutionUpdate> {
        let mut state = self.state.lock().expect("shadow state");
        let ids: Vec<ClientOrderId> = state.orders.keys().copied().collect();
        for id in ids {
            let participation = state.config.max_trade_participation;
            let maker_fee_bps = state.config.maker_fee_bps;
            let taker_fee_bps = state.config.taker_fee_bps;
            let mut produced_fill = None;
            let mut produced_update = None;
            {
                let Some(order) = state.orders.get_mut(&id) else {
                    continue;
                };
                if order.request.symbol != trade.symbol
                    || order.status.terminal()
                    || trade.event_time_ns < order.activation_ns
                {
                    continue;
                }
                let trigger_hit = match order.request.kind {
                    OrderKind::StopMarket => order.request.trigger_price.is_some_and(|trigger| {
                        match order.request.side {
                            Side::Sell => trade.price <= trigger,
                            Side::Buy => trade.price >= trigger,
                        }
                    }),
                    OrderKind::TakeProfitMarket => {
                        order.request.trigger_price.is_some_and(|trigger| {
                            match order.request.side {
                                Side::Sell => trade.price >= trigger,
                                Side::Buy => trade.price <= trigger,
                            }
                        })
                    }
                    _ => false,
                };
                let resting_hit = order.request.kind == OrderKind::Limit
                    && order.request.time_in_force == TimeInForce::Gtx
                    && order
                        .request
                        .price
                        .is_some_and(|limit| match order.request.side {
                            Side::Buy => trade.buyer_is_maker && trade.price <= limit,
                            Side::Sell => !trade.buyer_is_maker && trade.price >= limit,
                        });
                if !trigger_hit && !resting_hit {
                    continue;
                }
                let available =
                    Fixed::from_f64(trade.quantity.as_f64() * participation.clamp(0.0, 1.0));
                let maker = resting_hit;
                let mut executable = available;
                if maker && order.queue_ahead.0 > 0 {
                    let consumed = order.queue_ahead.min(executable);
                    order.queue_ahead = order.queue_ahead - consumed;
                    executable = executable - consumed;
                }
                let remaining = order.request.quantity - order.filled;
                let fill_quantity = executable.min(remaining);
                if fill_quantity.0 <= 0 {
                    continue;
                }
                order.filled = order.filled + fill_quantity;
                order.status = if order.filled >= order.request.quantity {
                    OrderStatus::Filled
                } else {
                    OrderStatus::PartiallyFilled
                };
                let fill_price = if maker {
                    order.request.price.unwrap_or(trade.price)
                } else {
                    trade.price
                };
                let fee_bps = if maker { maker_fee_bps } else { taker_fee_bps };
                let fee = Fixed::from_f64(
                    fill_price.as_f64() * fill_quantity.as_f64() * fee_bps / 10_000.0,
                );
                let fill = Fill {
                    client_id: id,
                    symbol: trade.symbol,
                    side: order.request.side,
                    quantity: fill_quantity,
                    price: fill_price,
                    fee,
                    timestamp_ns: trade.event_time_ns,
                    maker,
                };
                produced_update = Some(ExecutionUpdate {
                    client_id: id,
                    status: order.status,
                    cumulative_filled: order.filled,
                    last_fill: Some(fill),
                    realized_pnl: None,
                    timestamp_ns: trade.event_time_ns,
                });
                produced_fill = Some(fill);
            }
            if let Some(fill) = produced_fill {
                state.record_fill(fill);
            }
            if let Some(update) = produced_update {
                state.updates.push_back(update);
            }
        }
        state.updates.drain(..).collect()
    }

    pub fn drain_realized_outcomes(&self) -> Vec<Fixed> {
        self.state
            .lock()
            .expect("shadow state")
            .realized_outcomes
            .drain(..)
            .collect()
    }

    #[must_use]
    pub fn report(&self) -> ShadowReport {
        let state = self.state.lock().expect("shadow state");
        ShadowReport {
            starting_equity: state.config.starting_equity,
            ending_equity: Fixed::from_f64(state.equity),
            realized_pnl: Fixed::from_f64(state.realized_pnl),
            total_fees: Fixed::from_f64(state.total_fees),
            max_drawdown: state.max_drawdown,
            fills: state.fills,
            maker_fills: state.maker_fills,
            open_positions: state
                .positions
                .values()
                .filter(|position| position.signed_quantity.abs() > f64::EPSILON)
                .count(),
        }
    }

    fn immediate_fill(state: &mut ShadowState, request: &OrderRequest) -> Result<(), VenueError> {
        let level = state
            .best_level(request.symbol, request.side)
            .ok_or_else(|| VenueError::Rejected {
                code: -1,
                message: "no visible liquidity".to_owned(),
            })?;
        if request.kind == OrderKind::Limit {
            let limit = request
                .price
                .ok_or_else(|| VenueError::InvalidResponse("IOC price".to_owned()))?;
            let outside_limit = match request.side {
                Side::Buy => level.price > limit,
                Side::Sell => level.price < limit,
            };
            if outside_limit {
                return Err(VenueError::Rejected {
                    code: -5022,
                    message: "IOC liquidity outside slippage limit".to_owned(),
                });
            }
        }
        let visible = level.quantity.as_f64().max(f64::EPSILON);
        let participation = request.quantity.as_f64() / visible;
        let impact_bps = (participation * 2.0).min(20.0);
        let price = match request.side {
            Side::Buy => Fixed::from_f64(level.price.as_f64() * (1.0 + impact_bps / 10_000.0)),
            Side::Sell => Fixed::from_f64(level.price.as_f64() * (1.0 - impact_bps / 10_000.0)),
        };
        let fee = Fixed::from_f64(
            price.as_f64() * request.quantity.as_f64() * state.config.taker_fee_bps / 10_000.0,
        );
        let fill = Fill {
            client_id: request.client_id,
            symbol: request.symbol,
            side: request.side,
            quantity: request.quantity,
            price,
            fee,
            timestamp_ns: request.created_at_ns,
            maker: false,
        };
        state.record_fill(fill);
        state.updates.push_back(ExecutionUpdate {
            client_id: request.client_id,
            status: OrderStatus::Filled,
            cumulative_filled: request.quantity,
            last_fill: Some(fill),
            realized_pnl: None,
            timestamp_ns: request.created_at_ns,
        });
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ShadowReport {
    pub starting_equity: Fixed,
    pub ending_equity: Fixed,
    pub realized_pnl: Fixed,
    pub total_fees: Fixed,
    pub max_drawdown: f64,
    pub fills: u64,
    pub maker_fills: u64,
    pub open_positions: usize,
}

#[async_trait]
impl ExecutionVenue for ShadowExecutionEngine {
    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Shadow
    }

    async fn submit(&self, request: OrderRequest) -> Result<OrderAck, VenueError> {
        let mut state = self.state.lock().expect("shadow state");
        if state.orders.contains_key(&request.client_id) {
            return Err(VenueError::Rejected {
                code: -4116,
                message: "duplicate clientOrderId".to_owned(),
            });
        }
        if request.kind == OrderKind::Limit
            && request.time_in_force == TimeInForce::Gtx
            && state.would_cross(&request)
        {
            return Err(VenueError::Rejected {
                code: -5022,
                message: "Post-Only order would take liquidity".to_owned(),
            });
        }
        if request.kind == OrderKind::Market || request.time_in_force == TimeInForce::Ioc {
            Self::immediate_fill(&mut state, &request)?;
            return Ok(OrderAck {
                client_id: request.client_id,
                venue_order_id: None,
                status: OrderStatus::Filled,
                accepted_at_ns: request.created_at_ns,
            });
        }
        let queue_ahead = state.queue_at(&request);
        let activation_ns = request.created_at_ns.saturating_add(state.jitter_ns());
        state.orders.insert(
            request.client_id,
            ShadowOrder {
                request: request.clone(),
                status: OrderStatus::New,
                activation_ns,
                queue_ahead,
                filled: Fixed::ZERO,
            },
        );
        Ok(OrderAck {
            client_id: request.client_id,
            venue_order_id: None,
            status: OrderStatus::New,
            accepted_at_ns: request.created_at_ns,
        })
    }

    async fn cancel(
        &self,
        _symbol: Symbol,
        client_id: ClientOrderId,
    ) -> Result<OrderAck, VenueError> {
        let mut state = self.state.lock().expect("shadow state");
        let order = state
            .orders
            .get_mut(&client_id)
            .ok_or_else(|| VenueError::Rejected {
                code: -2011,
                message: "unknown order".to_owned(),
            })?;
        order.status = OrderStatus::Canceled;
        Ok(OrderAck {
            client_id,
            venue_order_id: None,
            status: OrderStatus::Canceled,
            accepted_at_ns: order.request.created_at_ns,
        })
    }

    async fn cancel_all(&self, symbol: Symbol) -> Result<(), VenueError> {
        for order in self
            .state
            .lock()
            .expect("shadow state")
            .orders
            .values_mut()
            .filter(|order| order.request.symbol == symbol && !order.status.terminal())
        {
            order.status = OrderStatus::Canceled;
        }
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>, VenueError> {
        Ok(self
            .state
            .lock()
            .expect("shadow state")
            .positions
            .iter()
            .filter(|(_, position)| position.signed_quantity.abs() > f64::EPSILON)
            .map(|(symbol, position)| Position {
                symbol: *symbol,
                signed_quantity: Fixed::from_f64(position.signed_quantity),
                entry_price: Fixed::from_f64(position.average_price),
                unrealized_pnl: Fixed::ZERO,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionVenue;

    fn book(symbol: Symbol) -> BookTop<5> {
        let mut bids = [Level::EMPTY; 5];
        let mut asks = [Level::EMPTY; 5];
        bids[0] = Level {
            price: Fixed::from_f64(100.0),
            quantity: Fixed::from_f64(10.0),
        };
        asks[0] = Level {
            price: Fixed::from_f64(100.1),
            quantity: Fixed::from_f64(10.0),
        };
        BookTop {
            symbol,
            timestamp_ns: 1,
            bids,
            asks,
            bid_count: 1,
            ask_count: 1,
        }
    }

    #[tokio::test]
    async fn touch_does_not_create_phantom_fill() {
        let symbol = Symbol::new("BTCUSDT").expect("symbol");
        let engine = ShadowExecutionEngine::new(ShadowConfig {
            min_latency_ns: 0,
            max_latency_ns: 0,
            max_trade_participation: 1.0,
            ..ShadowConfig::default()
        });
        engine.on_book(book(symbol));
        let request = OrderRequest {
            client_id: ClientOrderId::generate(1),
            symbol,
            side: Side::Buy,
            kind: OrderKind::Limit,
            time_in_force: TimeInForce::Gtx,
            quantity: Fixed::from_f64(1.0),
            price: Some(Fixed::from_f64(100.0)),
            trigger_price: None,
            reduce_only: false,
            created_at_ns: 1,
        };
        engine.submit(request).await.expect("submit");
        let first = engine.on_trade(&AggTrade {
            symbol,
            event_time_ns: 2,
            trade_id: 1,
            price: Fixed::from_f64(100.0),
            quantity: Fixed::from_f64(9.0),
            buyer_is_maker: true,
        });
        assert!(first.is_empty(), "queue ahead was not depleted");
        let second = engine.on_trade(&AggTrade {
            symbol,
            event_time_ns: 3,
            trade_id: 2,
            price: Fixed::from_f64(100.0),
            quantity: Fixed::from_f64(2.0),
            buyer_is_maker: true,
        });
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].cumulative_filled, Fixed::from_f64(1.0));
    }

    #[tokio::test]
    async fn latency_prevents_look_ahead_fill() {
        let symbol = Symbol::new("ETHUSDT").expect("symbol");
        let engine = ShadowExecutionEngine::new(ShadowConfig {
            min_latency_ns: 10,
            max_latency_ns: 10,
            fallback_queue: Fixed::ZERO,
            max_trade_participation: 1.0,
            ..ShadowConfig::default()
        });
        engine.on_book(book(symbol));
        engine
            .submit(OrderRequest {
                client_id: ClientOrderId::generate(100),
                symbol,
                side: Side::Buy,
                kind: OrderKind::Limit,
                time_in_force: TimeInForce::Gtx,
                quantity: Fixed::from_f64(1.0),
                price: Some(Fixed::from_f64(99.9)),
                trigger_price: None,
                reduce_only: false,
                created_at_ns: 100,
            })
            .await
            .expect("submit");
        assert!(engine
            .on_trade(&AggTrade {
                symbol,
                event_time_ns: 105,
                trade_id: 1,
                price: Fixed::from_f64(99.9),
                quantity: Fixed::from_f64(100.0),
                buyer_is_maker: true,
            })
            .is_empty());
    }
}
