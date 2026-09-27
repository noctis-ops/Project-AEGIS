use super::{parse_market_message, LatestEventBus};
use crate::domain::{unix_time_ns, BookSnapshot, Fixed, Level, MarketEvent, Symbol};
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{str::FromStr, sync::Arc, time::Duration};
use tokio::time::{sleep, timeout, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub websocket_base: String,
    pub rest_base: String,
    pub heartbeat_timeout: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    pub snapshot_min_interval: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            websocket_base: "wss://fstream.binance.com/stream?streams=".to_owned(),
            rest_base: "https://fapi.binance.com".to_owned(),
            heartbeat_timeout: Duration::from_secs(2),
            reconnect_min: Duration::from_millis(250),
            reconnect_max: Duration::from_secs(30),
            snapshot_min_interval: Duration::from_secs(3),
        }
    }
}

#[derive(Clone)]
pub struct BinanceMarketDataClient {
    config: TransportConfig,
    http: reqwest::Client,
    last_snapshot: Arc<DashMap<Symbol, Instant>>,
}

impl BinanceMarketDataClient {
    pub fn new(config: TransportConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            config,
            http,
            last_snapshot: Arc::new(DashMap::new()),
        })
    }

    /// Starts one WebSocket connection on a dedicated pinned OS thread. The
    /// returned handle is intentionally detached by the process coordinator.
    pub fn spawn_dedicated(
        self,
        symbols: Vec<Symbol>,
        output: LatestEventBus<MarketEvent>,
        core_id: Option<core_affinity::CoreId>,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        std::thread::Builder::new()
            .name("aegis-market-transport".to_owned())
            .spawn(move || {
                if let Some(core_id) = core_id {
                    let _ = core_affinity::set_for_current(core_id);
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("market transport runtime");
                runtime.block_on(self.run(&symbols, output));
            })
    }

    /// Runs until its task is cancelled. Every disconnect emits a halt event
    /// before an exponentially backed-off reconnect attempt.
    pub async fn run(&self, symbols: &[Symbol], output: LatestEventBus<MarketEvent>) {
        let url = self.stream_url(symbols);
        let mut backoff = self.config.reconnect_min;
        loop {
            info!(%url, "connecting Binance market stream");
            match connect_async(&url).await {
                Ok((mut socket, _)) => {
                    backoff = self.config.reconnect_min;
                    loop {
                        let incoming = timeout(self.config.heartbeat_timeout, socket.next()).await;
                        match incoming {
                            Ok(Some(Ok(Message::Ping(payload)))) => {
                                if let Err(error) = socket.send(Message::Pong(payload)).await {
                                    warn!(%error, "failed to answer market-data ping");
                                    break;
                                }
                            }
                            Ok(Some(Ok(Message::Text(text)))) => {
                                let mut bytes = text.as_bytes().to_vec();
                                match parse_market_message(&mut bytes) {
                                    Ok(event) => {
                                        if output.push_latest(event).is_some() {
                                            warn!(
                                                "market event bus overflow; oldest event dropped"
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        warn!(%error, "discarding malformed market event")
                                    }
                                }
                            }
                            Ok(Some(Ok(Message::Binary(payload)))) => {
                                let mut bytes = payload.to_vec();
                                if let Ok(event) = parse_market_message(&mut bytes) {
                                    output.push_latest(event);
                                }
                            }
                            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => break,
                            Ok(Some(Ok(_))) => {}
                            Ok(Some(Err(error))) => {
                                warn!(%error, "market stream error");
                                break;
                            }
                            Err(_) => {
                                error!("market stream heartbeat timed out");
                                break;
                            }
                        }
                    }
                }
                Err(error) => warn!(%error, "market stream connection failed"),
            }
            output.push_latest(MarketEvent::MarketDataHalt {
                reason: "websocket disconnected or heartbeat expired",
                at_ns: unix_time_ns(),
            });
            sleep(backoff).await;
            backoff = backoff.saturating_mul(2).min(self.config.reconnect_max);
        }
    }

    pub async fn open_interest(&self, symbol: Symbol) -> anyhow::Result<Fixed> {
        let response = self
            .http
            .get(format!("{}/fapi/v1/openInterest", self.config.rest_base))
            .query(&[("symbol", symbol.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json::<OpenInterestResponse>()
            .await?;
        Ok(Fixed::from_str(&response.open_interest)?)
    }

    pub async fn snapshot(&self, symbol: Symbol) -> anyhow::Result<BookSnapshot> {
        if let Some(last) = self.last_snapshot.get(&symbol) {
            let elapsed = last.elapsed();
            if elapsed < self.config.snapshot_min_interval {
                sleep(self.config.snapshot_min_interval - elapsed).await;
            }
        }
        self.last_snapshot.insert(symbol, Instant::now());
        let response = self
            .http
            .get(format!("{}/fapi/v1/depth", self.config.rest_base))
            .query(&[("symbol", symbol.as_str()), ("limit", "1000")])
            .send()
            .await?
            .error_for_status()?
            .json::<SnapshotResponse>()
            .await?;
        Ok(BookSnapshot {
            symbol,
            last_update_id: response.last_update_id,
            bids: parse_snapshot_levels(response.bids)?,
            asks: parse_snapshot_levels(response.asks)?,
        })
    }

    fn stream_url(&self, symbols: &[Symbol]) -> String {
        let mut streams = Vec::with_capacity(symbols.len() * 4);
        for symbol in symbols {
            let symbol = symbol.lowercase();
            streams.push(format!("{symbol}@depth@0ms"));
            streams.push(format!("{symbol}@aggTrade"));
            streams.push(format!("{symbol}@markPrice@1s"));
            streams.push(format!("{symbol}@forceOrder"));
        }
        format!("{}{}", self.config.websocket_base, streams.join("/"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenInterestResponse {
    open_interest: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotResponse {
    last_update_id: u64,
    bids: Vec<[String; 2]>,
    asks: Vec<[String; 2]>,
}

fn parse_snapshot_levels(values: Vec<[String; 2]>) -> anyhow::Result<Vec<Level>> {
    values
        .into_iter()
        .map(|[price, quantity]| {
            Ok(Level {
                price: Fixed::from_str(&price)?,
                quantity: Fixed::from_str(&quantity)?,
            })
        })
        .collect()
}
