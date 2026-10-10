//! The tests that have to pass in a real browser.
//!
//! The unit tests inside `src/` cover each module's own rules. This file covers
//! the handful of facts that are load-bearing across the whole client and that
//! a reviewer should be able to run in the target environment rather than on the
//! host:
//!
//! * the **derivation vector**. If the canonical BIP-39 phrase does not produce
//!   `0x9858EfFD232B4033E47d90003D41EC34EcaEda94` at index 0, this client derives
//!   different addresses from MetaMask, `cb-client` and the SuperFrontier Wallet,
//!   and every account it creates is stranded. Nothing else in the crate matters
//!   if this fails;
//! * the **signature round trip**, because the gateway authenticates by
//!   recovering the signer and comparing it to the claimed address;
//! * **order-book delta application**, especially removal at quantity zero and
//!   the drop of any delta that races the snapshot after a reconnect;
//! * **reconnection timing**, so the backoff cannot quietly become a busy loop;
//! * **decimal formatting**, because a price that renders differently in two
//!   places is a price nobody can trust;
//! * **order validation**, because the hint under a disabled button and the
//!   gateway's rejection have to name the same number.
//!
//! `cargo test --target wasm32-unknown-unknown` runs them under Node through
//! `wasm-bindgen-test-runner`, which the crate's `.cargo/config.toml` wires up;
//! `wasm-pack test --headless --chrome` runs the identical suite inside a real
//! browser. They also compile and run on the host with a plain `cargo test`, so
//! no environment can skip them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use cb_web::api::types::{Level, Market, MarketStatus, OrderBookPayload, OrderType, Side};
use cb_web::format;
use cb_web::ticket::{validate, Blocked};
use cb_web::wallet::{eip191_hash, is_valid_address, recover_address, to_eip55, Wallet};
use cb_web::ws::backoff::Backoff;
use cb_web::ws::book::{depth_pct, DeltaOutcome, OrderBook};
use rust_decimal::Decimal;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen_test::wasm_bindgen_test;

