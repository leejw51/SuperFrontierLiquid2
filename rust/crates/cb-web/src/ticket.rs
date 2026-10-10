//! The order ticket's state, and the rules that decide whether it may be sent.
//!
//! Validation lives here rather than in the view for one reason that the design
//! system states outright: **a disabled trading button never hides its reason.**
//! The hint beside a blocked submit and the sentence the gateway sends back on a
//! rejection have to agree, so the client-side guard is written against the same
//! fields the contract's `details` object carries — `min_notional`, `tick_size`,
//! `lot_size`, the available margin — and produces the same numbers.
//!
//! The ticket is also where a toast's one-tap remedy lands, which is why it is a
//! context store rather than component-local state: the toast that offers
//! *Reprice to 64,250.90* is rendered by the shell, not by the ticket.

use leptos::prelude::*;
use rust_decimal::Decimal;

use crate::api::types::*;
use crate::format;
use crate::state::ToastAction;

/// Why an order cannot be sent, in the interface's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// No size entered yet. Not an error, just nothing to do.
    Empty,
    /// The market is not accepting orders.
    NotTrading(MarketStatus),
    /// The size is not a whole number of lots.
    Lot(Decimal),
    /// The price is not a whole number of ticks.
    Tick(Decimal),
    /// The order is worth less than the market's minimum.
    MinNotional(Decimal),
    /// The order needs more margin than the account has free.
    Margin {
        /// What the order needs.
        required: Decimal,
        /// What is available.
        available: Decimal,
    },
    /// A spot order needs more of an asset than the account holds free: the
    /// quote asset to buy, the base asset to sell. Spot has no margin.
    Balance {
        /// The asset the order spends.
        asset: String,
        /// What the order needs of it.
        required: Decimal,
        /// What is free.
        available: Decimal,
    },
    /// A limit order with no price.
    NoPrice,
}

impl Blocked {
    /// The hint shown beside the disabled button.
    ///
    /// It names the number, because "invalid size" tells the user nothing they
    /// can act on and "size steps by 0.0001" tells them exactly what to change.
    #[must_use]
    pub fn hint(&self, size_dp: u32, price_dp: u32) -> String {
        match self {
            Self::Empty => "Enter a size".to_string(),
            Self::NotTrading(status) => format!(
                "This market is {} and is not accepting orders",
                status.label().to_lowercase()
            ),
            Self::Lot(lot) => format!("Size steps by {}", format::fixed(*lot, size_dp)),
            Self::Tick(tick) => format!("Price steps by {}", format::fixed(*tick, price_dp)),
            Self::MinNotional(min) => {
                format!("Orders start at {} USD here", format::fixed(*min, 2))
            }
            Self::Margin {
                required,
                available,
            } => format!(
                "Needs {} USD of margin, {} free",
                format::fixed(*required, 2),
                format::fixed(*available, 2)
            ),
            Self::Balance {
                asset,
                required,
                available,
            } => format!(
                "Needs {} {asset} free, have {}",
                format::fixed(*required, if asset == "USD" { 2 } else { size_dp }),
                format::fixed(*available, if asset == "USD" { 2 } else { size_dp }),
            ),
            Self::NoPrice => "Enter a limit price".to_string(),
        }
    }
}

/// What the ticket is about to send.
#[derive(Debug, Clone, Copy)]
pub struct Ticket {
    /// Buy or sell. The single most consequential choice on the screen, which
    /// is why its segmented control is the one allowed to take a market colour.
    pub side: RwSignal<Side>,
    /// Limit or market.
    pub order_type: RwSignal<OrderType>,
    /// The limit price, as typed.
    pub price: RwSignal<String>,
    /// The quantity, as typed.
    pub qty: RwSignal<String>,
    /// Leverage, for derivative markets.
    pub leverage: RwSignal<u32>,
    /// Cross or isolated.
    pub margin_mode: RwSignal<MarginMode>,
    /// Refuse rather than take liquidity.
    pub post_only: RwSignal<bool>,
    /// Refuse rather than grow the position.
    pub reduce_only: RwSignal<bool>,
    /// Time in force.
    pub tif: RwSignal<TimeInForce>,
    /// True while a submission is in flight.
    pub sending: RwSignal<bool>,
}

