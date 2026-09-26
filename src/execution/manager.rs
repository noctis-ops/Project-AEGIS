use super::{
    ClientOrderId, ExchangeFilters, ExecutionUpdate, ExecutionVenue, OrderAck, OrderKind,
    OrderRequest, OrderStatus, RequestClass, TimeInForce, TokenBucket, VenueError,
};
use crate::domain::{Fixed, Side, Symbol, TradeIntent};
use dashmap::DashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ManagedOrder {
    pub request: OrderRequest,
    pub status: OrderStatus,
    pub venue_order_id: Option<u64>,
    pub cumulative_filled: Fixed,
    pub stop_loss: Option<Fixed>,
    pub take_profit: Option<Fixed>,
    pub brackets_installed: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct OrderManagerConfig {
    pub filters: ExchangeFilters,
    pub max_exit_slippage_ticks: i64,
}

pub struct OrderManager {
    venue: Arc<dyn ExecutionVenue>,
    config: OrderManagerConfig,
    active: DashMap<ClientOrderId, ManagedOrder>,
    limiter: Mutex<TokenBucket>,
}

impl OrderManager {
    #[must_use]
    pub fn new(
        venue: Arc<dyn ExecutionVenue>,
        config: OrderManagerConfig,
        limiter: TokenBucket,
    ) -> Self {
        Self {
            venue,
            config,
            active: DashMap::new(),
            limiter: Mutex::new(limiter),
        }
    }

    pub async fn submit_intent(
        &self,
        intent: TradeIntent,
        now_ns: u64,
        entries_allowed: bool,
    ) -> Result<OrderAck, OrderManagerError> {
        if !entries_allowed {
            return Err(OrderManagerError::EntriesHalted);
        }
        if now_ns > intent.expires_at_ns {
            return Err(OrderManagerError::IntentExpired);
        }
        if intent.expected_net_value.0 <= 0 {
            return Err(OrderManagerError::NegativeExpectancy);
        }
        let (price, quantity) = self
            .config
            .filters
            .normalize(intent.side, intent.price, intent.quantity)
            .map_err(OrderManagerError::Filter)?;
        self.acquire(1, RequestClass::Entry)?;
        let request = OrderRequest {
            client_id: ClientOrderId::generate(now_ns),
            symbol: intent.symbol,
            side: intent.side,
            kind: OrderKind::Limit,
            time_in_force: TimeInForce::Gtx,
            quantity,
            price: Some(price),
            trigger_price: None,
            reduce_only: false,
            created_at_ns: now_ns,
        };
        self.active.insert(
            request.client_id,
            ManagedOrder {
                request: request.clone(),
                status: OrderStatus::PendingSubmit,
                venue_order_id: None,
                cumulative_filled: Fixed::ZERO,
                stop_loss: Some(intent.stop_loss),
                take_profit: Some(intent.take_profit),
                brackets_installed: false,
            },
        );
        match self.venue.submit(request.clone()).await {
            Ok(ack) => {
                if let Some(mut managed) = self.active.get_mut(&request.client_id) {
                    managed.status = ack.status;
                    managed.venue_order_id = ack.venue_order_id;
                }
                Ok(ack)
            }
            Err(VenueError::Indeterminate) => {
                if let Some(mut managed) = self.active.get_mut(&request.client_id) {
                    managed.status = OrderStatus::Indeterminate;
                }
                Err(OrderManagerError::Venue(VenueError::Indeterminate))
            }
            Err(error) => {
                if let Some(mut managed) = self.active.get_mut(&request.client_id) {
                    managed.status = OrderStatus::Rejected;
                }
                Err(OrderManagerError::Venue(error))
            }
        }
    }

    pub async fn on_execution_update(
        &self,
        update: ExecutionUpdate,
    ) -> Result<(), OrderManagerError> {
        let install_brackets = {
            let mut managed = self
                .active
                .get_mut(&update.client_id)
                .ok_or(OrderManagerError::UnknownOrder)?;
            if !valid_transition(managed.status, update.status) {
                return Err(OrderManagerError::InvalidTransition {
                    from: managed.status,
                    to: update.status,
                });
            }
            managed.status = update.status;
            managed.cumulative_filled = update.cumulative_filled;
            update.status == OrderStatus::Filled
                && !managed.request.reduce_only
                && !managed.brackets_installed
        };
        if install_brackets {
            self.install_brackets(update.client_id, update.timestamp_ns).await?;
        }
        Ok(())
    }

    async fn install_brackets(
        &self,
        entry_id: ClientOrderId,
        now_ns: u64,
    ) -> Result<(), OrderManagerError> {
        let (request, stop_loss, take_profit, filled) = {
            let managed = self
                .active
                .get(&entry_id)
                .ok_or(OrderManagerError::UnknownOrder)?;
            (
                managed.request.clone(),
                managed.stop_loss.ok_or(OrderManagerError::MissingBracket)?,
                managed.take_profit.ok_or(OrderManagerError::MissingBracket)?,
                managed.cumulative_filled,
            )
        };
        if filled.0 <= 0 {
            return Err(OrderManagerError::MissingBracket);
        }
        self.acquire(2, RequestClass::Emergency)?;
        let stop = OrderRequest {
            client_id: ClientOrderId::generate(now_ns),
            symbol: request.symbol,
            side: request.side.opposite(),
            kind: OrderKind::StopMarket,
            time_in_force: TimeInForce::Gtc,
            quantity: filled,
            price: None,
            trigger_price: Some(stop_loss),
            reduce_only: true,
            created_at_ns: now_ns,
        };
        let take_profit_request = OrderRequest {
            client_id: ClientOrderId::generate(now_ns.saturating_add(1)),
            kind: OrderKind::TakeProfitMarket,
            trigger_price: Some(take_profit),
            ..stop.clone()
        };
        let stop_ack = self.venue.submit(stop.clone()).await?;
        self.track_child(stop, stop_ack);
        let take_profit_ack = self.venue.submit(take_profit_request.clone()).await?;
        self.track_child(take_profit_request, take_profit_ack);
        if let Some(mut managed) = self.active.get_mut(&entry_id) {
            managed.brackets_installed = true;
        }
        Ok(())
    }

    fn track_child(&self, request: OrderRequest, ack: OrderAck) {
        self.active.insert(
            request.client_id,
            ManagedOrder {
                request,
                status: ack.status,
                venue_order_id: ack.venue_order_id,
                cumulative_filled: Fixed::ZERO,
                stop_loss: None,
                take_profit: None,
                brackets_installed: true,
            },
        );
    }

    /// Cancels the unfilled remainder and crosses only the filled exposure with
    /// a price-bounded IOC order.
    pub async fn abort_partial(
        &self,
        client_id: ClientOrderId,
        current_price: Fixed,
    ) -> Result<(), OrderManagerError> {
        let managed = self
            .active
            .get(&client_id)
            .map(|entry| entry.value().clone())
            .ok_or(OrderManagerError::UnknownOrder)?;
        if managed.status != OrderStatus::PartiallyFilled || managed.cumulative_filled.0 <= 0 {
            return Ok(());
        }
        self.acquire(2, RequestClass::Emergency)?;
        self.venue
            .cancel(managed.request.symbol, client_id)
            .await?;
        let offset = Fixed::from_raw(
            self.config
                .filters
                .tick_size
                .0
                .saturating_mul(self.config.max_exit_slippage_ticks),
        );
        let exit_side = managed.request.side.opposite();
        let limit_price = match exit_side {
            Side::Buy => current_price + offset,
            Side::Sell => current_price - offset,
        };
        let exit = OrderRequest {
            client_id: ClientOrderId::generate(managed.request.created_at_ns.saturating_add(1)),
            symbol: managed.request.symbol,
            side: exit_side,
            kind: OrderKind::Limit,
            time_in_force: TimeInForce::Ioc,
            quantity: managed.cumulative_filled,
            price: Some(limit_price),
            trigger_price: None,
            reduce_only: true,
            created_at_ns: managed.request.created_at_ns,
        };
        self.submit_tracked_emergency(exit).await?;
        Ok(())
    }

    pub async fn cancel_all(&self, symbol: Symbol) -> Result<(), OrderManagerError> {
        self.acquire(1, RequestClass::Emergency)?;
        self.venue.cancel_all(symbol).await?;
        Ok(())
    }

    /// USER_DATA_STREAM watchdog action: cancel first, fetch ground truth, and
    /// flatten every non-zero venue position without trusting local state.
    pub async fn orphan_protection(
        &self,
        symbol: Symbol,
        now_ns: u64,
    ) -> Result<(), OrderManagerError> {
        self.acquire(1, RequestClass::Emergency)?;
        self.venue.cancel_all(symbol).await?;
        for position in self.venue.positions().await? {
            if position.symbol != symbol || position.signed_quantity.0 == 0 {
                continue;
            }
            let side = if position.signed_quantity.0 > 0 {
                Side::Sell
            } else {
                Side::Buy
            };
            let close = OrderRequest {
                client_id: ClientOrderId::generate(now_ns),
                symbol,
                side,
                kind: OrderKind::Market,
                time_in_force: TimeInForce::Ioc,
                quantity: position.signed_quantity.abs(),
                price: None,
                trigger_price: None,
                reduce_only: true,
                created_at_ns: now_ns,
            };
            self.submit_tracked_emergency(close).await?;
        }
        Ok(())
    }

    async fn submit_tracked_emergency(
        &self,
        request: OrderRequest,
    ) -> Result<OrderAck, OrderManagerError> {
        self.active.insert(
            request.client_id,
            ManagedOrder {
                request: request.clone(),
                status: OrderStatus::PendingSubmit,
                venue_order_id: None,
                cumulative_filled: Fixed::ZERO,
                stop_loss: None,
                take_profit: None,
                brackets_installed: true,
            },
        );
        match self.venue.submit(request.clone()).await {
            Ok(ack) => {
                if let Some(mut managed) = self.active.get_mut(&request.client_id) {
                    managed.status = ack.status;
                    managed.venue_order_id = ack.venue_order_id;
                    if ack.status == OrderStatus::Filled {
                        managed.cumulative_filled = request.quantity;
                    }
                }
                Ok(ack)
            }
            Err(error) => {
                if let Some(mut managed) = self.active.get_mut(&request.client_id) {
                    managed.status = if matches!(&error, VenueError::Indeterminate) {
                        OrderStatus::Indeterminate
                    } else {
                        OrderStatus::Rejected
                    };
                }
                Err(OrderManagerError::Venue(error))
            }
        }
    }

    fn acquire(&self, weight: u32, class: RequestClass) -> Result<(), OrderManagerError> {
        self.limiter
            .lock()
            .map_err(|_| OrderManagerError::RateLimiterPoisoned)?
            .acquire(weight, class)
            .map_err(OrderManagerError::RateLimit)
    }

    #[must_use]
    pub fn order(&self, client_id: ClientOrderId) -> Option<ManagedOrder> {
        self.active
            .get(&client_id)
            .map(|entry| entry.value().clone())
    }

    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active
            .iter()
            .filter(|entry| !entry.status.terminal())
            .count()
    }
}

