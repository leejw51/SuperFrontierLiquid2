//! Websocket message shapes, in both directions.
//!
//! Parsing is deliberately two-stage. A frame is first read as a
//! [`RawMessage`] — channel, type, and an untouched `data` value — and only then
//! matched into a [`ServerMessage`]. That way an unrecognised channel or a new
//! message type from a newer gateway is an [`ServerMessage::Unknown`] that the
//! client ignores, rather than a parse failure that tears down a working
//! connection.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::api::error::GatewayError;
use crate::api::types::{Balance, Fill, Order, OrderBookPayload, Position, Side};

/// A command sent to the gateway.
///
/// `Deserialize` is derived as well as `Serialize` so that the mock feed can
/// read the very frames the client writes, rather than a parallel set of types
/// that could drift away from them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ClientMessage {
    /// Present the session token, to receive private channels.
    Auth {
        /// The bearer token.
        token: String,
    },
    /// Subscribe to channels. The gateway answers a book subscription with a
    /// fresh snapshot, every time.
    Subscribe {
        /// Channel names such as `orderbook:BTC-PERP`.
        channels: Vec<String>,
    },
    /// Stop receiving channels.
    Unsubscribe {
        /// Channel names to drop.
        channels: Vec<String>,
    },
    /// Keep the connection alive. The gateway closes after 90 silent seconds.
    Ping,
}

/// A frame as it arrives, before it is matched.
#[derive(Debug, Clone, Deserialize)]
pub struct RawMessage {
    /// The channel this frame belongs to.
    pub channel: String,
    /// The message type within that channel.
    #[serde(rename = "type")]
    pub kind: String,
    /// The payload, still untyped.
    #[serde(default)]
    pub data: serde_json::Value,
}

/// A ticker update from the all-market `ticker` channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TickerUpdate {
    /// The market.
    pub symbol: String,
    /// Last traded price.
    pub last_price: Decimal,
    /// Mark price.
    pub mark_price: Decimal,
    /// 24-hour change, already a percentage.
    pub change_24h_pct: Decimal,
    /// Funding rate per interval, as a fraction.
    #[serde(default)]
    pub funding_rate: Option<Decimal>,
}

/// A new block header from the `chain` channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BlockUpdate {
    /// The new height.
    pub height: u64,
    /// The block hash.
    pub hash: String,
    /// How many transactions it carried.
    pub tx_count: u32,
    /// When it was produced.
    pub at: String,
}

/// A trade from a `trades:{symbol}` channel.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TradeUpdate {
    /// Execution price.
    pub price: Decimal,
    /// Executed quantity.
    pub qty: Decimal,
    /// The aggressor's side.
    pub side: Side,
    /// When it happened.
    pub at: String,
}

/// The `account` channel's liquidation message.
///
/// This gets a toast of its own that stays until dismissed. It is the game's
/// most consequential event and it is never allowed to scroll past.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LiquidationEvent {
    /// The market that was closed out.
    pub symbol: String,
    /// How much was closed.
    pub qty: Decimal,
    /// The price it closed at.
    pub price: Decimal,
    /// The loss taken; negative.
    pub loss: Decimal,
    /// When it happened.
    pub at: String,
    /// The block it landed in, where the gateway supplies one.
    #[serde(default)]
    pub height: u64,
}

/// A frame, matched.
#[derive(Debug, Clone)]
pub enum ServerMessage {
    /// A fresh book for a market. Replaces whatever is held.
    BookSnapshot(String, OrderBookPayload),
    /// A book patch for a market.
    BookDelta(String, OrderBookPayload),
    /// A public trade.
    Trade(String, TradeUpdate),
    /// A ticker update.
    Ticker(TickerUpdate),
    /// A new block.
    Block(BlockUpdate),
    /// One of the account's orders changed.
    OrderUpdate(Box<Order>),
    /// One of the account's orders executed.
    Fill(Box<Fill>),
    /// One of the account's positions changed.
    PositionUpdate(Box<Position>),
    /// The account's balances changed.
    BalanceUpdate(Vec<Balance>),
    /// A position was forcibly closed.
    Liquidation(LiquidationEvent),
    /// The gateway's answer to a ping.
    Pong,
    /// The gateway reported an error on the socket.
    Error(GatewayError),
    /// Something this build does not recognise, and safely ignores.
    Unknown,
}

/// Reads a payload, or `None` when it is not the shape this build expects.
///
/// A frame that fails to decode is dropped rather than fatal, but it is **not**
/// dropped silently: a shape mismatch between this client and the gateway is
/// precisely the kind of bug that looks like "the screen just does not update",
/// and a line in the console is the difference between finding it in a minute
/// and finding it in a day.
fn decode<T: serde::de::DeserializeOwned>(
    channel: &str,
    kind: &str,
    value: serde_json::Value,
) -> Option<T> {
    match serde_json::from_value(value) {
        Ok(decoded) => Some(decoded),
        Err(error) => {
            #[cfg(target_arch = "wasm32")]
            web_sys::console::warn_1(
                &format!("cb-web: dropped a {channel}/{kind} frame: {error}").into(),
            );
            #[cfg(not(target_arch = "wasm32"))]
            let _ = (channel, kind, error);
            None
        }
    }
}

/// The market a `channel` string names, if it names one.
fn channel_symbol(channel: &str) -> Option<&str> {
    channel.split_once(':').map(|(_, rest)| rest)
}