/// Marks a test so it runs both in a browser and on the host.
macro_rules! browser_test {
    ($(#[$meta:meta])* fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test)]
        #[cfg_attr(not(target_arch = "wasm32"), test)]
        fn $name() $body
    };
}

/// The all-zero-entropy phrase every EVM tool derives from.
const CANONICAL_PHRASE: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// The address that phrase must produce at `m/44'/60'/0'/0/0`.
const CANONICAL_ADDRESS: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

fn d(value: &str) -> Decimal {
    Decimal::from_str(value).expect("a decimal literal in a test")
}

fn level(price: &str, qty: &str) -> Level {
    Level(d(price), d(qty))
}

fn btc_perp() -> Market {
    cb_web::api::fixtures::markets()
        .into_iter()
        .find(|market| market.symbol == "BTC-PERP")
        .expect("BTC-PERP is in the fixtures")
}

// ------------------------------------------------------------------ identity

browser_test! {
    /// **The one that matters most.** A derivation that does not reproduce this
    /// address is a different derivation from the rest of the ecosystem, and the
    /// accounts it creates cannot be reached from any other wallet.
    fn the_canonical_phrase_derives_the_canonical_address() {
        let wallet = Wallet::from_phrase(CANONICAL_PHRASE, 0).expect("the canonical phrase parses");
        assert_eq!(wallet.address(), CANONICAL_ADDRESS);
    }
}

browser_test! {
    /// The same phrase at another index is another account, not an error.
    fn each_index_is_its_own_account() {
        let zero = Wallet::from_phrase(CANONICAL_PHRASE, 0).expect("valid");
        let one = Wallet::from_phrase(CANONICAL_PHRASE, 1).expect("valid");
        assert_ne!(zero.address(), one.address());
        assert!(is_valid_address(one.address()));
    }
}

browser_test! {
    /// The gateway authenticates by recovering the signer, so a signature this
    /// client produces has to recover to the address it claims.
    fn a_signature_recovers_to_the_address_that_made_it() {
        let wallet = Wallet::from_phrase(CANONICAL_PHRASE, 0).expect("valid");
        let message = "SuperFrontierLiquid wants you to sign in.\n\nAddress: \
                       0x9858EfFD232B4033E47d90003D41EC34EcaEda94\nNonce: 7f2a91c4e8";
        let signature = wallet.sign_personal(message).expect("signing works");
        assert_eq!(
            recover_address(message, &signature).as_deref(),
            Some(CANONICAL_ADDRESS),
        );
        // And a signature over one message must not validate against another.
        assert_ne!(
            recover_address("a different message", &signature).as_deref(),
            Some(CANONICAL_ADDRESS),
        );
    }
}

browser_test! {
    /// EIP-191 prefixes the **byte** length. Counting characters would produce a
    /// valid signature over the wrong digest for any non-ASCII message.
    fn the_personal_sign_prefix_counts_bytes() {
        let hash = eip191_hash("é");
        let expected = cb_web::wallet::keccak256(b"\x19Ethereum Signed Message:\n2\xc3\xa9");
        assert_eq!(hash, expected);
    }
}

browser_test! {
    /// A mixed-case address whose checksum does not validate is a typo, and
    /// crediting it would pay somebody else.
    fn eip55_catches_a_broken_checksum() {
        let bytes = hex_20(CANONICAL_ADDRESS);
        assert_eq!(to_eip55(&bytes), CANONICAL_ADDRESS);
        assert!(is_valid_address(&CANONICAL_ADDRESS.to_lowercase()));
        assert!(!is_valid_address("0x9858EFFD232B4033E47d90003D41EC34EcaEda94"));
    }
}

/// Decodes a `0x`-prefixed address into twenty bytes.
fn hex_20(value: &str) -> [u8; 20] {
    let body = value.strip_prefix("0x").unwrap_or(value);
    let mut out = [0_u8; 20];
    for (index, slot) in out.iter_mut().enumerate() {
        let pair = &body[index * 2..index * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).expect("hex");
    }
    out
}

// ---------------------------------------------------------------- order book

browser_test! {
    /// The contract's own delta example: a level at quantity `"0"` is removed,
    /// not zeroed in place.
    fn a_delta_at_quantity_zero_removes_the_level() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&OrderBookPayload {
            height: 184_203,
            bids: vec![level("64250.00", "1.2500"), level("64249.50", "3.0000")],
            asks: vec![level("64251.00", "0.8020")],
        });
        assert_eq!(book.best_bid(), Some(d("64250.00")));

        let outcome = book.apply_delta(&OrderBookPayload {
            height: 184_204,
            bids: vec![level("64250.00", "0.00"), level("64249.00", "2.00")],
            asks: vec![],
        });

        assert_eq!(outcome, DeltaOutcome::Applied);
        assert_eq!(book.best_bid(), Some(d("64249.50")));
        let prices: Vec<Decimal> = book.bids(10, None).iter().map(|row| row.price).collect();
        assert!(!prices.contains(&d("64250.00")), "the level must be gone");
        assert!(prices.contains(&d("64249.00")), "the new level must be there");
    }
}

browser_test! {
    /// On every reconnect the book waits for a fresh snapshot, and any delta
    /// that races it is dropped. Applying one would leave the book holding
    /// levels that never existed, which is the failure this rule exists for.
    fn a_reconnect_drops_deltas_until_the_snapshot_lands() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&OrderBookPayload {
            height: 1,
            bids: vec![level("100.00", "1.0")],
            asks: vec![level("101.00", "1.0")],
        });

        book.expect_snapshot();
        assert!(book.is_awaiting_snapshot());
        assert_eq!(
            book.apply_delta(&OrderBookPayload {
                height: 2,
                bids: vec![level("100.00", "0")],
                asks: vec![],
            }),
            DeltaOutcome::DroppedAwaitingSnapshot,
        );
        // Nothing is hidden meanwhile: the last known book stays on screen.
        assert_eq!(book.best_bid(), Some(d("100.00")));

        book.apply_snapshot(&OrderBookPayload {
            height: 9,
            bids: vec![level("99.00", "2.0")],
            asks: vec![level("99.50", "2.0")],
        });
        assert!(!book.is_awaiting_snapshot());
        assert_eq!(book.best_bid(), Some(d("99.00")));
    }
}

