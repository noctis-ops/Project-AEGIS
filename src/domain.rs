use arrayvec::ArrayVec;
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    hash::Hash,
    ops::{Add, Sub},
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

/// Fixed-point scale used for prices, quantities and USDT values.
pub const FIXED_SCALE: i64 = 100_000_000;
pub const MAX_BOOK_UPDATES: usize = 1_024;

/// An allocation-free signed decimal with eight fractional digits.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fixed(pub i64);

impl Fixed {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(FIXED_SCALE);

    #[must_use]
    pub const fn from_raw(raw: i64) -> Self {
        Self(raw)
    }

    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        Self((value * FIXED_SCALE as f64).round() as i64)
    }

    #[must_use]
    pub fn as_f64(self) -> f64 {
        self.0 as f64 / FIXED_SCALE as f64
    }

    #[must_use]
    pub fn abs(self) -> Self {
        Self(self.0.saturating_abs())
    }

    #[must_use]
    pub fn checked_mul(self, rhs: Self) -> Option<Self> {
        let raw = i128::from(self.0)
            .checked_mul(i128::from(rhs.0))?
            .checked_div(i128::from(FIXED_SCALE))?;
        i64::try_from(raw).ok().map(Self)
    }

    #[must_use]
    pub fn checked_div(self, rhs: Self) -> Option<Self> {
        if rhs.0 == 0 {
            return None;
        }
        let raw = i128::from(self.0)
            .checked_mul(i128::from(FIXED_SCALE))?
            .checked_div(i128::from(rhs.0))?;
        i64::try_from(raw).ok().map(Self)
    }

    /// Floors a positive number to an exchange tick/step.
    #[must_use]
    pub fn floor_to(self, step: Self) -> Self {
        if step.0 <= 0 {
            return self;
        }
        Self(self.0.div_euclid(step.0) * step.0)
    }

    #[must_use]
    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }
}

impl Add for Fixed {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Fixed {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let negative = self.0 < 0;
        let raw = i128::from(self.0).abs();
        let whole = raw / i128::from(FIXED_SCALE);
        let fraction = raw % i128::from(FIXED_SCALE);
        if negative {
            f.write_str("-")?;
        }
        if fraction == 0 {
            write!(f, "{whole}")
        } else {
            let mut fractional = format!("{fraction:08}");
            while fractional.ends_with('0') {
                fractional.pop();
            }
            write!(f, "{whole}.{fractional}")
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FixedParseError {
    #[error("empty decimal")]
    Empty,
    #[error("invalid decimal")]
    Invalid,
    #[error("more than eight fractional digits")]
    Precision,
    #[error("decimal overflow")]
    Overflow,
}

impl FromStr for Fixed {
    type Err = FixedParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() {
            return Err(FixedParseError::Empty);
        }
        let (negative, unsigned) = match input.as_bytes()[0] {
            b'-' => (true, &input[1..]),
            b'+' => (false, &input[1..]),
            _ => (false, input),
        };
        if unsigned.is_empty() {
            return Err(FixedParseError::Invalid);
        }
        let mut parts = unsigned.split('.');
        let whole = parts.next().ok_or(FixedParseError::Invalid)?;
        let fraction = parts.next().unwrap_or("");
        if parts.next().is_some()
            || whole.is_empty()
            || !whole.bytes().all(|c| c.is_ascii_digit())
            || !fraction.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(FixedParseError::Invalid);
        }
        if fraction.len() > 8 {
            return Err(FixedParseError::Precision);
        }
        let whole_value = whole
            .parse::<i128>()
            .map_err(|_| FixedParseError::Overflow)?;
        let fractional_value = if fraction.is_empty() {
            0_i128
        } else {
            let parsed = fraction
                .parse::<i128>()
                .map_err(|_| FixedParseError::Invalid)?;
            parsed * 10_i128.pow((8 - fraction.len()) as u32)
        };
        let mut raw = whole_value
            .checked_mul(i128::from(FIXED_SCALE))
            .and_then(|v| v.checked_add(fractional_value))
            .ok_or(FixedParseError::Overflow)?;
        if negative {
            raw = -raw;
        }
        i64::try_from(raw)
            .map(Self)
            .map_err(|_| FixedParseError::Overflow)
    }
}

/// Inline ASCII symbol. Binance USD-M symbols fit in sixteen bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Symbol {
    len: u8,
    bytes: [u8; 16],
}

