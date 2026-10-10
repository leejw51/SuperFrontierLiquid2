//! One test per fault the first real end-to-end run found in ten minutes.
//!
//! Every fault below was invisible from inside a subsystem and obvious the
//! moment the pieces were put together. None had a test. Each test here is
//! named after the guarantee it restores, and carries the story of the fault it
//! was written for, because a regression test whose reason has been forgotten
//! gets "simplified" out of existence at the next refactor.
//!
//! The first three need no running stack: they are statements about the files
//! the repository ships, and they run in a plain `cargo test`. That matters,
//! because two of the five faults were in configuration that no amount of
//! runtime testing would have examined.

use std::collections::BTreeSet;

use cb_e2e::flows::sign_in;
use cb_e2e::shipped;
use cb_e2e::ws::{frontend_websocket_url, naive_websocket_url, Ws};
use cb_e2e::Stack;
use cb_types::ids::MarketId;
use cb_types::symbols::{spot_symbol, FUTURES, PERPS};
use serde_json::json;

// ---------------------------------------------------------------------------
// Fault 1: the genesis treasury was a vanity address with no private key.
// ---------------------------------------------------------------------------

/// The treasury is the only account allowed to mint, so it is the account the
/// faucet pays out of. An earlier genesis used a hand-picked "nice looking"
/// address that no key produces. Genesis applied cleanly, the node ran, the
/// gateway started, and the faucet silently could not sign a thing. The only
/// symptom was a user clicking a button and getting nothing.
///
/// The check is a round trip: take the key the shipped environment hands the
/// gateway as `--faucet-treasury-key`, derive its address, and require that to
/// be the address the shipped genesis funds as the treasury.
#[test]
fn the_genesis_treasury_is_an_address_the_shipped_environment_has_a_key_for() {
    let env = shipped::shipped_env();
    let env_path = shipped::env_example_path();

    let treasury = env.get("TREASURY").unwrap_or_else(|| {
        panic!(
            "TREASURY is not set in {}. Genesis funds the treasury by that name, so the chain \
             cannot be applied at all without it.",
            env_path.display()
        )
    });
    let key_hex = env.get("TREASURY_KEY").unwrap_or_else(|| {
        panic!(
            "TREASURY_KEY is not set in {}. The gateway is started with \
             `--faucet-treasury-key $TREASURY_KEY`, so without it the faucet cannot sign the \
             mint and no user can ever get test money.",
            env_path.display()
        )
    });

    let bytes = hex::decode(key_hex.trim_start_matches("0x"))
        .unwrap_or_else(|err| panic!("TREASURY_KEY is not hex: {err}"));
    let secret = secp256k1::SecretKey::from_slice(&bytes)
        .unwrap_or_else(|err| panic!("TREASURY_KEY is not a valid secp256k1 scalar: {err}"));
    let public = secp256k1::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &secret);
    let derived = cb_crypto::address_from_pubkey(&public).to_string();

    assert_eq!(
        derived.to_lowercase(),
        treasury.trim().to_lowercase(),
        "the genesis treasury and the faucet's signing key are different accounts.\n\
         \n  TREASURY      = {treasury}\n  TREASURY_KEY  derives {derived}\n\n\
         Genesis funds the first address; the gateway signs mints with the second. Nothing \
         reports this at boot — the faucet simply never works. Both live in {}.",
        env_path.display()
    );

    // And the address genesis actually substitutes has to be that one, not some
    // other address written into the file directly.
    let genesis = shipped::default_genesis_path();
    let company = shipped::genesis_lines(&genesis)
        .into_iter()
        .find(|line| line.kind() == Some("company"))
        .unwrap_or_else(|| {
            panic!(
                "{} has no `company` record. That record is the treasury: the only account \
                 permitted to mint.",
                genesis.display()
            )
        });
    assert!(
        company.text.contains("${TREASURY}"),
        "the treasury address in {} line {} is written out rather than taken from \
         ${{TREASURY}}, so it can drift away from the key the gateway is given: {}",
        genesis.display(),
        company.number,
        company.text
    );
}

// ---------------------------------------------------------------------------
// Fault 5: genesis defaulted to a file without the advertised markets, and
// referenced a variable the environment never set.
// ---------------------------------------------------------------------------

