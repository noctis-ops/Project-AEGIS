use super::{ClientOrderId, ExecutionUpdate, Fill, OrderStatus};
use crate::domain::{unix_time_ns, Fixed, Side, Symbol};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{str::FromStr, time::Duration};
use tokio::{sync::mpsc, time::{interval, sleep, Instant}};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::warn;

#[derive(Debug, Clone)]
pub enum UserDataEvent {
    Execution(ExecutionUpdate),
    AccountEquity { wallet: Fixed, available: Fixed, at_ns: u64 },
    Disconnected { at_ns: u64 },
}

pub struct BinanceUserStream {
    api_key: String,
    rest_base: String,
    websocket_base: String,
    http: reqwest::Client,
}

impl BinanceUserStream {
    pub fn new(api_key: String) -> anyhow::Result<Self> {
        Ok(Self {
            api_key,
            rest_base: "https://fapi.binance.com".to_owned(),
            websocket_base: "wss://fstream.binance.com/ws".to_owned(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .build()?,
        })
    }

    /// Maintains the listen key and reconnects forever. A disconnect is emitted
    /// before every retry so Layer 4 can activate orphan protection.
    pub async fn run(&self, output: mpsc::Sender<UserDataEvent>) {
        loop {
            if let Err(error) = self.run_once(&output).await {
                warn!(%error, "Binance user stream stopped");
            }
            let _ = output
                .send(UserDataEvent::Disconnected { at_ns: unix_time_ns() })
                .await;
            sleep(Duration::from_secs(1)).await;
        }
    }

    async fn run_once(&self, output: &mpsc::Sender<UserDataEvent>) -> anyhow::Result<()> {
        let listen_key = self.create_listen_key().await?;
        let (mut socket, _) = connect_async(format!("{}/{}", self.websocket_base, listen_key)).await?;
        let mut heartbeat = interval(Duration::from_secs(1));
        let mut keepalive = interval(Duration::from_secs(30 * 60));
        let mut last_response = Instant::now();
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if last_response.elapsed() > Duration::from_secs(3) {
                        anyhow::bail!("USER_DATA_STREAM heartbeat exceeded three seconds");
                    }
                    socket.send(Message::Ping(Vec::new().into())).await?;
                }
                _ = keepalive.tick() => {
                    self.keepalive(&listen_key).await?;
                }
                incoming = socket.next() => {
                    let Some(message) = incoming else {
                        anyhow::bail!("USER_DATA_STREAM closed");
                    };
                    match message? {
                        Message::Pong(_) => last_response = Instant::now(),
                        Message::Ping(payload) => {
                            last_response = Instant::now();
                            socket.send(Message::Pong(payload)).await?;
                        }
                        Message::Text(text) => {
                            last_response = Instant::now();
                            if let Some(event) = parse_user_event(text.as_ref())? {
                                output.send(event).await?;
                            }
                        }
                        Message::Close(_) => anyhow::bail!("USER_DATA_STREAM close frame"),
                        _ => {}
                    }
                }
            }
        }
    }

    async fn create_listen_key(&self) -> anyhow::Result<String> {
        let response = self
            .http
            .post(format!("{}/fapi/v1/listenKey", self.rest_base))
            .header("X-MBX-APIKEY", &self.api_key)
            .send()
            .await?
            .error_for_status()?
            .json::<ListenKeyResponse>()
            .await?;
        Ok(response.listen_key)
    }

    async fn keepalive(&self, listen_key: &str) -> anyhow::Result<()> {
        self.http
            .put(format!("{}/fapi/v1/listenKey", self.rest_base))
            .header("X-MBX-APIKEY", &self.api_key)
            .query(&[("listenKey", listen_key)])
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

fn parse_user_event(raw: &str) -> anyhow::Result<Option<UserDataEvent>> {
    let envelope: UserEnvelope = serde_json::from_str(raw)?;
    let at_ns = envelope.event_time.saturating_mul(1_000_000);
    match envelope.event_type.as_str() {
        "ORDER_TRADE_UPDATE" => {
            let order = envelope.order.ok_or_else(|| anyhow::anyhow!("order payload missing"))?;
            let Some(client_id) = ClientOrderId::parse(&order.client_order_id) else {
                // Ignore manual orders, but reconciliation will still see their exposure.
                return Ok(None);
            };
            let status = match order.status.as_str() {
                "NEW" => OrderStatus::New,
                "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
                "FILLED" => OrderStatus::Filled,
                "CANCELED" => OrderStatus::Canceled,
                "EXPIRED" => OrderStatus::Expired,
                "REJECTED" => OrderStatus::Rejected,
                _ => OrderStatus::Indeterminate,
            };
            let last_quantity = Fixed::from_str(&order.last_filled_quantity)?;
            let last_fill = if last_quantity.0 > 0 {
                Some(Fill {
                    client_id,
                    symbol: Symbol::new(&order.symbol)?,
                    side: if order.side == "BUY" { Side::Buy } else { Side::Sell },
                    quantity: last_quantity,
                    price: Fixed::from_str(&order.last_filled_price)?,
                    fee: Fixed::from_str(&order.commission)?,
                    timestamp_ns: at_ns,
                    maker: order.maker,
                })
            } else {
                None
            };
            let realized = Fixed::from_str(&order.realized_profit)?;
            let commission = Fixed::from_str(&order.commission)?;
            let realized_pnl = if last_quantity.0 > 0 && (order.reduce_only || realized.0 != 0) {
                Some(realized - commission)
            } else {
                None
            };
            Ok(Some(UserDataEvent::Execution(ExecutionUpdate {
                client_id,
                status,
                cumulative_filled: Fixed::from_str(&order.cumulative_quantity)?,
                last_fill,
                realized_pnl,
                timestamp_ns: at_ns,
            })))
        }
        "ACCOUNT_UPDATE" => {
            let account = envelope.account.ok_or_else(|| anyhow::anyhow!("account payload missing"))?;
            let usdt = account
                .balances
                .into_iter()
                .find(|balance| balance.asset == "USDT")
                .ok_or_else(|| anyhow::anyhow!("USDT balance missing"))?;
            Ok(Some(UserDataEvent::AccountEquity {
                wallet: Fixed::from_str(&usdt.wallet_balance)?,
                available: Fixed::from_str(&usdt.cross_wallet_balance)?,
                at_ns,
            }))
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListenKeyResponse {
    listen_key: String,
}

#[derive(Debug, Deserialize)]
struct UserEnvelope {
    #[serde(rename = "e")]
    event_type: String,
    #[serde(rename = "E")]
    event_time: u64,
    #[serde(rename = "o")]
    order: Option<OrderPayload>,
    #[serde(rename = "a")]
    account: Option<AccountPayload>,
}

#[derive(Debug, Deserialize)]
struct OrderPayload {
    #[serde(rename = "c")]
    client_order_id: String,
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "S")]
    side: String,
    #[serde(rename = "X")]
    status: String,
    #[serde(rename = "z")]
    cumulative_quantity: String,
    #[serde(rename = "l")]
    last_filled_quantity: String,
    #[serde(rename = "L")]
    last_filled_price: String,
    #[serde(rename = "n", default = "zero_decimal")]
    commission: String,
    #[serde(rename = "m", default)]
    maker: bool,
    #[serde(rename = "R", default)]
    reduce_only: bool,
    #[serde(rename = "rp", default = "zero_decimal")]
    realized_profit: String,
}

fn zero_decimal() -> String {
    "0".to_owned()
}

#[derive(Debug, Deserialize)]
struct AccountPayload {
    #[serde(rename = "B")]
    balances: Vec<BalancePayload>,
}

#[derive(Debug, Deserialize)]
struct BalancePayload {
    #[serde(rename = "a")]
    asset: String,
    #[serde(rename = "wb")]
    wallet_balance: String,
    #[serde(rename = "cw")]
    cross_wallet_balance: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_execution_report() {
        let raw = r#"{"e":"ORDER_TRADE_UPDATE","E":1000,"o":{"c":"00000000000000000000000000000001","s":"BTCUSDT","S":"BUY","X":"PARTIALLY_FILLED","z":"0.5","l":"0.5","L":"100","n":"0.01","m":true}}"#;
        let event = parse_user_event(raw).expect("parse").expect("event");
        let UserDataEvent::Execution(update) = event else {
            panic!("execution event expected");
        };
        assert_eq!(update.status, OrderStatus::PartiallyFilled);
        assert_eq!(update.last_fill.expect("fill").price, Fixed::from_f64(100.0));
    }
}
