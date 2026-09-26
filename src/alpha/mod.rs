//! Layer 2: incremental market-microstructure features and alpha FSM.

mod engine;
mod features;
mod sizing;

pub use engine::{AlphaConfig, AlphaEngine, AlphaState, SignalContext};
pub use features::{liquidity_void_ticks, order_book_imbalance, FlowToxicity, OpenInterestDelta};
pub use sizing::{CapitalSizer, SizingError};