impl Symbol {
    pub fn new(value: &str) -> Result<Self, SymbolError> {
        if value.is_empty() || value.len() > 16 || !value.is_ascii() {
            return Err(SymbolError);
        }
        let mut bytes = [0; 16];
        for (target, source) in bytes.iter_mut().zip(value.bytes()) {
            *target = source.to_ascii_uppercase();
        }
        Ok(Self {
            len: value.len() as u8,
            bytes,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        // Construction enforces ASCII, which is valid UTF-8.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }

    #[must_use]
    pub fn lowercase(&self) -> String {
        self.as_str().to_ascii_lowercase()
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Symbol {
    type Err = SymbolError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("symbol must be 1-16 ASCII characters")]
pub struct SymbolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Self::Buy => Self::Sell,
            Self::Sell => Self::Buy,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Level {
    pub price: Fixed,
    pub quantity: Fixed,
}

impl Level {
    pub const EMPTY: Self = Self {
        price: Fixed::ZERO,
        quantity: Fixed::ZERO,
    };
}

pub type LevelUpdate = Level;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookDelta {
    pub symbol: Symbol,
    pub event_time_ns: u64,
    pub first_update_id: u64,
    pub final_update_id: u64,
    pub previous_final_update_id: u64,
    pub bids: ArrayVec<LevelUpdate, MAX_BOOK_UPDATES>,
    pub asks: ArrayVec<LevelUpdate, MAX_BOOK_UPDATES>,
}

#[derive(Debug, Clone)]
pub struct BookSnapshot {
    pub symbol: Symbol,
    pub last_update_id: u64,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggTrade {
    pub symbol: Symbol,
    pub event_time_ns: u64,
    pub trade_id: u64,
    pub price: Fixed,
    pub quantity: Fixed,
    /// True means the buyer was resting; this was a taker sell.
    pub buyer_is_maker: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkPrice {
    pub symbol: Symbol,
    pub event_time_ns: u64,
    pub price: Fixed,
    pub funding_rate: Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForceOrder {
    pub symbol: Symbol,
    pub event_time_ns: u64,
    pub side: Side,
    pub price: Fixed,
    pub quantity: Fixed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketEvent {
    Depth(BookDelta),
    Trade(AggTrade),
    MarkPrice(MarkPrice),
    ForceOrder(ForceOrder),
    MarketDataHalt { reason: &'static str, at_ns: u64 },
}

impl MarketEvent {
    #[must_use]
    pub const fn timestamp_ns(&self) -> u64 {
        match self {
            Self::Depth(v) => v.event_time_ns,
            Self::Trade(v) => v.event_time_ns,
            Self::MarkPrice(v) => v.event_time_ns,
            Self::ForceOrder(v) => v.event_time_ns,
            Self::MarketDataHalt { at_ns, .. } => *at_ns,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookTop<const N: usize> {
    pub symbol: Symbol,
    pub timestamp_ns: u64,
    pub bids: [Level; N],
    pub asks: [Level; N],
    pub bid_count: usize,
    pub ask_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeIntent {
    pub symbol: Symbol,
    pub side: Side,
    pub price: Fixed,
    pub quantity: Fixed,
    pub stop_loss: Fixed,
    pub take_profit: Fixed,
    pub created_at_ns: u64,
    pub expires_at_ns: u64,
    /// Expected net edge after pessimistic fees/slippage, expressed in USDT.
    pub expected_net_value: Fixed,
}

#[must_use]
pub fn unix_time_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_decimal_is_exact() {
        let value = "123.45000001".parse::<Fixed>().expect("valid fixed");
        assert_eq!(value.0, 12_345_000_001);
        assert_eq!(value.to_string(), "123.45000001");
        assert_eq!("-0.5".parse::<Fixed>().expect("valid fixed").0, -50_000_000);
    }

    #[test]
    fn fixed_rejects_excess_precision() {
        assert_eq!(
            "1.000000001".parse::<Fixed>(),
            Err(FixedParseError::Precision)
        );
    }

    #[test]
    fn symbol_is_inline_and_normalized() {
        let symbol = Symbol::new("btcusdt").expect("valid symbol");
        assert_eq!(symbol.as_str(), "BTCUSDT");
    }
}