/// `make init` applies one genesis file, and which one is a single line in
/// `rust/Makefile`. When that line pointed at the narrow `genesis.jsonl`, the
/// chain came up with a handful of spot markets and none of the perpetuals or
/// futures the product's own documentation lists — so every derivative page in
/// the frontend was empty and the ticker for a listed symbol was a 404.
///
/// The default is read out of the Makefile rather than named here. A test that
/// asserted about `genesis-full.jsonl` would have passed all the way through
/// the outage: that file was never the problem, the default was.
#[test]
fn the_default_genesis_lists_every_market_the_product_advertises() {
    let genesis = shipped::default_genesis_path();
    let lines = shipped::genesis_lines(&genesis);

    let ids = |kind: &str| -> BTreeSet<String> {
        lines
            .iter()
            .filter(|line| line.kind() == Some(kind))
            .filter_map(|line| {
                let rest = line.text.split_once("\"id\":\"")?.1;
                rest.split_once('"').map(|(id, _)| id.to_string())
            })
            .collect()
    };

    // A genesis id is the wire symbol, lower-cased, with the separator changed:
    // `BTC-PERP` is `btc_perp`. Deriving it rather than listing it keeps this
    // test correct when a market is added to the tables.
    let expected = |symbol: &str| symbol.to_lowercase().replace('-', "_");

    for (kind, wanted) in [
        (
            "market",
            MarketId::ALL
                .iter()
                .map(|m| spot_symbol(*m))
                .collect::<Vec<_>>(),
        ),
        ("perp_market", PERPS.iter().map(|p| p.symbol).collect()),
        ("future_market", FUTURES.iter().map(|f| f.symbol).collect()),
    ] {
        let present = ids(kind);
        for symbol in &wanted {
            assert!(
                present.contains(&expected(symbol)),
                "{} does not list {symbol}, which `cb_types::symbols` advertises and every \
                 client expects to be able to trade. `{}` records present: {present:?}.\n\
                 The genesis file `make init` applies is {}, read from the `GENESIS ?=` line \
                 of rust/Makefile.",
                genesis.display(),
                kind,
                genesis.display(),
            );
        }
    }
}