browser_test! {
    /// Depth bars are cumulative and scaled against the larger side, so the two
    /// sides of the book are directly comparable.
    fn depth_bars_are_cumulative_and_comparable() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&OrderBookPayload {
            height: 1,
            bids: vec![
                level("64250.00", "1.2500"),
                level("64249.50", "3.0000"),
                level("64249.10", "0.8400"),
            ],
            asks: vec![level("64251.00", "0.8020")],
        });
        let bids = book.bids(3, None);
        let asks = book.asks(3, None);
        assert_eq!(bids[2].cumulative, d("5.0900"));
        let scale = OrderBook::scale(&bids, &asks);
        assert_eq!(depth_pct(bids[2].cumulative, scale), 100);
        assert_eq!(depth_pct(asks[0].cumulative, scale), 16);
    }
}

// -------------------------------------------------------------- reconnection

browser_test! {
    /// Fast first, then backing off to a ceiling. A schedule that collapsed to
    /// zero would be a busy loop against a gateway that has just fallen over.
    fn the_backoff_schedule_grows_and_is_bounded() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay_ms(500), 500);
        assert_eq!(backoff.next_delay_ms(500), 1_000);
        assert_eq!(backoff.next_delay_ms(500), 2_000);
        assert_eq!(backoff.attempt(), 3);
        for _ in 0..20 {
            assert!(backoff.next_delay_ms(500) <= cb_web::ws::backoff::CAP_MS);
        }
        backoff.reset();
        assert_eq!(backoff.attempt(), 0);
        assert_eq!(backoff.next_delay_ms(500), cb_web::ws::backoff::BASE_MS);
    }
}

// ---------------------------------------------------------------- formatting

browser_test! {
    /// Every numeric rule in the design system, in one place.
    fn numbers_render_the_way_the_design_system_says() {
        assert_eq!(format::fixed(d("64250.50"), 2), "64,250.50");
        assert_eq!(format::size(d("1.25"), 4), "1.2500", "trailing zeros are kept");
        assert_eq!(format::pnl(d("1872.15")), "+1,872.15", "the sign is explicit");
        assert_eq!(format::pnl(d("-2500")), "\u{2212}2,500.00", "a minus, not a hyphen");
        assert_eq!(format::pct(d("-3.42")), "\u{2212}3.42%");
        assert_eq!(format::funding_pct(d("0.000125")), "+0.0125%");
        assert_eq!(format::count(184_203), "184,203", "a block height is never abbreviated");
        assert_eq!(format::notional(d("128450000.00")), "128.4M");
        assert_eq!(format::hash(CANONICAL_ADDRESS), "0x9858\u{2026}da94");
    }
}

// ---------------------------------------------------------------- validation

browser_test! {
    /// A blocked order names the market's own increment, which is the same
    /// figure the gateway's `details` object carries.
    fn a_blocked_order_names_the_number_that_blocked_it() {
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
}

browser_test! {
    /// A market that is not trading blocks before anything else is looked at,
    /// and the reason is the one the ticket shows.
    fn a_halted_market_blocks_first() {
        let market = cb_web::api::fixtures::markets()
            .into_iter()
            .find(|market| market.symbol == "TSLA-PERP")
            .expect("TSLA-PERP is halted in the fixtures");
        assert_eq!(market.status, MarketStatus::Halted);
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
        assert_eq!(
            blocked.hint(4, 2),
            "This market is halted and is not accepting orders",
        );
    }
}

browser_test! {
    /// A good order passes and hands back the notional the ticket displays.
    fn a_good_order_passes() {
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
        .expect("this order is valid");
        assert_eq!(format::fixed(notional, 2), "32,125.25");
    }
}

// ---------------------------------------------------------------------------
// Empty is not loading.
//
// The single highest-value distinction in this client on a chain with no data
// in it. `OrderBook::is_ready` conflated the two, so a market with no orders
// shimmered a skeleton forever, promising data that was never coming.
// ---------------------------------------------------------------------------

browser_test! {
    /// A book that has not had its snapshot yet is loading, not empty.
    fn a_book_awaiting_its_snapshot_is_loading() {
        let book = OrderBook::new();
        assert!(book.is_awaiting_snapshot(), "a new book wants a snapshot");
        assert!(!book.is_ready());
    }
}

browser_test! {
    /// A book whose snapshot arrived with nothing in it is empty, not loading.
    ///
    /// This is the state every first visitor is in, and the one that used to
    /// draw a skeleton.
    fn an_empty_snapshot_makes_the_book_empty_rather_than_loading() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&OrderBookPayload {
            height: 1,
            bids: Vec::new(),
            asks: Vec::new(),
        });
        assert!(
            !book.is_awaiting_snapshot(),
            "the snapshot landed, so nothing is outstanding",
        );
        assert!(book.is_empty(), "an empty book is empty");
        assert!(!book.is_ready());
    }
}

