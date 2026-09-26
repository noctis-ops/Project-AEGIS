use crate::domain::Fixed;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub struct CapitalSizer {
    /// Fraction of live equity at risk, e.g. 0.01 = 1%.
    pub risk_fraction: f64,
    pub min_notional: Fixed,
    pub max_notional: Fixed,
    pub quantity_step: Fixed,
}

impl CapitalSizer {
    pub fn size(
        self,
        live_equity: Fixed,
        entry_price: Fixed,
        stop_price: Fixed,
    ) -> Result<Fixed, SizingError> {
        if live_equity.0 <= 0 || entry_price.0 <= 0 {
            return Err(SizingError::NoCapital);
        }
        if !(0.0..=0.05).contains(&self.risk_fraction) || self.risk_fraction == 0.0 {
            return Err(SizingError::InvalidRisk);
        }
        let stop_distance = (entry_price - stop_price).abs();
        if stop_distance.0 == 0 {
            return Err(SizingError::ZeroStopDistance);
        }
        let risk_amount = Fixed::from_f64(live_equity.as_f64() * self.risk_fraction);
        let raw_quantity = risk_amount
            .checked_div(stop_distance)
            .ok_or(SizingError::Overflow)?;
        let quantity = raw_quantity.floor_to(self.quantity_step);
        if quantity.0 <= 0 {
            return Err(SizingError::CapitalTooSmall);
        }
        let notional = quantity
            .checked_mul(entry_price)
            .ok_or(SizingError::Overflow)?;
        if notional < self.min_notional {
            return Err(SizingError::CapitalTooSmall);
        }
        if self.max_notional.0 > 0 && notional > self.max_notional {
            let capped = self
                .max_notional
                .checked_div(entry_price)
                .ok_or(SizingError::Overflow)?
                .floor_to(self.quantity_step);
            if capped.0 <= 0 {
                return Err(SizingError::CapitalTooSmall);
            }
            return Ok(capped);
        }
        Ok(quantity)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SizingError {
    #[error("no available capital")]
    NoCapital,
    #[error("risk fraction must be in (0, 5%]")]
    InvalidRisk,
    #[error("stop distance is zero")]
    ZeroStopDistance,
    #[error("capital is too small for exchange constraints and current volatility")]
    CapitalTooSmall,
    #[error("fixed-point sizing overflow")]
    Overflow,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_size_tracks_live_equity() {
        let sizer = CapitalSizer {
            risk_fraction: 0.01,
            min_notional: Fixed::from_f64(5.0),
            max_notional: Fixed::from_f64(1_000_000.0),
            quantity_step: Fixed::from_f64(0.001),
        };
        let first = sizer
            .size(
                Fixed::from_f64(1_000.0),
                Fixed::from_f64(100.0),
                Fixed::from_f64(99.0),
            )
            .expect("sizing");
        let after_drawdown = sizer
            .size(
                Fixed::from_f64(900.0),
                Fixed::from_f64(100.0),
                Fixed::from_f64(99.0),
            )
            .expect("sizing");
        assert_eq!(first.as_f64(), 10.0);
        assert_eq!(after_drawdown.as_f64(), 9.0);
    }
}
