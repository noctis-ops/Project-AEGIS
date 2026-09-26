//! Layer 5: deterministic replay, pessimistic execution and shadow trading.

mod backtest;
mod data_lake;
mod shadow;
mod walk_forward;

pub use backtest::{BacktestEngine, BacktestError, BacktestReport};
pub use data_lake::{DataFeed, HistoricalDataLake, HistoricalRecord};
pub use shadow::{ShadowConfig, ShadowExecutionEngine, ShadowReport};
pub use walk_forward::{chronological_split, RobustnessResult};
