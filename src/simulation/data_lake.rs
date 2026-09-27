use crate::domain::{BookSnapshot, MarketEvent};
use std::path::Path;

// Replay preserves the exact allocation-free MarketEvent representation used
// by the live path; the size trade-off is deliberate path parity.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum HistoricalRecord {
    Snapshot {
        snapshot: BookSnapshot,
        timestamp_ns: u64,
    },
    Event(MarketEvent),
}

impl HistoricalRecord {
    #[must_use]
    pub fn timestamp_ns(&self) -> u64 {
        match self {
            Self::Snapshot { timestamp_ns, .. } => *timestamp_ns,
            Self::Event(event) => event.timestamp_ns(),
        }
    }
}

/// Strict chronological feeder. It never exposes an item after the cursor and
/// rejects a source with backwards timestamps rather than silently sorting it.
pub struct DataFeed {
    records: Vec<HistoricalRecord>,
    cursor: usize,
    current_time_ns: u64,
}

impl DataFeed {
    pub fn new(records: Vec<HistoricalRecord>) -> anyhow::Result<Self> {
        let mut previous = 0;
        for record in &records {
            let timestamp = record.timestamp_ns();
            if timestamp != 0 && timestamp < previous {
                anyhow::bail!("historical feed is not chronologically ordered");
            }
            previous = previous.max(timestamp);
        }
        Ok(Self {
            records,
            cursor: 0,
            current_time_ns: 0,
        })
    }

    #[must_use]
    pub const fn current_time_ns(&self) -> u64 {
        self.current_time_ns
    }
}

impl Iterator for DataFeed {
    type Item = HistoricalRecord;

    fn next(&mut self) -> Option<Self::Item> {
        let record = self.records.get(self.cursor)?.clone();
        self.cursor += 1;
        self.current_time_ns = self.current_time_ns.max(record.timestamp_ns());
        Some(record)
    }
}

pub struct HistoricalDataLake;

impl HistoricalDataLake {
    /// Reads the canonical AEGIS Parquet schema documented in
    /// `docs/data-lake-schema.md`.
    #[cfg(feature = "parquet-data")]
    pub fn read(path: impl AsRef<Path>) -> anyhow::Result<Vec<HistoricalRecord>> {
        use crate::domain::{AggTrade, Fixed, Level, Symbol};
        use parquet::{
            file::reader::{FileReader, SerializedFileReader},
            record::RowAccessor,
        };
        use std::{fs::File, str::FromStr};

        let reader = SerializedFileReader::new(File::open(path)?)?;
        let mut records = Vec::new();
        let mut previous_timestamp = 0_u64;
        for row in reader.get_row_iter(None)? {
            let row = row?;
            let kind = row.get_string(0)?;
            let timestamp_ns = row.get_long(1)? as u64;
            if timestamp_ns < previous_timestamp {
                anyhow::bail!("Parquet rows are not chronological");
            }
            previous_timestamp = timestamp_ns;
            let symbol = Symbol::new(row.get_string(2)?)?;
            match kind.as_str() {
                "snapshot" => {
                    records.push(HistoricalRecord::Snapshot {
                        snapshot: BookSnapshot {
                            symbol,
                            last_update_id: row.get_long(5)? as u64,
                            bids: parse_json_levels(row.get_string(6)?)?,
                            asks: parse_json_levels(row.get_string(7)?)?,
                        },
                        timestamp_ns,
                    });
                }
                "depth" => {
                    let bids: Vec<Level> = parse_json_levels(row.get_string(6)?)?;
                    let asks: Vec<Level> = parse_json_levels(row.get_string(7)?)?;
                    records.push(HistoricalRecord::Event(MarketEvent::Depth(BookDelta {
                        symbol,
                        event_time_ns: timestamp_ns,
                        first_update_id: row.get_long(4)? as u64,
                        final_update_id: row.get_long(5)? as u64,
                        previous_final_update_id: row.get_long(3)? as u64,
                        bids: levels_to_arrayvec(bids)
                            .map_err(|_| anyhow::anyhow!("too many bid updates"))?,
                        asks: levels_to_arrayvec(asks)
                            .map_err(|_| anyhow::anyhow!("too many ask updates"))?,
                    })));
                }
                "trade" => {
                    records.push(HistoricalRecord::Event(MarketEvent::Trade(AggTrade {
                        symbol,
                        event_time_ns: timestamp_ns,
                        trade_id: row.get_long(8)? as u64,
                        price: Fixed::from_str(row.get_string(9)?)?,
                        quantity: Fixed::from_str(row.get_string(10)?)?,
                        buyer_is_maker: row.get_bool(11)?,
                    })));
                }
                unknown => anyhow::bail!("unsupported data-lake record: {unknown}"),
            }
        }
        Ok(records)
    }

    #[cfg(not(feature = "parquet-data"))]
    pub fn read(_path: impl AsRef<Path>) -> anyhow::Result<Vec<HistoricalRecord>> {
        anyhow::bail!("Parquet support is disabled; rebuild with --features parquet-data")
    }
}

#[cfg(feature = "parquet-data")]
fn levels_to_arrayvec(
    levels: Vec<crate::domain::Level>,
) -> Result<arrayvec::ArrayVec<crate::domain::Level, { crate::domain::MAX_BOOK_UPDATES }>, ()> {
    let mut output = arrayvec::ArrayVec::new();
    for level in levels {
        output.try_push(level).map_err(|_| ())?;
    }
    Ok(output)
}

#[cfg(feature = "parquet-data")]
fn parse_json_levels(raw: &str) -> anyhow::Result<Vec<crate::domain::Level>> {
    use crate::domain::{Fixed, Level};
    use std::str::FromStr;

    let values: Vec<[String; 2]> = serde_json::from_str(raw)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AggTrade, Fixed, Symbol};

    #[test]
    fn feed_rejects_look_ahead_ordering() {
        let symbol = Symbol::new("BTCUSDT").expect("symbol");
        let trade = |timestamp| {
            HistoricalRecord::Event(MarketEvent::Trade(AggTrade {
                symbol,
                event_time_ns: timestamp,
                trade_id: timestamp,
                price: Fixed::from_f64(100.0),
                quantity: Fixed::from_f64(1.0),
                buyer_is_maker: true,
            }))
        };
        assert!(DataFeed::new(vec![trade(20), trade(10)]).is_err());
    }
}
