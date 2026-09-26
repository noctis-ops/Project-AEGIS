//! Project AEGIS: a five-layer, safety-first market microstructure engine.
//!
//! Live order placement is compile-time gated by `live-trading` and runtime
//! gated by an explicit acknowledgement. Shadow mode is the default.

pub mod alpha;
pub mod c2;
pub mod data;
pub mod domain;
pub mod execution;
pub mod risk;
pub mod simulation;
pub mod telemetry;

pub use domain::{Fixed, Side, Symbol};
