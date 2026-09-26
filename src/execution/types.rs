use crate::domain::{Fixed, Side, Symbol};
use async_trait::async_trait;
use std::{fmt, sync::atomic::{AtomicU64, Ordering}};
use thiserror::Error;

static ORDER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientOrderId([u8; 32]);

impl ClientOrderId {
    #[must_use]
    pub fn generate(timestamp_ns: u64) -> Self {
        let sequence = ORDER_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let value = (u128::from(timestamp_ns) << 64) | u128::from(sequence);
        let mut output = [b'0'; 32];
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (index, byte) in output.iter_mut().enumerate() {
            let shift = (31 - index) * 4;
            *byte = HEX[((value >> shift) & 0xf) as usize];
        }
        Self(output)
    }

    pub fn parse(value: &str) -> Option<Self> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut output = [0_u8; 32];
        output.copy_from_slice(value.as_bytes());
        Some(Self(output))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        // Constructors accept only ASCII hex.
        std::str::from_utf8(&self.0).unwrap_or("")
    }
}

impl fmt::Display for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for ClientOrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Shadow,
    Live,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderKind {
    Limit,
    Market,
    StopMarket,
    TakeProfitMarket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    Gtx,
    Ioc,
    Gtc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    PendingSubmit,
    New,
    PartiallyFilled,
    Filled,
    PendingCancel,
    Canceled,
    Expired,
    Rejected,
    /// Transport failed after submission; reconciliation is mandatory.
    Indeterminate,
}

impl OrderStatus {
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Filled | Self::Canceled | Self::Expired | Self::Rejected
        )
    }
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub client_id: ClientOrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub kind: OrderKind,
    pub time_in_force: TimeInForce,
    pub quantity: Fixed,
    pub price: Option<Fixed>,
    pub trigger_price: Option<Fixed>,
    pub reduce_only: bool,
    pub created_at_ns: u64,
}

#[derive(Debug, Clone)]
pub struct OrderAck {
    pub client_id: ClientOrderId,
    pub venue_order_id: Option<u64>,
    pub status: OrderStatus,
    pub accepted_at_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Fill {
    pub client_id: ClientOrderId,
    pub symbol: Symbol,
    pub side: Side,
    pub quantity: Fixed,
    pub price: Fixed,
    pub fee: Fixed,
    pub timestamp_ns: u64,
    pub maker: bool,
}

#[derive(Debug, Clone)]
pub struct ExecutionUpdate {
    pub client_id: ClientOrderId,
    pub status: OrderStatus,
    pub cumulative_filled: Fixed,
    pub last_fill: Option<Fill>,
    /// Net realized PnL reported by the venue for a reducing fill.
    pub realized_pnl: Option<Fixed>,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Position {
    pub symbol: Symbol,
    /// Positive is long and negative is short.
    pub signed_quantity: Fixed,
    pub entry_price: Fixed,
    pub unrealized_pnl: Fixed,
}

#[derive(Debug, Error)]
pub enum VenueError {
    #[error("live execution is disabled at compile time")]
    LiveDisabled,
    #[error("live acknowledgement is missing")]
    LiveAcknowledgementMissing,
    #[error("transport failed: {0}")]
    Transport(String),
    #[error("venue rejected request ({code}): {message}")]
    Rejected { code: i64, message: String },
    #[error("response was not received; order state is indeterminate")]
    Indeterminate,
    #[error("invalid venue response: {0}")]
    InvalidResponse(String),
}

#[async_trait]
pub trait ExecutionVenue: Send + Sync {
    fn mode(&self) -> ExecutionMode;

    async fn submit(&self, request: OrderRequest) -> Result<OrderAck, VenueError>;

    async fn cancel(
        &self,
        symbol: Symbol,
        client_id: ClientOrderId,
    ) -> Result<OrderAck, VenueError>;

    async fn cancel_all(&self, symbol: Symbol) -> Result<(), VenueError>;

    async fn positions(&self) -> Result<Vec<Position>, VenueError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_fixed_ascii_and_unique() {
        let first = ClientOrderId::generate(42);
        let second = ClientOrderId::generate(42);
        assert_eq!(first.as_str().len(), 32);
        assert_ne!(first, second);
    }
}