impl Default for Ticket {
    fn default() -> Self {
        Self::new()
    }
}

impl Ticket {
    /// A ticket with the defaults the reference shows.
    #[must_use]
    pub fn new() -> Self {
        Self {
            side: RwSignal::new(Side::Buy),
            order_type: RwSignal::new(OrderType::Limit),
            price: RwSignal::new(String::new()),
            qty: RwSignal::new(String::new()),
            leverage: RwSignal::new(10),
            margin_mode: RwSignal::new(MarginMode::Cross),
            post_only: RwSignal::new(false),
            reduce_only: RwSignal::new(false),
            tif: RwSignal::new(TimeInForce::Gtc),
            sending: RwSignal::new(false),
        }
    }

    /// The typed price as a decimal, if it parses.
    #[must_use]
    pub fn price_value(&self) -> Option<Decimal> {
        parse_typed(&self.price.get())
    }

    /// The typed quantity as a decimal, if it parses.
    #[must_use]
    pub fn qty_value(&self) -> Option<Decimal> {
        parse_typed(&self.qty.get())
    }
}

/// Parses a number the way a person types it: with separators, and possibly
/// half-finished.
///
/// Returns `None` rather than zero for anything unparseable, because a zero that
/// came from a typo and a zero the user meant are not the same number.
#[must_use]
pub fn parse_typed(value: &str) -> Option<Decimal> {
    let cleaned: String = value
        .chars()
        .filter(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
        .collect();
    if cleaned.is_empty() || cleaned == "-" || cleaned == "." {
        return None;
    }
    cleaned.parse().ok()
}

/// True when `value` is a whole multiple of `step`.
#[must_use]
pub fn is_multiple_of(value: Decimal, step: Decimal) -> bool {
    if step <= Decimal::ZERO {
        return true;
    }
    (value % step).is_zero()
}

/// Checks an order against the market's own increments and what the account
/// can spend.
///
/// Every number this returns comes from a field the contract sends, so the hint
/// under a disabled button and the gateway's rejection say the identical thing.
///
/// `available` means two different things, and the caller supplies the right
/// one: on a derivative it is the account's free margin; on a spot market it
/// is the free balance of the asset the order spends — the quote asset for a
/// buy, the base asset for a sell. Spot has no margin, and checking a spot buy
/// against `available_margin` blocked every funded account's first order with
/// "Needs 30.00 USD of margin, 0.00 free" while it held a hundred thousand.
///
/// The argument list is long because the check is against eight independent
/// facts; bundling them into a struct would hide which of them a given call site
/// has actually supplied.
#[allow(clippy::too_many_arguments)]
pub fn validate(
    market: &Market,
    side: Side,
    order_type: OrderType,
    price: Option<Decimal>,
    qty: Option<Decimal>,
    leverage: Decimal,
    available: Decimal,
    reference_price: Decimal,
) -> Result<Decimal, Blocked> {
    if !market.status.is_tradable() {
        return Err(Blocked::NotTrading(market.status));
    }
    let Some(qty) = qty.filter(|qty| *qty > Decimal::ZERO) else {
        return Err(Blocked::Empty);
    };
    if !is_multiple_of(qty, market.lot_size) {
        return Err(Blocked::Lot(market.lot_size));
    }

    let effective_price = match order_type {
        OrderType::Market => reference_price,
        OrderType::Limit => {
            let Some(price) = price.filter(|price| *price > Decimal::ZERO) else {
                return Err(Blocked::NoPrice);
            };
            if !is_multiple_of(price, market.tick_size) {
                return Err(Blocked::Tick(market.tick_size));
            }
            price
        }
    };

    let notional = effective_price * qty;
    if notional < market.min_notional {
        return Err(Blocked::MinNotional(market.min_notional));
    }

    if market.kind.is_derivative() {
        let required = notional / leverage.max(Decimal::ONE);
        if required > available {
            return Err(Blocked::Margin {
                required: required.round_dp(2),
                available,
            });
        }
    } else {
        // A spot buy spends the quote asset; a spot sell spends the base.
        let (asset, required) = match side {
            Side::Buy => (market.quote.clone(), notional),
            Side::Sell => (market.base.clone(), qty),
        };
        if required > available {
            return Err(Blocked::Balance {
                asset,
                required,
                available,
            });
        }
    }

    Ok(notional)
}

/// Checks a handle against the contract's rule: 3–20 of `[a-z0-9_]`.
///
/// Validated as the user types so that `409 HANDLE_TAKEN` is the only failure
/// the server ever has to report.
#[must_use]
pub fn handle_problem(handle: &str) -> Option<&'static str> {
    if handle.is_empty() {
        return Some("Pick a handle");
    }
    if handle.chars().count() < 3 {
        return Some("At least three characters");
    }
    if handle.chars().count() > 20 {
        return Some("At most twenty characters");
    }
    if !handle
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
    {
        return Some("Lower case letters, digits and underscores only");
    }
    None
}

