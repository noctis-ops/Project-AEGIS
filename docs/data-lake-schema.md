# AEGIS tick data lake schema

Files are Apache Parquet, ordered strictly by `timestamp_ns`, and use the columns below. Decimal market values remain strings so ingestion never passes through a floating-point parser.

| index | name | Parquet type | use |
|---:|---|---|---|
| 0 | `record_type` | UTF8 | `snapshot`, `depth`, or `trade` |
| 1 | `timestamp_ns` | INT64 | exchange/event time in nanoseconds |
| 2 | `symbol` | UTF8 | Binance symbol |
| 3 | `previous_final_update_id` | INT64 | depth only |
| 4 | `first_update_id` | INT64 | depth only |
| 5 | `final_update_id` | INT64 | depth, or snapshot `last_update_id` |
| 6 | `bids_json` | UTF8 | JSON array of decimal string pairs |
| 7 | `asks_json` | UTF8 | JSON array of decimal string pairs |
| 8 | `trade_id` | INT64 | trade only |
| 9 | `price` | UTF8 | trade decimal string |
| 10 | `quantity` | UTF8 | trade decimal string |
| 11 | `buyer_is_maker` | BOOLEAN | trade aggressor classification |

A partition starts with a complete 1,000-level snapshot. Differential rows then follow Binance `U/u/pu` sequence rules. Partitioning convention:

```text
data/symbol=BTCUSDT/date=2026-09-25/hour=13/part-000.parquet
```

The loader rejects out-of-order rows. It never sorts malformed input because sorting could hide collection defects and introduce look-ahead bias.