impl ServerMessage {
    /// Matches a raw frame into a typed message.
    ///
    /// A payload that does not parse becomes [`ServerMessage::Unknown`] rather
    /// than an error: one malformed frame must not take down a connection that
    /// is otherwise carrying a working market.
    #[must_use]
    pub fn from_raw(raw: RawMessage) -> Self {
        let symbol = channel_symbol(&raw.channel).unwrap_or_default().to_string();
        let RawMessage {
            channel,
            kind,
            data,
        } = raw;

        let family = channel.split(':').next().unwrap_or_default().to_string();
        match (family.as_str(), kind.as_str()) {
            // Subscription acknowledgements carry the channel name and no
            // payload. They are not data and were being decoded as data: a
            // `ticker/subscribed` ack logged a dropped-frame warning on every
            // connect, which is noise dressed as a defect.
            (_, "subscribed" | "unsubscribed") => Self::Unknown,
            ("orderbook", "snapshot") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |payload| Self::BookSnapshot(symbol, payload)),
            ("orderbook", "delta") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |payload| Self::BookDelta(symbol, payload)),
            ("trades", "trade") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |trade| Self::Trade(symbol, trade)),
            ("ticker", _) => decode(&channel, &kind, data).map_or(Self::Unknown, Self::Ticker),
            ("chain", "block") => decode(&channel, &kind, data).map_or(Self::Unknown, Self::Block),
            ("account", "order_update") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |order| Self::OrderUpdate(Box::new(order))),
            ("account", "fill") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |fill| Self::Fill(Box::new(fill))),
            ("account", "position_update") => decode(&channel, &kind, data)
                .map_or(Self::Unknown, |p| Self::PositionUpdate(Box::new(p))),
            ("account", "balance_update") => {
                decode(&channel, &kind, data).map_or(Self::Unknown, Self::BalanceUpdate)
            }
            ("account", "liquidation") => {
                decode(&channel, &kind, data).map_or(Self::Unknown, Self::Liquidation)
            }
            ("system", "pong") => Self::Pong,
            ("system", "error") => decode(&channel, &kind, data).map_or(Self::Unknown, Self::Error),
            _ => Self::Unknown,
        }
    }

    /// Parses a raw text frame.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        serde_json::from_str::<RawMessage>(text).map_or(Self::Unknown, Self::from_raw)
    }
}

/// The channel name for a market's order book.
#[must_use]
pub fn orderbook_channel(symbol: &str) -> String {
    format!("orderbook:{symbol}")
}

/// The channel name for a market's trade tape.
#[must_use]
pub fn trades_channel(symbol: &str) -> String {
    format!("trades:{symbol}")
}

/// The channel name for a market's in-progress candle.
#[must_use]
pub fn candles_channel(symbol: &str, interval: &str) -> String {
    format!("candles:{symbol}:{interval}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contracts_snapshot_and_delta_examples_parse() {
        let snapshot = ServerMessage::parse(
            r#"{"channel":"orderbook:BTC-PERP","type":"snapshot",
                "data":{"height":184203,"bids":[["64250.00","1.25"]],"asks":[]}}"#,
        );
        match snapshot {
            ServerMessage::BookSnapshot(symbol, payload) => {
                assert_eq!(symbol, "BTC-PERP");
                assert_eq!(payload.height, 184_203);
                assert_eq!(payload.bids.len(), 1);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }

        let delta = ServerMessage::parse(
            r#"{"channel":"orderbook:BTC-PERP","type":"delta",
                "data":{"height":184204,"bids":[["64250.00","0.00"],["64249.00","2.00"]],"asks":[]}}"#,
        );
        match delta {
            ServerMessage::BookDelta(_, payload) => {
                assert_eq!(payload.bids.len(), 2);
                assert!(
                    payload.bids[0].qty().is_zero(),
                    "a removal survives parsing"
                );
            }
            other => panic!("expected a delta, got {other:?}"),
        }
    }

    #[test]
    fn an_unrecognised_frame_is_ignored_rather_than_fatal() {
        assert!(matches!(
            ServerMessage::parse(r#"{"channel":"weather","type":"rain","data":{}}"#),
            ServerMessage::Unknown
        ));
        assert!(matches!(
            ServerMessage::parse("not json"),
            ServerMessage::Unknown
        ));
    }

    #[test]
    fn a_socket_error_keeps_its_code() {
        let message = ServerMessage::parse(
            r#"{"channel":"system","type":"error","data":{"code":"UNAUTHORIZED","message":"no"}}"#,
        );
        match message {
            ServerMessage::Error(error) => {
                assert_eq!(error.code, crate::api::error::ErrorCode::Unauthorized);
            }
            other => panic!("expected an error, got {other:?}"),
        }
    }

    #[test]
    fn client_commands_serialise_the_way_the_contract_writes_them() {
        let subscribe = serde_json::to_string(&ClientMessage::Subscribe {
            channels: vec!["orderbook:BTC-PERP".into(), "ticker".into()],
        })
        .unwrap_or_default();
        assert_eq!(
            subscribe,
            r#"{"op":"subscribe","channels":["orderbook:BTC-PERP","ticker"]}"#
        );
        assert_eq!(
            serde_json::to_string(&ClientMessage::Ping).unwrap_or_default(),
            r#"{"op":"ping"}"#
        );
    }
}
