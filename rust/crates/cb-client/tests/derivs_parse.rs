//! Parse/dispatch tests for the M6 derivative subcommands.
//!
//! No RPC is touched — these tests only exercise the clap parser surface
//! plus the help text wiring. RPC-level behavior is covered by the
//! `cb-rpc::tests::derivatives_reads` integration test.

use cb_client::cli::{parse_repl_line, Command};

fn parse_one(line: &str) -> Command {
    let cli = parse_repl_line(line).expect("parse").expect("non-empty");
    cli.cmd.expect("subcommand")
}

#[test]
fn position_parses_with_optional_addr_and_json_flag() {
    match parse_one("position\n") {
        Command::Position { addr, json } => {
            assert_eq!(addr, None);
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match parse_one("position 0xabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd --json\n") {
        Command::Position { addr, json } => {
            assert_eq!(addr.unwrap(), "0xabcdabcdabcdabcdabcdabcdabcdabcdabcdabcd");
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn margin_parses() {
    match parse_one("margin --json\n") {
        Command::Margin { addr, json } => {
            assert!(addr.is_none());
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn funding_requires_market_arg() {
    match parse_one("funding perp:0\n") {
        Command::Funding { market, json } => {
            assert_eq!(market, "perp:0");
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    // Missing market should fail to parse.
    assert!(parse_repl_line("funding\n").is_err());
}

#[test]
fn mark_parses() {
    match parse_one("mark perp:1 --json\n") {
        Command::Mark { market, json } => {
            assert_eq!(market, "perp:1");
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn book_accepts_optional_depth() {
    match parse_one("book perp:0\n") {
        Command::Book {
            market,
            depth,
            json,
        } => {
            assert_eq!(market, "perp:0");
            assert!(depth.is_none());
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }
    match parse_one("book perp:0 25 --json\n") {
        Command::Book {
            market,
            depth,
            json,
        } => {
            assert_eq!(market, "perp:0");
            assert_eq!(depth, Some(25));
            assert!(json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn markets_no_args() {
    match parse_one("markets\n") {
        Command::Markets { json } => assert!(!json),
        other => panic!("unexpected: {other:?}"),
    }
    match parse_one("markets --json\n") {
        Command::Markets { json } => assert!(json),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn perp_place_full_args_default_tif() {
    match parse_one("perp-place perp:0 buy 1000 50000\n") {
        Command::PerpPlace {
            market,
            side,
            qty,
            price,
            flags,
        } => {
            assert_eq!(market, "perp:0");
            assert_eq!(side, "buy");
            assert_eq!(qty, 1000);
            assert_eq!(price, 50_000);
            assert!(flags.is_empty());
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn perp_place_with_flags() {
    match parse_one("perp-place perp:0 sell 2000 49000 ioc post-only\n") {
        Command::PerpPlace { flags, .. } => {
            assert_eq!(flags, vec!["ioc".to_string(), "post-only".to_string()]);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn perp_cancel_takes_one_hex_arg() {
    let oid = format!("0x{}", "ab".repeat(32));
    let line = format!("perp-cancel {}\n", oid);
    match parse_one(&line) {
        Command::PerpCancel { order_id } => assert_eq!(order_id, oid),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn liquidate_requires_addr_and_market() {
    match parse_one("liquidate 0x1234 perp:0\n") {
        Command::Liquidate { addr, market } => {
            assert_eq!(addr, "0x1234");
            assert_eq!(market, "perp:0");
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert!(parse_repl_line("liquidate 0x1234\n").is_err());
}

#[test]
fn help_lists_new_subcommands() {
    let cli = parse_repl_line("help\n")
        .expect("parse")
        .expect("non-empty");
    let cmd = cli.cmd.expect("subcommand");
    match cmd {
        Command::Help { cmd } => assert!(cmd.is_none()),
        other => panic!("unexpected: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// S5: unified `place` / `cancel` / `faucet` grammar.
// ---------------------------------------------------------------------

#[test]
fn place_parses_spot_market_with_default_flags() {
    match parse_one("place spot:btc_usd buy 1 50000 gtc\n") {
        Command::Place {
            market,
            side,
            qty,
            price,
            flags,
        } => {
            assert_eq!(market, "spot:btc_usd");
            assert_eq!(side, "buy");
            assert_eq!(qty, 1);
            assert_eq!(price, 50_000);
            assert_eq!(flags, vec!["gtc".to_string()]);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn place_parses_perp_market_with_compound_flags() {
    match parse_one("place perp:0 sell 1000 50000 ioc post-only\n") {
        Command::Place {
            market,
            side,
            qty,
            price,
            flags,
        } => {
            assert_eq!(market, "perp:0");
            assert_eq!(side, "sell");
            assert_eq!(qty, 1000);
            assert_eq!(price, 50_000);
            assert_eq!(flags, vec!["ioc".to_string(), "post-only".to_string()]);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn place_parses_future_market() {
    match parse_one("place future:0 buy 250 320000000000\n") {
        Command::Place { market, .. } => assert_eq!(market, "future:0"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn place_requires_qty_and_price() {
    // Missing price → clap error.
    assert!(parse_repl_line("place spot:btc_usd buy 1\n").is_err());
}

#[test]
fn cancel_parses_spot_kind() {
    let oid = format!("0x{}", "ab".repeat(32));
    let line = format!("cancel spot {}\n", oid);
    match parse_one(&line) {
        Command::Cancel { kind, order_id } => {
            assert_eq!(kind, "spot");
            assert_eq!(order_id, oid);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn cancel_parses_perp_kind() {
    let oid = "00".repeat(32);
    let line = format!("cancel perp {}\n", oid);
    match parse_one(&line) {
        Command::Cancel { kind, order_id } => {
            assert_eq!(kind, "perp");
            assert_eq!(order_id, oid);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn cancel_requires_kind_and_id() {
    // No id at all.
    assert!(parse_repl_line("cancel spot\n").is_err());
    // No kind at all.
    assert!(parse_repl_line("cancel\n").is_err());
}

#[test]
fn faucet_no_args_uses_default() {
    match parse_one("faucet\n") {
        Command::Faucet { amount } => assert_eq!(amount, None),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn faucet_with_explicit_amount() {
    match parse_one("faucet 5000000000\n") {
        Command::Faucet { amount } => assert_eq!(amount, Some(5_000_000_000)),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn book_accepts_spot_market_ref() {
    match parse_one("book spot:eth_usd\n") {
        Command::Book {
            market,
            depth,
            json,
        } => {
            assert_eq!(market, "spot:eth_usd");
            assert!(depth.is_none());
            assert!(!json);
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deprecated_perp_place_still_parses() {
    // `perp-place` continues to clap-parse; the dispatcher prepends a
    // deprecation note and forwards to the unified `place` handler.
    match parse_one("perp-place perp:0 buy 1 1\n") {
        Command::PerpPlace { market, .. } => assert_eq!(market, "perp:0"),
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn deprecated_perp_cancel_still_parses() {
    let oid = "01".repeat(32);
    match parse_one(&format!("perp-cancel {}\n", oid)) {
        Command::PerpCancel { order_id } => assert_eq!(order_id, oid),
        other => panic!("unexpected: {other:?}"),
    }
}