browser_test! {
    /// A book with levels in it is neither loading nor empty.
    fn a_populated_book_is_neither_loading_nor_empty() {
        let mut book = OrderBook::new();
        book.apply_snapshot(&OrderBookPayload {
            height: 1,
            bids: vec![level("64250.00", "1.0")],
            asks: vec![level("64251.00", "1.0")],
        });
        assert!(!book.is_awaiting_snapshot());
        assert!(!book.is_empty());
        assert!(book.is_ready());
    }
}

browser_test! {
    /// `Load` distinguishes the three answers a region can have.
    fn a_region_that_failed_is_not_a_region_that_is_loading() {
        use cb_web::state::Load;

        assert!(Load::Loading.is_loading());
        assert!(!Load::Ready.is_loading());
        assert!(!Load::Failed("cursor is not valid".to_string()).is_loading());
        assert_eq!(
            Load::Failed("cursor is not valid".to_string()).failure(),
            Some("cursor is not valid"),
        );
        assert_eq!(Load::Ready.failure(), None);

        let ok: Result<(), std::io::Error> = Ok(());
        assert_eq!(Load::from_result(&ok), Load::Ready);
        let bad: Result<(), String> = Err("the gateway did not answer".to_string());
        assert!(Load::from_result(&bad).failure().is_some());
    }
}

// ---------------------------------------------------------------------------
// The connection indicator against a server that stops answering.
//
// A gateway frozen with `kill -STOP` never closes anything, so the socket
// stays `Live` while nothing at all arrives. The indicator is therefore derived
// from the age of the last frame as well as from the socket's own state.
// ---------------------------------------------------------------------------

browser_test! {
    /// A fresh frame is live, and says how fresh.
    fn a_recent_frame_reads_live() {
        use cb_web::state::feed_report;
        use cb_web::ws::ConnectionState;

        let report = feed_report(
            &ConnectionState::Live { since_ms: 1_000 },
            100_000,
            101_000,
        );
        assert_eq!(report.attr, "live");
        assert_eq!(report.word, "Live");
        assert!(!report.frozen);
        assert!(!report.figure.is_empty(), "a state without a figure is a bare dot");
    }
}

browser_test! {
    /// A minute of silence is brass and says so, on every screen — while
    /// twenty-eight seconds, which is what an idle but healthy socket actually
    /// reaches between pongs, is still live.
    fn a_silent_minute_stops_reading_live() {
        use cb_web::state::feed_report;
        use cb_web::ws::ConnectionState;

        let healthy = feed_report(
            &ConnectionState::Live { since_ms: 1_000 },
            100_000,
            128_000,
        );
        assert_eq!(healthy.attr, "live", "an idle chain still pongs every 30s");

        let report = feed_report(
            &ConnectionState::Live { since_ms: 1_000 },
            100_000,
            160_000,
        );
        assert_eq!(report.attr, "stale");
        assert_eq!(report.word, "Stale");
        assert!(report.frozen, "a stale feed stops prices looking live");
    }
}

browser_test! {
    /// Past ninety seconds it is vermilion and reads Offline, as §9 requires —
    /// which is the case the reviewer measured at a hundred and ten seconds and
    /// found still reading `Live · 1m`.
    fn ninety_seconds_of_silence_reads_offline() {
        use cb_web::state::feed_report;
        use cb_web::ws::ConnectionState;

        let report = feed_report(
            &ConnectionState::Live { since_ms: 1_000 },
            100_000,
            210_000,
        );
        assert_eq!(report.attr, "down");
        assert_eq!(report.word, "Offline");
        assert!(report.frozen);
        assert!(
            report.figure.contains("silent since"),
            "Offline names when it went quiet, got {:?}",
            report.figure,
        );
    }
}

