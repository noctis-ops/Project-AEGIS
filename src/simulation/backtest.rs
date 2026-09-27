use super::{DataFeed, HistoricalRecord, ShadowExecutionEngine, ShadowReport};
use crate::{
    alpha::AlphaEngine,
    data::LocalOrderBook,
    domain::{BookSnapshot, MarketEvent},
    execution::{OrderManager, OrderManagerError},
    risk::{RiskAction, RiskInputs, RiskManager},
};
use std::sync::Arc;
use thiserror::Error;

pub struct BacktestEngine {
    feed: DataFeed,
    alpha: AlphaEngine,
    risk: RiskManager,
    orders: Arc<OrderManager>,
    shadow: Arc<ShadowExecutionEngine>,
    book: LocalOrderBook<2_048>,
    last_timestamp_ns: u64,
    events: u64,
}

impl BacktestEngine {
    pub fn new(
        feed: DataFeed,
        alpha: AlphaEngine,
        risk: RiskManager,
        orders: Arc<OrderManager>,
        shadow: Arc<ShadowExecutionEngine>,
        initial_snapshot: &BookSnapshot,
    ) -> Result<Self, BacktestError> {
        let mut book = LocalOrderBook::new(initial_snapshot.symbol);
        book.install_snapshot(initial_snapshot)?;
        Ok(Self {
            feed,
            alpha,
            risk,
            orders,
            shadow,
            book,
            last_timestamp_ns: 0,
            events: 0,
        })
    }

    pub async fn run(mut self) -> Result<BacktestReport, BacktestError> {
        while let Some(record) = self.feed.next() {
            match record {
                HistoricalRecord::Snapshot { snapshot, .. } => {
                    self.book.install_snapshot(&snapshot)?;
                    self.shadow.on_book(self.book.top::<5>());
                }
                HistoricalRecord::Event(event) => self.process_event(event).await?,
            }
            self.events += 1;
        }
        let shadow = self.shadow.report();
        Ok(BacktestReport {
            shadow,
            events_processed: self.events,
            final_risk: self.risk.snapshot(),
        })
    }

    async fn process_event(&mut self, event: MarketEvent) -> Result<(), BacktestError> {
        let timestamp = event.timestamp_ns();
        if timestamp < self.last_timestamp_ns {
            return Err(BacktestError::LookAhead);
        }
        self.last_timestamp_ns = timestamp;
        match event {
            MarketEvent::Depth(delta) => {
                self.book.apply_delta(&delta)?;
                let top = self.book.top::<5>();
                self.shadow.on_book(top);
                let risk_snapshot = self.risk.snapshot();
                let _ = self.alpha.set_risk_fraction(risk_snapshot.risk_fraction);
                // Capital sizing follows current virtual equity, not starting capital.
                let equity = self.shadow.report().ending_equity;
                if let Some(intent) = self.alpha.on_book(&top, equity) {
                    let _ = self
                        .orders
                        .submit_intent(intent, timestamp, risk_snapshot.entries_allowed)
                        .await;
                }
            }
            MarketEvent::Trade(trade) => {
                self.alpha.on_trade(&trade);
                for update in self.shadow.on_trade(&trade) {
                    self.orders.on_execution_update(update).await?;
                }
                for net_pnl in self.shadow.drain_realized_outcomes() {
                    self.risk.record_trade(crate::risk::TradeOutcome {
                        net_pnl,
                        mae: crate::domain::Fixed::ZERO,
                        mfe: crate::domain::Fixed::ZERO,
                    });
                }
            }
            MarketEvent::MarkPrice(mark) => {
                let _ = mark;
            }
            MarketEvent::ForceOrder(_) => {}
            MarketEvent::MarketDataHalt { .. } => {
                if self.risk.evaluate(
                    RiskInputs {
                        user_stream_silence_ns: 5_000_000_000,
                        ..RiskInputs::default()
                    },
                    timestamp,
                ) == RiskAction::FlattenAndShutdown
                {
                    return Err(BacktestError::RiskKillSwitch);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BacktestReport {
    pub shadow: ShadowReport,
    pub events_processed: u64,
    pub final_risk: crate::risk::RiskSnapshot,
}

impl BacktestReport {
    #[must_use]
    pub fn net_return_fraction(self) -> f64 {
        let starting = self.shadow.starting_equity.as_f64();
        if starting <= 0.0 {
            0.0
        } else {
            (self.shadow.ending_equity - self.shadow.starting_equity).as_f64() / starting
        }
    }
}

#[derive(Debug, Error)]
pub enum BacktestError {
    #[error("historical event moved backwards in time")]
    LookAhead,
    #[error("risk kill-switch stopped the replay")]
    RiskKillSwitch,
    #[error(transparent)]
    Book(#[from] crate::data::BookError),
    #[error(transparent)]
    Order(#[from] OrderManagerError),
}