/// Puts the ticket in context.
pub fn provide_ticket(ticket: Ticket) {
    provide_context(ticket);
}

/// What a spot order can spend: the free balance of the asset it pays with.
///
/// A buy pays in the quote asset, a sell in the base. Nothing here is margin.
#[must_use]
pub fn spendable(account: &Account, market: &Market, side: Side) -> Decimal {
    let asset = match side {
        Side::Buy => market.quote.as_str(),
        Side::Sell => market.base.as_str(),
    };
    account
        .balances
        .iter()
        .find(|balance| balance.asset == asset)
        .map_or(Decimal::ZERO, |balance| balance.free)
}

/// Reads the ticket from context, or a fresh one outside the terminal.
#[must_use]
pub fn use_ticket() -> Ticket {
    use_context::<Ticket>().unwrap_or_default()
}

/// Applies a toast's one-tap remedy to the ticket.
///
/// Returns the route the caller should navigate to, where the remedy is a
/// change of screen rather than a change of field. The navigation itself is the
/// caller's job because it has to be a client-side route change: a
/// `location.href` here would reload the page and lose the backend the user is
/// signed in to. See [`crate::nav`].
#[must_use]
pub fn apply_remedy(action: &ToastAction) -> Option<&'static str> {
    let ticket = use_ticket();
    match action {
        ToastAction::Reprice(price) => {
            ticket.price.set(price.to_string());
            None
        }
        ToastAction::ReduceSize => {
            if let Some(qty) = ticket.qty_value() {
                ticket.qty.set((qty / Decimal::TWO).to_string());
            }
            None
        }
        ToastAction::OpenFaucet => Some("/faucet"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::fixtures;

    fn btc_perp() -> Market {
        fixtures::markets()
            .into_iter()
            .find(|market| market.symbol == "BTC-PERP")
            .unwrap_or_else(|| unreachable!("BTC-PERP is in the fixtures"))
    }

    fn d(s: &str) -> Decimal {
        s.parse().unwrap_or_default()
    }

    fn btc_usd() -> Market {
        fixtures::markets()
            .into_iter()
            .find(|market| market.symbol == "BTC-USD")
            .unwrap_or_else(|| unreachable!("BTC-USD is in the fixtures"))
    }

    /// The failure this pins: a spot buy was checked against `available_margin`,
    /// which a spot-only account never has, so a user holding a hundred
    /// thousand dollars was told they needed thirty dollars of margin.
    #[test]
    fn a_spot_buy_is_checked_against_the_free_quote_balance_not_margin() {
        let market = btc_usd();
        let price = market.tick_size * d("3000000");
        let qty = market.lot_size * d("100");
        let ok = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(price),
            Some(qty),
            d("1"),
            d("100000.00"), // free USD
            price,
        );
        assert!(ok.is_ok(), "{ok:?}");

        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(price),
            Some(qty),
            d("1"),
            d("0.00"),
            price,
        )
        .expect_err("no USD, no buy");
        match blocked {
            Blocked::Balance { asset, .. } => assert_eq!(asset, "USD"),
            other => panic!("expected a balance block, got {other:?}"),
        }
    }

    #[test]
    fn a_spot_sell_needs_the_base_asset_free() {
        let market = btc_usd();
        let price = market.tick_size * d("3000000");
        let qty = market.lot_size * d("100");
        let blocked = validate(
            &market,
            Side::Sell,
            OrderType::Limit,
            Some(price),
            Some(qty),
            d("1"),
            qty / d("2"), // half the coins
            price,
        )
        .expect_err("cannot sell what is not held");
        match blocked {
            Blocked::Balance {
                asset, required, ..
            } => {
                assert_eq!(asset, "BTC");
                assert_eq!(required, qty);
            }
            other => panic!("expected a balance block, got {other:?}"),
        }
        let ok = validate(
            &market,
            Side::Sell,
            OrderType::Limit,
            Some(price),
            Some(qty),
            d("1"),
            qty,
            price,
        );
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn typed_numbers_survive_separators_and_half_finished_input() {
        assert_eq!(parse_typed("64,250.50"), Some(d("64250.50")));
        assert_eq!(parse_typed("0.5000"), Some(d("0.5000")));
        assert_eq!(parse_typed(""), None);
        assert_eq!(parse_typed("."), None);
        assert_eq!(parse_typed("abc"), None);
    }

    #[test]
    fn a_size_off_the_lot_is_refused_and_says_the_lot() {
        let market = btc_perp();
        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("64250.50")),
            Some(d("0.00005")),
            d("10"),
            d("56086.00"),
            d("64250.50"),
        )
        .expect_err("half a lot is not a size");
        assert_eq!(blocked, Blocked::Lot(d("0.0001")));
        assert_eq!(blocked.hint(4, 2), "Size steps by 0.0001");
    }

    #[test]
    fn a_price_off_the_tick_is_refused_and_says_the_tick() {
        let market = btc_perp();
        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("64250.55")),
            Some(d("0.5000")),
            d("10"),
            d("56086.00"),
            d("64250.50"),
        )
        .expect_err("half a tick is not a price");
        assert_eq!(blocked, Blocked::Tick(d("0.10")));
    }

    #[test]
    fn an_order_below_the_minimum_names_the_minimum() {
        let market = btc_perp();
        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("64250.50")),
            Some(d("0.0001")),
            d("10"),
            d("56086.00"),
            d("64250.50"),
        )
        .expect_err("six dollars is under the ten dollar minimum");
        assert_eq!(blocked, Blocked::MinNotional(d("10.00")));
    }

    #[test]
    fn margin_is_checked_against_leverage_and_reports_both_figures() {
        let market = btc_perp();
        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("64250.50")),
            Some(d("100.0000")),
            d("10"),
            d("56086.00"),
            d("64250.50"),
        )
        .expect_err("ten million of notional does not fit");
        match blocked {
            Blocked::Margin {
                required,
                available,
            } => {
                assert_eq!(required, d("642505.00"));
                assert_eq!(available, d("56086.00"));
            }
            other => panic!("expected a margin block, got {other:?}"),
        }
    }

    #[test]
    fn a_halted_market_blocks_before_anything_else_is_checked() {
        let market = fixtures::markets()
            .into_iter()
            .find(|market| market.symbol == "TSLA-PERP")
            .unwrap_or_else(|| unreachable!("TSLA-PERP is halted in the fixtures"));
        let blocked = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("246.00")),
            Some(d("80.0000")),
            d("10"),
            d("56086.00"),
            d("248.90"),
        )
        .expect_err("a halted market takes no orders");
        assert_eq!(blocked, Blocked::NotTrading(MarketStatus::Halted));
        assert_eq!(
            blocked.hint(4, 2),
            "This market is halted and is not accepting orders"
        );
    }

    #[test]
    fn a_good_order_passes_and_returns_its_notional() {
        let market = btc_perp();
        let notional = validate(
            &market,
            Side::Buy,
            OrderType::Limit,
            Some(d("64250.50")),
            Some(d("0.5000")),
            d("10"),
            d("56086.00"),
            d("64250.50"),
        )
        .expect("this is a valid order");
        assert_eq!(notional, d("32125.250000"));
    }

    #[test]
    fn handles_follow_the_contracts_rule() {
        assert_eq!(handle_problem("satoshi"), None);
        assert_eq!(handle_problem("a_1"), None);
        assert!(handle_problem("ab").is_some());
        assert!(handle_problem("Satoshi").is_some());
        assert!(handle_problem("sat oshi").is_some());
        assert!(handle_problem(&"a".repeat(21)).is_some());
    }
}
