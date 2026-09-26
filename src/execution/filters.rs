use crate::domain::{Fixed, Side};
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub struct ExchangeFilters {
    pub min_notional: Fixed,
    pub max_notional: Fixed,
    pub tick_size: Fixed,
    pub quantity_step: Fixed,
    pub min_quantity: Fixed,
}

impl ExchangeFilters {
    pub fn normalize(
        self,
        side: Side,
        price: Fixed,
        quantity: Fixed,
    ) -> Result<(Fixed, Fixed), FilterError> {
        if price.0 <= 0 || quantity.0 <= 0 || self.tick_size.0 <= 0 || self.quantity_step.0 <= 0 {
            return Err(FilterError::InvalidValue);
        }
        // Sell GTX prices are rounded up to avoid becoming taker after rounding.
        let normalized_price = match side {
            Side::Buy => price.floor_to(self.tick_size),
            Side::Sell => {
                let floored = price.floor_to(self.tick_size);
                if floored == price {
                    floored
                } else {
                    floored + self.tick_size
                }
            }
        };
        let normalized_quantity = quantity.floor_to(self.quantity_step);
        if normalized_quantity < self.min_quantity || normalized_quantity.0 <= 0 {
            return Err(FilterError::QuantityTooSmall);
        }
        let notional = normalized_price
            .checked_mul(normalized_quantity)
            .ok_or(FilterError::Overflow)?;
        if notional < self.min_notional {
            return Err(FilterError::NotionalTooSmall);
        }
        if self.max_notional.0 > 0 && notional > self.max_notional {
            return Err(FilterError::NotionalTooLarge);
        }
        Ok((normalized_price, normalized_quantity))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MarginGuard {
    pub leverage: u32,
    pub fee_buffer_fraction: f64,
}

impl MarginGuard {
    /// Validates isolated margin without using leverage to inflate the risk
    /// budget. Leverage only reduces the collateral locked by the position.
    #[must_use]
    pub fn permits(self, notional: Fixed, available_balance: Fixed) -> bool {
        if self.leverage == 0 || notional.0 <= 0 || available_balance.0 <= 0 {
            return false;
        }
        let required = notional.as_f64() / f64::from(self.leverage)
            + notional.as_f64() * self.fee_buffer_fraction.max(0.0);
        required <= available_balance.as_f64()
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum FilterError {
    #[error("invalid price, quantity or exchange step")]
    InvalidValue,
    #[error("quantity is below LOT_SIZE")]
    QuantityTooSmall,
    #[error("notional is below MIN_NOTIONAL")]
    NotionalTooSmall,
    #[error("notional exceeds configured market-impact ceiling")]
    NotionalTooLarge,
    #[error("fixed-point overflow")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_margin_keeps_fee_reserve() {
        let guard = MarginGuard {
            leverage: 50,
            fee_buffer_fraction: 0.001,
        };
        assert!(guard.permits(Fixed::from_f64(1_000.0), Fixed::from_f64(25.0)));
        assert!(!guard.permits(Fixed::from_f64(1_000.0), Fixed::from_f64(20.0)));
    }

    #[test]
    fn sell_price_rounds_away_from_taker_side() {
        let filters = ExchangeFilters {
            min_notional: Fixed::from_f64(5.0),
            max_notional: Fixed::from_f64(100_000.0),
            tick_size: Fixed::from_f64(0.1),
            quantity_step: Fixed::from_f64(0.001),
            min_quantity: Fixed::from_f64(0.001),
        };
        let (price, quantity) = filters
            .normalize(
                Side::Sell,
                Fixed::from_f64(100.09),
                Fixed::from_f64(1.0009),
            )
            .expect("normalize");
        assert_eq!(price, Fixed::from_f64(100.1));
        assert_eq!(quantity, Fixed::from_f64(1.0));
    }
}