browser_test! {
    /// A socket that has opened but not yet said anything reads *Connecting*,
    /// never *Live*, and its age is measured from the connection rather than
    /// from a `last_frame_ms` of zero.
    ///
    /// The browser hands back a `WebSocket` before the handshake completes, so
    /// against a stopped server the connection "succeeds" and then carries
    /// nothing. Claiming *Live* there is what let a frozen gateway keep a jade
    /// dot through every ninety-second reconnection.
    fn a_connection_with_no_frames_yet_never_claims_live() {
        use cb_web::state::feed_report;
        use cb_web::ws::ConnectionState;

        // `last_frame_ms` is seeded with the application's start, so a socket
        // that opened a moment later has an age of a moment.
        let report = feed_report(
            &ConnectionState::Live { since_ms: 500_000 },
            499_900,
            500_400,
        );
        assert_eq!(report.word, "Connecting");
        assert_ne!(report.attr, "down");
        assert!(report.frozen);

        // A reconnection to a server that is still not answering: the old
        // `last_frame_ms` is before this connection, so the clock does not
        // restart on it and ninety seconds later it is Offline again.
        let report = feed_report(
            &ConnectionState::Live { since_ms: 500_000 },
            100_000,
            600_000,
        );
        assert_eq!(report.word, "Offline");
    }
}

// ---------------------------------------------------------------------------
// The sign-in nonce is single use.
//
// The sequence every new user hit: sign → `422 HANDLE_REQUIRED` → type a
// handle → sign the **same** challenge again → `401`, with an error written for
// a programmer, while the account had in fact been created. The client now asks
// for a fresh challenge before the retry.
//
// Browser-only, because the mock gateway reads the clock through `js_sys`.
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
mod nonce {
    use cb_web::api::mock::MockApi;
    use cb_web::api::{Api, ErrorCode};
    use cb_web::wallet::Wallet;
    use wasm_bindgen_test::wasm_bindgen_test;

    /// A phrase with no account behind it, so the handle path is exercised.
    const NEW_PHRASE: &str =
        "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[wasm_bindgen_test]
    async fn signing_a_spent_nonce_fails_and_a_fresh_one_succeeds() {
        let api = MockApi::new();
        let wallet = Wallet::from_phrase(NEW_PHRASE, 0).expect("a valid phrase");
        let address = wallet.address().to_string();

        // 1. First attempt, with no handle: the gateway asks for one, and
        //    spends the nonce doing it.
        let first = api
            .challenge(&address)
            .await
            .expect("a challenge is issued");
        let signature = wallet
            .sign_personal(&first.message)
            .expect("the key signs its own challenge");
        let refused = api
            .login(&address, &signature, None)
            .await
            .expect_err("a new address needs a handle");
        assert_eq!(refused.code(), Some(ErrorCode::HandleRequired));

        // 2. Re-signing the same challenge is what the client used to do, and
        //    it cannot work: the nonce is gone.
        let again = api
            .login(&address, &signature, Some("jade_tester"))
            .await
            .expect_err("a spent nonce is not a nonce");
        assert_ne!(
            again.code(),
            Some(ErrorCode::HandleRequired),
            "the second failure is the spent nonce, not the handle",
        );

        // 3. A fresh challenge, signed, with the handle: this is the fix, and
        //    it is the step the user was never offered.
        let fresh = api
            .challenge(&address)
            .await
            .expect("a second challenge is issued");
        assert_ne!(fresh.nonce, first.nonce, "each challenge is its own nonce");
        let signature = wallet
            .sign_personal(&fresh.message)
            .expect("the key signs the fresh challenge");
        let session = api
            .login(&address, &signature, Some("jade_tester"))
            .await
            .expect("a fresh nonce with a handle signs in");
        assert_eq!(session.handle, "jade_tester");
    }
}
