use crate::domain::{
    AggTrade, BookDelta, Fixed, ForceOrder, Level, MarkPrice, MarketEvent, Side, Symbol,
    MAX_BOOK_UPDATES,
};
use arrayvec::ArrayVec;
use simd_json::{prelude::*, BorrowedValue};
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed JSON: {0}")]
    Json(#[from] simd_json::Error),
    #[error("missing or invalid field: {0}")]
    Field(&'static str),
    #[error("unsupported market event: {0}")]
    Unsupported(String),
    #[error("too many levels in one depth event")]
    TooManyLevels,
}

/// Parses a Binance combined-stream message in place. The resulting event owns
/// only fixed-size values, so all references into the network buffer are gone
/// before the buffer is returned to its pool.
pub fn parse_market_message(input: &mut [u8]) -> Result<MarketEvent, ParseError> {
    let root: BorrowedValue<'_> = simd_json::to_borrowed_value(input)?;
    let data = root.get("data").unwrap_or(&root);
    let event_type = required_str(data, "e")?;
    match event_type {
        "depthUpdate" => parse_depth(data),
        "aggTrade" => parse_trade(data),
        "markPriceUpdate" => parse_mark_price(data),
        "forceOrder" => parse_force_order(data),
        other => Err(ParseError::Unsupported(other.to_owned())),
    }
}

fn required_str<'reference, 'input>(
    value: &'reference BorrowedValue<'input>,
    key: &'static str,
) -> Result<&'reference str, ParseError> {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .ok_or(ParseError::Field(key))
}

fn required_u64(value: &BorrowedValue<'_>, key: &'static str) -> Result<u64, ParseError> {
    value
        .get(key)
        .and_then(|field| field.as_u64())
        .ok_or(ParseError::Field(key))
}

fn required_fixed(value: &BorrowedValue<'_>, key: &'static str) -> Result<Fixed, ParseError> {
    Fixed::from_str(required_str(value, key)?).map_err(|_| ParseError::Field(key))
}

fn event_time_ns(value: &BorrowedValue<'_>) -> Result<u64, ParseError> {
    required_u64(value, "E")?
        .checked_mul(1_000_000)
        .ok_or(ParseError::Field("E"))
}

fn parse_levels(
    value: &BorrowedValue<'_>,
    key: &'static str,
) -> Result<ArrayVec<Level, MAX_BOOK_UPDATES>, ParseError> {
    let values = value
        .get(key)
        .and_then(|field| field.as_array())
        .ok_or(ParseError::Field(key))?;
    let mut output = ArrayVec::new();
    for raw_level in values {
        let tuple = raw_level.as_array().ok_or(ParseError::Field(key))?;
        if tuple.len() < 2 {
            return Err(ParseError::Field(key));
        }
        let price = tuple[0]
            .as_str()
            .ok_or(ParseError::Field(key))?
            .parse::<Fixed>()
            .map_err(|_| ParseError::Field(key))?;
        let quantity = tuple[1]
            .as_str()
            .ok_or(ParseError::Field(key))?
            .parse::<Fixed>()
            .map_err(|_| ParseError::Field(key))?;
        output
            .try_push(Level { price, quantity })
            .map_err(|_| ParseError::TooManyLevels)?;
    }
    Ok(output)
}

fn parse_depth(value: &BorrowedValue<'_>) -> Result<MarketEvent, ParseError> {
    Ok(MarketEvent::Depth(BookDelta {
        symbol: Symbol::new(required_str(value, "s")?).map_err(|_| ParseError::Field("s"))?,
        event_time_ns: event_time_ns(value)?,
        first_update_id: required_u64(value, "U")?,
        final_update_id: required_u64(value, "u")?,
        previous_final_update_id: required_u64(value, "pu")?,
        bids: parse_levels(value, "b")?,
        asks: parse_levels(value, "a")?,
    }))
}

fn parse_trade(value: &BorrowedValue<'_>) -> Result<MarketEvent, ParseError> {
    Ok(MarketEvent::Trade(AggTrade {
        symbol: Symbol::new(required_str(value, "s")?).map_err(|_| ParseError::Field("s"))?,
        event_time_ns: event_time_ns(value)?,
        trade_id: required_u64(value, "a")?,
        price: required_fixed(value, "p")?,
        quantity: required_fixed(value, "q")?,
        buyer_is_maker: value
            .get("m")
            .and_then(|field| field.as_bool())
            .ok_or(ParseError::Field("m"))?,
    }))
}

fn parse_mark_price(value: &BorrowedValue<'_>) -> Result<MarketEvent, ParseError> {
    Ok(MarketEvent::MarkPrice(MarkPrice {
        symbol: Symbol::new(required_str(value, "s")?).map_err(|_| ParseError::Field("s"))?,
        event_time_ns: event_time_ns(value)?,
        price: required_fixed(value, "p")?,
        funding_rate: required_fixed(value, "r")?,
    }))
}

fn parse_force_order(value: &BorrowedValue<'_>) -> Result<MarketEvent, ParseError> {
    let order = value.get("o").ok_or(ParseError::Field("o"))?;
    let side = match required_str(order, "S")? {
        "BUY" => Side::Buy,
        "SELL" => Side::Sell,
        _ => return Err(ParseError::Field("S")),
    };
    let price = order
        .get("ap")
        .and_then(|field| field.as_str())
        .filter(|raw| *raw != "0")
        .or_else(|| order.get("p").and_then(|field| field.as_str()))
        .ok_or(ParseError::Field("ap"))?
        .parse::<Fixed>()
        .map_err(|_| ParseError::Field("ap"))?;
    Ok(MarketEvent::ForceOrder(ForceOrder {
        symbol: Symbol::new(required_str(order, "s")?).map_err(|_| ParseError::Field("s"))?,
        event_time_ns: event_time_ns(value)?,
        side,
        price,
        quantity: required_fixed(order, "q")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_combined_depth_without_floats() {
        let mut payload = br#"{"stream":"btcusdt@depth@0ms","data":{"e":"depthUpdate","E":1720000000000,"s":"BTCUSDT","U":10,"u":12,"pu":9,"b":[["60000.125","1.25"]],"a":[["60000.25","0"]]}}"#.to_vec();
        let event = parse_market_message(&mut payload).expect("parse");
        let MarketEvent::Depth(depth) = event else {
            panic!("expected depth");
        };
        assert_eq!(depth.final_update_id, 12);
        assert_eq!(depth.bids[0].price.to_string(), "60000.125");
    }

    #[test]
    fn parses_aggregate_trade() {
        let mut payload = br#"{"e":"aggTrade","E":1720000000000,"s":"ETHUSDT","a":42,"p":"3000.1","q":"2.5","m":true}"#.to_vec();
        let event = parse_market_message(&mut payload).expect("parse");
        let MarketEvent::Trade(trade) = event else {
            panic!("expected trade");
        };
        assert!(trade.buyer_is_maker);
        assert_eq!(trade.trade_id, 42);
    }
}