/// A `${VAR}` in genesis that the environment does not set is a hard error at
/// `cb-node init`, and the node is right to refuse: a silently-empty address
/// would produce a chain whose funds nobody controls. But the failure lands on
/// whoever runs `make setup` for the first time, with no clue which file is at
/// fault. This says so before they ever run it.
///
/// Only record lines are scanned. The header comments of `genesis-full.jsonl`
/// document the substitution syntax and contain a literal `${VAR}`; a scan of
/// the whole file reports a missing variable called `VAR` and sends the reader
/// looking for something that does not exist.
#[test]
fn every_variable_the_default_genesis_references_is_set_by_the_shipped_environment() {
    let genesis = shipped::default_genesis_path();
    let env = shipped::shipped_env();
    let env_path = shipped::env_example_path();

    let mut missing = Vec::new();
    for (line_number, name) in shipped::referenced_variables(&genesis) {
        if !env.contains_key(&name) {
            missing.push(format!(
                "  {}:{line_number} references ${{{name}}}",
                genesis.display()
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "the default genesis references {} variable(s) that {} does not set:\n{}\n\n\
         `cb-node init` treats an unset variable as a hard error, so this is a chain that \
         cannot be created from a fresh clone.",
        missing.len(),
        env_path.display(),
        missing.join("\n"),
    );
}

/// The shipped environment must also be parseable by the parser the node uses.
///
/// `cb-genesis`'s dotenv parser does not strip a trailing `# comment` from a
/// value, which is why every comment in `.env.example` is on its own line. A
/// comment moved onto the end of an address line silently becomes part of the
/// address, and the chain funds an account nobody can reach.
#[test]
fn the_shipped_environment_parses_with_no_comment_stuck_to_a_value() {
    let env = shipped::shipped_env();
    for (key, value) in &env {
        assert!(
            !value.contains('#'),
            "{key} = {value:?} in {} has a `#` in its value. cb-genesis's parser keeps a \
             trailing comment inside the value, so this address or amount is not what it \
             looks like. Put the comment on its own line.",
            shipped::env_example_path().display()
        );
    }
    for required in [
        "MYCHAINID",
        "MYDENOM",
        "TREASURY",
        "TREASURY_KEY",
        "GATEWAY_SERVER_SECRET",
    ] {
        assert!(
            env.contains_key(required),
            "{required} is missing from {}; the stack cannot be booted from a fresh clone \
             without it",
            shipped::env_example_path().display()
        );
    }
}

// ---------------------------------------------------------------------------
// Fault 4: the gateway served /ws at the root, the frontend asked for
// /api/v1/ws.
// ---------------------------------------------------------------------------

/// The frontend's derivation, applied to a live gateway, must upgrade.
///
/// The URL is *derived* with the frontend's own rule rather than typed in. A
/// test that connected to a literal `"/ws"` would have been green throughout
/// the outage, because the gateway served `/ws` the whole time; what was broken
/// was the question the other side asked. The naive derivation is dialled too,
/// so this test demonstrates it discriminates rather than merely passing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn the_websocket_url_the_frontend_derives_is_the_one_the_gateway_serves() {
    let stack = Stack::start().await;
    let api_base = stack.api_base();

    let derived = frontend_websocket_url(&api_base);
    let naive = naive_websocket_url(&api_base);
    assert_ne!(
        derived, naive,
        "the two derivations produced the same URL, so this test can no longer tell them \
         apart and proves nothing"
    );

    let mut socket = Ws::connect(&derived).await.unwrap_or_else(|err| {
        panic!(
            "the frontend derives {derived} from an API base of {api_base}, and the gateway \
             would not upgrade it: {err}\n\
             A browser reports this only as a failed handshake with no body, so the terminal \
             shows a permanently disconnected feed and no error anyone can act on."
        )
    });
    socket.send(json!({ "op": "ping" })).await;
    socket
        .await_message(
            "a pong on the socket the frontend dials",
            std::time::Duration::from_secs(10),
            |message| message["type"].as_str() == Some("pong"),
        )
        .await;

    assert!(
        Ws::connect(&naive).await.is_err(),
        "{naive} also upgraded. That is the URL the frontend used to build by appending the \
         socket path to the API base; if both work, this test cannot detect the frontend \
         going back to the wrong one."
    );
}

/// The port of the frontend's derivation still matches the frontend.
///
/// `cb-web` is excluded from the workspace and compiles only for `wasm32`, so
/// its `websocket_url` cannot be called from here and had to be copied. A copy
/// is a second implementation, and two implementations that agree today
/// disagree after the next edit — which is how fault 4 happened in the first
/// place. This reads the shipped frontend source and fails if its derivation
/// has changed shape, which is the signal to re-port it.
#[test]
fn the_ported_websocket_derivation_still_matches_the_frontend_source() {
    let source = shipped::rust_dir().join("crates/cb-web/src/config.rs");
    let text = shipped::read(&source);
    let start = text.find("fn websocket_url").unwrap_or_else(|| {
        panic!(
            "{} no longer defines `websocket_url`. `cb_e2e::ws::frontend_websocket_url` is a \
             port of that function and has to be re-derived from whatever replaced it.",
            source.display()
        )
    });
    let body = &text[start..];
    let body = &body[..body.find("\npub fn").unwrap_or(body.len())];

    for fragment in [
        "strip_prefix(\"http://\")",
        "strip_prefix(\"https://\")",
        "split('/').next()",
        "/ws",
    ] {
        assert!(
            body.contains(fragment),
            "the frontend's websocket derivation in {} no longer contains `{fragment}`, so \
             the port in `cb_e2e::ws::frontend_websocket_url` may have drifted from it. \
             Re-read that function and update the port, then update this list.",
            source.display()
        );
    }

    // And the port must still agree with the rule that source describes.
    assert_eq!(
        frontend_websocket_url("http://127.0.0.1:8081/api/v1"),
        "ws://127.0.0.1:8081/ws"
    );
    assert_eq!(
        frontend_websocket_url("https://exchange.example/api/v1"),
        "wss://exchange.example/ws"
    );
}

// ---------------------------------------------------------------------------
// Fault 2: a multi-asset faucet grant credited the chain and then errored.
// ---------------------------------------------------------------------------

/// The shipped grant is two assets — 100,000 USD and 1 BTC — and the two rows
/// collided on a primary key. The mint had already gone to the chain by then,
/// so the user got an error *and* the money: the worst of both, and a ledger
/// that disagrees with the balance.
///
/// The claim is therefore checked three ways: the call succeeds, every asset in
/// the advertised grant lands, and the account is recorded as having claimed
/// **once** rather than once per asset.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn a_multi_asset_faucet_grant_succeeds_and_records_exactly_one_claim() {
    let stack = Stack::start().await;
    let session = sign_in(&stack.api, 0, "faucet_multi").await;
    let user = stack.api.as_user(&session);

    let advertised = user.get_ok("/faucet").await;
    let grants = advertised.ok(200)["grant"]
        .as_array()
        .expect("the faucet advertises its grant as a list")
        .clone();
    assert!(
        grants.len() >= 2,
        "this stack's faucet grants {} asset(s); the collision only happens with more than \
         one, so the test needs a multi-asset grant to mean anything. Grant: {grants:?}",
        grants.len()
    );

    let claim = user.post_ok("/faucet/claim", json!({})).await;
    assert_eq!(
        claim.status, 200,
        "a multi-asset faucet claim failed with {}.\n\
         Check the balance before assuming nothing happened: the mint is submitted before the \
         claim is recorded, so an error here has historically meant the user got the money \
         *and* an error.",
        claim.raw
    );

    let granted = claim.body["granted"]
        .as_array()
        .expect("a successful claim lists what it granted");
    assert_eq!(
        granted.len(),
        grants.len(),
        "the faucet advertised {} assets and granted {}: {}",
        grants.len(),
        granted.len(),
        claim.raw
    );

    for grant in &grants {
        let asset = grant["asset"].as_str().expect("a grant names its asset");
        let balance = user
            .await_account(&format!("the granted {asset} to arrive"), |account| {
                cb_e2e::client::balance_entry(account, asset, "total") > rust_decimal::Decimal::ZERO
            })
            .await;
        let total = cb_e2e::client::balance_entry(&balance, asset, "total");
        assert!(
            total > rust_decimal::Decimal::ZERO,
            "{asset} was granted but never arrived on chain; balances are {}",
            balance["balances"]
        );
    }

    let after = user.get_ok("/faucet").await;
    let after = after.ok(200);
    assert_eq!(
        after["claims_made"].as_u64(),
        Some(1),
        "one claim of a {}-asset grant was recorded as {} claims. A claim is what the \
         cooldown and the per-user limit are counted in, so counting rows instead of claims \
         makes both of them wrong by the size of the grant.",
        grants.len(),
        after["claims_made"]
    );
}

// ---------------------------------------------------------------------------
// Fault 3: an unpriced derivative made the ticker 500 and blanked the terminal.
// ---------------------------------------------------------------------------

/// Every market the gateway lists must serve a ticker on a chain where no
/// oracle has ever published a price.
///
/// This is the fault with the widest blast radius. The terminal fetches a
/// ticker for every market to draw its market list, so one 500 from one
/// unpriced perpetual blanked the entire screen — including the spot markets
/// that were working perfectly.
///
/// The condition that triggered it is the *default* condition: a chain that has
/// just been created has no marks. "No price yet" is information a ticker can
/// carry; it is not a server error.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "boots the real node and gateway; run with `make e2e`"]
async fn every_listed_market_serves_a_ticker_on_a_fresh_chain_with_no_oracle() {
    let stack = Stack::start().await;
    let markets = stack.api.get_ok("/markets").await;
    let markets = markets.ok(200)["markets"]
        .as_array()
        .expect("GET /markets returns a list")
        .clone();
    assert!(
        !markets.is_empty(),
        "the gateway lists no markets at all, so this test would pass vacuously"
    );
    assert!(
        markets.iter().any(|m| m["kind"].as_str() != Some("spot")),
        "the catalogue contains no derivative, and the derivative is the market that had no \
         oracle price. Markets listed: {:?}",
        markets
            .iter()
            .map(|m| m["symbol"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>()
    );

    let mut broken = Vec::new();
    for market in &markets {
        let symbol = market["symbol"]
            .as_str()
            .expect("every market has a symbol");
        let reply = stack.api.get_ok(&format!("/markets/{symbol}/ticker")).await;
        if reply.status != 200 {
            broken.push(format!(
                "  {symbol} ({}) -> HTTP {} {}",
                market["kind"].as_str().unwrap_or("?"),
                reply.status,
                reply.raw
            ));
            continue;
        }
        cb_e2e::money::assert_number_conventions(
            &reply.body,
            &format!("GET /markets/{symbol}/ticker"),
        );
    }

    assert!(
        broken.is_empty(),
        "{} of {} listed markets did not serve a ticker on a fresh chain:\n{}\n\n\
         The terminal fetches a ticker for every market in the list, so any one of these \
         blanks the whole screen rather than just its own row.",
        broken.len(),
        markets.len(),
        broken.join("\n"),
    );
}
