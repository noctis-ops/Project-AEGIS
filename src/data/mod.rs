//! Layer 1: market transport, SIMD parsing, lock-free buses and the local L2 book.

mod bus;
mod lob;
mod parser;
mod transport;

pub use bus::{spsc_bus, LatestEventBus, SpscConsumer, SpscProducer};
pub use lob::{BookError, LobSynchronizer, LocalOrderBook, SyncOutcome, SyncState};
pub use parser::{parse_market_message, ParseError};
pub use transport::{BinanceMarketDataClient, TransportConfig};
