//! Cached market descriptions.

use crate::codec::{
    decode_decimal, decode_opt_decimal, decode_opt_u32, decode_opt_u64, decode_time, decode_u32,
    encode_decimal, encode_opt_u64, encode_time, encode_u32,
};
use crate::error::{map_sqlx, require_one_row};
use crate::repos::col;
use crate::SqliteDb;
use async_trait::async_trait;
use cb_ports::db::MarketMetaRepository;
use cb_ports::{MarketMeta, PortResult};
use sqlx::sqlite::SqliteRow;

const COLUMNS: &str = "symbol, kind, asset_class, base, quote, display_name, status, \
     tick_size, lot_size, min_notional, price_decimals, size_decimals, max_leverage, \
     initial_margin_bps, maintenance_margin_bps, funding_interval_seconds, expiry, \
     maker_fee_bps, taker_fee_bps";

fn from_row(row: &SqliteRow) -> PortResult<MarketMeta> {
    let read = |name: &str| -> PortResult<rust_decimal::Decimal> {
        let raw: String = col(row, name)?;
        decode_decimal(name, &raw)
    };
    let expiry: Option<i64> = col(row, "expiry")?;

    Ok(MarketMeta {
        symbol: col(row, "symbol")?,
        kind: col(row, "kind")?,
        asset_class: col(row, "asset_class")?,
        base: col(row, "base")?,
        quote: col(row, "quote")?,
        display_name: col(row, "display_name")?,
        status: col(row, "status")?,
        tick_size: read("tick_size")?,
        lot_size: read("lot_size")?,
        min_notional: read("min_notional")?,
        price_decimals: decode_u32("price_decimals", col(row, "price_decimals")?)?,
        size_decimals: decode_u32("size_decimals", col(row, "size_decimals")?)?,
        max_leverage: decode_opt_decimal("max_leverage", col(row, "max_leverage")?)?,
        initial_margin_bps: decode_opt_u32("initial_margin_bps", col(row, "initial_margin_bps")?)?,
        maintenance_margin_bps: decode_opt_u32(
            "maintenance_margin_bps",
            col(row, "maintenance_margin_bps")?,
        )?,
        funding_interval_seconds: decode_opt_u64(
            "funding_interval_seconds",
            col(row, "funding_interval_seconds")?,
        )?,
        expiry: expiry.map(|v| decode_time("expiry", v)).transpose()?,
        maker_fee_bps: col(row, "maker_fee_bps")?,
        taker_fee_bps: col(row, "taker_fee_bps")?,
    })
}

#[async_trait]
impl MarketMetaRepository for SqliteDb {
    /// Replaces the whole table in one transaction.
    ///
    /// The chain is authoritative about which markets exist; this table is a
    /// cache of that list. Merging would leave a delisted market visible
    /// forever, because nothing else ever deletes from here. Replacement makes
    /// the cache converge on the chain's answer by construction.
    ///
    /// The status set by `set_status` is overwritten by this call. That is
    /// intended: a refresh from the chain carries the authoritative status,
    /// and a local override surviving it would be a lie with no expiry.
    async fn replace_all(&self, markets: &[MarketMeta]) -> PortResult<()> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| map_sqlx("beginning a market refresh", e))?;

        sqlx::query("DELETE FROM market_meta")
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx("clearing the market cache", e))?;

        for market in markets {
            let funding =
                encode_opt_u64("funding_interval_seconds", market.funding_interval_seconds)?;
            sqlx::query(
                "INSERT INTO market_meta \
                 (symbol, kind, asset_class, base, quote, display_name, status, tick_size, \
                  lot_size, min_notional, price_decimals, size_decimals, max_leverage, \
                  initial_margin_bps, maintenance_margin_bps, funding_interval_seconds, \
                  expiry, maker_fee_bps, taker_fee_bps) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&market.symbol)
            .bind(&market.kind)
            .bind(&market.asset_class)
            .bind(&market.base)
            .bind(&market.quote)
            .bind(&market.display_name)
            .bind(&market.status)
            .bind(encode_decimal(&market.tick_size))
            .bind(encode_decimal(&market.lot_size))
            .bind(encode_decimal(&market.min_notional))
            .bind(encode_u32(market.price_decimals))
            .bind(encode_u32(market.size_decimals))
            .bind(market.max_leverage.as_ref().map(encode_decimal))
            .bind(market.initial_margin_bps.map(encode_u32))
            .bind(market.maintenance_margin_bps.map(encode_u32))
            .bind(funding)
            .bind(market.expiry.map(encode_time))
            .bind(market.maker_fee_bps)
            .bind(market.taker_fee_bps)
            .execute(&mut *tx)
            .await
            .map_err(|e| map_sqlx(&format!("caching market `{}`", market.symbol), e))?;
        }

        tx.commit()
            .await
            .map_err(|e| map_sqlx("committing a market refresh", e))
    }

    async fn all(&self) -> PortResult<Vec<MarketMeta>> {
        let rows = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM market_meta ORDER BY symbol"
        ))
        .fetch_all(self.pool())
        .await
        .map_err(|e| map_sqlx("listing markets", e))?;
        rows.iter().map(from_row).collect()
    }

    async fn by_symbol(&self, symbol: &str) -> PortResult<Option<MarketMeta>> {
        let row = sqlx::query(&format!(
            "SELECT {COLUMNS} FROM market_meta WHERE symbol = ?"
        ))
        .bind(symbol)
        .fetch_optional(self.pool())
        .await
        .map_err(|e| map_sqlx(&format!("loading market `{symbol}`"), e))?;
        row.as_ref().map(from_row).transpose()
    }

    /// Halting a market that is not in the cache is an error, not a no-op: the
    /// caller believes it just halted trading, and silence would let it think
    /// so.
    async fn set_status(&self, symbol: &str, status: &str) -> PortResult<()> {
        let result = sqlx::query("UPDATE market_meta SET status = ? WHERE symbol = ?")
            .bind(status)
            .bind(symbol)
            .execute(self.pool())
            .await
            .map_err(|e| map_sqlx(&format!("setting status of `{symbol}`"), e))?;
        require_one_row(&format!("market `{symbol}`"), result.rows_affected())
    }
}