fn valid_transition(from: OrderStatus, to: OrderStatus) -> bool {
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        (OrderStatus::PendingSubmit, OrderStatus::New)
            | (OrderStatus::PendingSubmit, OrderStatus::Rejected)
            | (OrderStatus::PendingSubmit, OrderStatus::Indeterminate)
            | (OrderStatus::New, OrderStatus::PartiallyFilled)
            | (OrderStatus::New, OrderStatus::Filled)
            | (OrderStatus::New, OrderStatus::PendingCancel)
            | (OrderStatus::New, OrderStatus::Canceled)
            | (OrderStatus::New, OrderStatus::Expired)
            | (OrderStatus::PartiallyFilled, OrderStatus::PartiallyFilled)
            | (OrderStatus::PartiallyFilled, OrderStatus::Filled)
            | (OrderStatus::PartiallyFilled, OrderStatus::PendingCancel)
            | (OrderStatus::PartiallyFilled, OrderStatus::Canceled)
            | (OrderStatus::PendingCancel, OrderStatus::Canceled)
            | (OrderStatus::Indeterminate, OrderStatus::New)
            | (OrderStatus::Indeterminate, OrderStatus::PartiallyFilled)
            | (OrderStatus::Indeterminate, OrderStatus::Filled)
            | (OrderStatus::Indeterminate, OrderStatus::Canceled)
            | (OrderStatus::Indeterminate, OrderStatus::Rejected)
    )
}

#[derive(Debug, Error)]
pub enum OrderManagerError {
    #[error("new entries are halted by risk management")]
    EntriesHalted,
    #[error("trade intent TTL expired")]
    IntentExpired,
    #[error("net expected value is not positive")]
    NegativeExpectancy,
    #[error(transparent)]
    Filter(#[from] super::FilterError),
    #[error(transparent)]
    RateLimit(#[from] super::RateLimitError),
    #[error(transparent)]
    Venue(#[from] VenueError),
    #[error("local rate limiter mutex was poisoned")]
    RateLimiterPoisoned,
    #[error("execution update references an unknown order")]
    UnknownOrder,
    #[error("invalid order transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: OrderStatus,
        to: OrderStatus,
    },
    #[error("filled entry has no valid bracket data")]
    MissingBracket,
}
