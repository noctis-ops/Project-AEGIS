#![cfg_attr(not(feature = "live-trading"), allow(dead_code, unused_imports))]

use super::{
    ClientOrderId, ExecutionMode, ExecutionVenue, OrderAck, OrderKind, OrderRequest, OrderStatus,
    Position, TimeInForce, VenueError,
};
use crate::domain::{unix_time_ns, Fixed, Side, Symbol};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::Sha256;
use std::{collections::BTreeMap, env, str::FromStr, time::Duration};
use tokio::{net::TcpStream, sync::Mutex, time::timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type HmacSha256 = Hmac<Sha256>;
type BinanceSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct LiveCredentials {
    api_key: String,
    secret_key: String,
}

impl LiveCredentials {
    pub fn from_env() -> Result<Self, VenueError> {
        let api_key = env::var("BINANCE_API_KEY")
            .map_err(|_| VenueError::Transport("BINANCE_API_KEY is not set".to_owned()))?;
        let secret_key = env::var("BINANCE_SECRET_KEY")
            .map_err(|_| VenueError::Transport("BINANCE_SECRET_KEY is not set".to_owned()))?;
        Ok(Self {
            api_key,
            secret_key,
        })
    }
}

/// Persistent Binance USD-M WebSocket Order API client. REST is used only for
/// ground-truth position reconciliation. Secrets are accepted only at runtime.
pub struct BinanceLiveExecution {
    credentials: LiveCredentials,
    websocket_url: String,
    rest_url: String,
    http: reqwest::Client,
    socket: Mutex<Option<BinanceSocket>>,
}

impl BinanceLiveExecution {
    pub fn new(credentials: LiveCredentials, acknowledgement: &str) -> Result<Self, VenueError> {
        #[cfg(not(feature = "live-trading"))]
        {
            let _ = credentials;
            let _ = acknowledgement;
            return Err(VenueError::LiveDisabled);
        }
        #[cfg(feature = "live-trading")]
        {
            if acknowledgement != "I_UNDERSTAND_THE_RISK" {
                return Err(VenueError::LiveAcknowledgementMissing);
            }
            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .tcp_nodelay(true)
                .build()
                .map_err(|error| VenueError::Transport(error.to_string()))?;
            Ok(Self {
                credentials,
                websocket_url: "wss://ws-fapi.binance.com/ws-fapi/v1".to_owned(),
                rest_url: "https://fapi.binance.com".to_owned(),
                http,
                socket: Mutex::new(None),
            })
        }
    }

    #[cfg(feature = "live-trading")]
    async fn ws_call(
        &self,
        method: &str,
        mut params: BTreeMap<String, String>,
    ) -> Result<Value, VenueError> {
        params.insert("apiKey".to_owned(), self.credentials.api_key.clone());
        params.insert(
            "timestamp".to_owned(),
            (unix_time_ns() / 1_000_000).to_string(),
        );
        params.insert("recvWindow".to_owned(), "2000".to_owned());
        let signature = sign_query(&self.credentials.secret_key, &query_string(&params))?;
        params.insert("signature".to_owned(), signature);

        let request_id = unix_time_ns();
        let parameter_object: Map<String, Value> = params
            .into_iter()
            .map(|(key, value)| (key, Value::String(value)))
            .collect();
        let payload = json!({
            "id": request_id,
            "method": method,
            "params": parameter_object,
        })
        .to_string();

        let mut guard = self.socket.lock().await;
        if guard.is_none() {
            let (socket, _) = connect_async(&self.websocket_url)
                .await
                .map_err(|error| VenueError::Transport(error.to_string()))?;
            *guard = Some(socket);
        }
        let send_result = guard
            .as_mut()
            .expect("socket initialized")
            .send(Message::Text(payload.into()))
            .await;
        if let Err(error) = send_result {
            *guard = None;
            return Err(VenueError::Transport(error.to_string()));
        }

        loop {
            let next = timeout(
                Duration::from_secs(2),
                guard.as_mut().expect("socket initialized").next(),
            )
            .await;
            let message = match next {
                Ok(Some(Ok(message))) => message,
                Ok(Some(Err(error))) => {
                    *guard = None;
                    return Err(VenueError::Transport(error.to_string()));
                }
                Ok(None) => {
                    *guard = None;
                    return Err(VenueError::Indeterminate);
                }
                Err(_) => return Err(VenueError::Indeterminate),
            };
            if let Message::Ping(payload) = message {
                guard
                    .as_mut()
                    .expect("socket initialized")
                    .send(Message::Pong(payload))
                    .await
                    .map_err(|error| VenueError::Transport(error.to_string()))?;
                continue;
            }
            let Message::Text(text) = message else {
                continue;
            };
            let response: Value = serde_json::from_str(text.as_ref())
                .map_err(|error| VenueError::InvalidResponse(error.to_string()))?;
            if response.get("id").and_then(Value::as_u64) != Some(request_id) {
                continue;
            }
            let status = response
                .get("status")
                .and_then(Value::as_u64)
                .unwrap_or(500);
            if status >= 400 {
                let error = response.get("error").cloned().unwrap_or(Value::Null);
                return Err(VenueError::Rejected {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(-1),
                    message: error
                        .get("msg")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown Binance error")
                        .to_owned(),
                });
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| VenueError::InvalidResponse("missing result".to_owned()));
        }
    }

    #[cfg(feature = "live-trading")]
    fn order_parameters(request: &OrderRequest) -> Result<BTreeMap<String, String>, VenueError> {
        let mut params = BTreeMap::new();
        params.insert("symbol".to_owned(), request.symbol.to_string());
        params.insert(
            "side".to_owned(),
            match request.side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            }
            .to_owned(),
        );
        params.insert("newClientOrderId".to_owned(), request.client_id.to_string());
        params.insert("quantity".to_owned(), request.quantity.to_string());
        params.insert(
            "type".to_owned(),
            match request.kind {
                OrderKind::Limit => "LIMIT",
                OrderKind::Market => "MARKET",
                OrderKind::StopMarket => "STOP_MARKET",
                OrderKind::TakeProfitMarket => "TAKE_PROFIT_MARKET",
            }
            .to_owned(),
        );
        if request.kind == OrderKind::Limit {
            params.insert(
                "price".to_owned(),
                request
                    .price
                    .ok_or_else(|| VenueError::InvalidResponse("limit price missing".to_owned()))?
                    .to_string(),
            );
            params.insert(
                "timeInForce".to_owned(),
                match request.time_in_force {
                    TimeInForce::Gtx => "GTX",
                    TimeInForce::Ioc => "IOC",
                    TimeInForce::Gtc => "GTC",
                }
                .to_owned(),
            );
        }
        if let Some(trigger) = request.trigger_price {
            params.insert("stopPrice".to_owned(), trigger.to_string());
            params.insert("workingType".to_owned(), "MARK_PRICE".to_owned());
        }
        if request.reduce_only {
            params.insert("reduceOnly".to_owned(), "true".to_owned());
        }
        Ok(params)
    }

    #[cfg(feature = "live-trading")]
    async fn rest_positions(&self) -> Result<Vec<Position>, VenueError> {
        let mut params = BTreeMap::new();
        params.insert(
            "timestamp".to_owned(),
            (unix_time_ns() / 1_000_000).to_string(),
        );
        params.insert("recvWindow".to_owned(), "2000".to_owned());
        let query = query_string(&params);
        let signature = sign_query(&self.credentials.secret_key, &query)?;
        let response = self
            .http
            .get(format!(
                "{}/fapi/v2/positionRisk?{}&signature={}",
                self.rest_url, query, signature
            ))
            .header("X-MBX-APIKEY", &self.credentials.api_key)
            .send()
            .await
            .map_err(|error| VenueError::Transport(error.to_string()))?
            .error_for_status()
            .map_err(|error| VenueError::Transport(error.to_string()))?
            .json::<Vec<PositionResponse>>()
            .await
            .map_err(|error| VenueError::InvalidResponse(error.to_string()))?;
        response
            .into_iter()
            .filter(|position| position.position_amt != "0")
            .map(|position| {
                Ok(Position {
                    symbol: Symbol::new(&position.symbol)
                        .map_err(|_| VenueError::InvalidResponse("invalid symbol".to_owned()))?,
                    signed_quantity: Fixed::from_str(&position.position_amt)
                        .map_err(|error| VenueError::InvalidResponse(error.to_string()))?,
                    entry_price: Fixed::from_str(&position.entry_price)
                        .map_err(|error| VenueError::InvalidResponse(error.to_string()))?,
                    unrealized_pnl: Fixed::from_str(&position.un_realized_profit)
                        .map_err(|error| VenueError::InvalidResponse(error.to_string()))?,
                })
            })
            .collect()
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PositionResponse {
    symbol: String,
    position_amt: String,
    entry_price: String,
    un_realized_profit: String,
}

#[async_trait]
impl ExecutionVenue for BinanceLiveExecution {
    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Live
    }

    async fn submit(&self, request: OrderRequest) -> Result<OrderAck, VenueError> {
        #[cfg(not(feature = "live-trading"))]
        {
            let _ = request;
            Err(VenueError::LiveDisabled)
        }
        #[cfg(feature = "live-trading")]
        {
            let params = Self::order_parameters(&request)?;
            let result = self.ws_call("order.place", params).await?;
            let order_id = result.get("orderId").and_then(Value::as_u64);
            let status = parse_status(result.get("status").and_then(Value::as_str));
            Ok(OrderAck {
                client_id: request.client_id,
                venue_order_id: order_id,
                status,
                accepted_at_ns: unix_time_ns(),
            })
        }
    }

    async fn cancel(
        &self,
        symbol: Symbol,
        client_id: ClientOrderId,
    ) -> Result<OrderAck, VenueError> {
        #[cfg(not(feature = "live-trading"))]
        {
            let _ = symbol;
            let _ = client_id;
            Err(VenueError::LiveDisabled)
        }
        #[cfg(feature = "live-trading")]
        {
            let mut params = BTreeMap::new();
            params.insert("symbol".to_owned(), symbol.to_string());
            params.insert("origClientOrderId".to_owned(), client_id.to_string());
            let result = self.ws_call("order.cancel", params).await?;
            Ok(OrderAck {
                client_id,
                venue_order_id: result.get("orderId").and_then(Value::as_u64),
                status: OrderStatus::Canceled,
                accepted_at_ns: unix_time_ns(),
            })
        }
    }

    async fn cancel_all(&self, symbol: Symbol) -> Result<(), VenueError> {
        #[cfg(not(feature = "live-trading"))]
        {
            let _ = symbol;
            Err(VenueError::LiveDisabled)
        }
        #[cfg(feature = "live-trading")]
        {
            let mut params = BTreeMap::new();
            params.insert("symbol".to_owned(), symbol.to_string());
            self.ws_call("openOrders.cancelAll", params).await?;
            Ok(())
        }
    }

    async fn positions(&self) -> Result<Vec<Position>, VenueError> {
        #[cfg(not(feature = "live-trading"))]
        {
            Err(VenueError::LiveDisabled)
        }
        #[cfg(feature = "live-trading")]
        {
            self.rest_positions().await
        }
    }
}

#[cfg(feature = "live-trading")]
fn parse_status(status: Option<&str>) -> OrderStatus {
    match status.unwrap_or("NEW") {
        "NEW" => OrderStatus::New,
        "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
        "FILLED" => OrderStatus::Filled,
        "CANCELED" => OrderStatus::Canceled,
        "EXPIRED" => OrderStatus::Expired,
        "REJECTED" => OrderStatus::Rejected,
        _ => OrderStatus::Indeterminate,
    }
}

fn query_string(params: &BTreeMap<String, String>) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn sign_query(secret: &str, query: &str) -> Result<String, VenueError> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|error| VenueError::Transport(error.to_string()))?;
    mac.update(query.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_signature_matches_known_vector() {
        let signature =
            sign_query("key", "The quick brown fox jumps over the lazy dog").expect("signature");
        assert_eq!(
            signature,
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }
}
