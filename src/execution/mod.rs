//! Layer 3: order routing, lifecycle management and Binance reconciliation.

mod filters;
mod live;
mod manager;
mod rate_limit;
mod slicing;
mod types;
mod user_stream;

pub use filters::{ExchangeFilters, FilterError, MarginGuard};
pub use live::{BinanceLiveExecution, LiveCredentials};
pub use manager::{ManagedOrder, OrderManager, OrderManagerConfig, OrderManagerError};
pub use rate_limit::{RateLimitError, RequestClass, TokenBucket};
pub use slicing::OrderSlicer;
pub use types::{
    ClientOrderId, ExecutionMode, ExecutionUpdate, ExecutionVenue, Fill, OrderAck, OrderKind,
    OrderRequest, OrderStatus, Position, TimeInForce, VenueError,
};
pub use user_stream::{BinanceUserStream, UserDataEvent};
