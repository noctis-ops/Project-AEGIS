//! Layer 4: global risk, hierarchical breakers and recovery protocol.

mod manager;
mod market_guard;

pub use manager::{
    BreakerLevel, RiskAction, RiskConfig, RiskInputs, RiskManager, RiskSnapshot, TradeOutcome,
};
pub use market_guard::{FundingGuard, TimeWeightedStop, VolatilityDetector};
