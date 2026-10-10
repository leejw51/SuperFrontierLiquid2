//! Smoke-test the binary surface without spawning a subprocess: we drive
//! `clap`'s `try_parse_from` for `--help` and run the `wallets` dispatch
//! against an empty store. This is what the parent agent's "binary --help
//! works" smoke-step would verify; running the bin directly was blocked
//! by sandbox in CI.

use cb_client::cli::{Cli, Command};
use cb_client::prompts::ScriptedPrompts;
use cb_client::repl;
use cb_client::store::Store;
use clap::CommandFactory;
use tempfile::TempDir;

#[test]
fn help_renders_subcommand_list() {
    let mut cmd = Cli::command();
    let help = cmd.render_long_help().to_string();
    for needed in [
        "login",
        "select",
        "wallets",
        "balance",
        "send",
        "addr",
        "set-rpc",
        "set-chain-id",
        "help",
        "quit",
        "--rpc",
        "--chain-id",
        "place",
        "cancel",
        "faucet",
        "book",
        "markets",
    ] {
        assert!(help.contains(needed), "help missing `{needed}`:\n{help}");
    }
}

#[test]
fn help_for_place_mentions_all_three_market_kinds() {
    // Drive the in-process help dispatcher rather than `--help`, since the
    // per-subcommand short blurb is what `help <cmd>` prints in the REPL.
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Help {
            cmd: Some("place".to_string()),
        },
    )
    .unwrap();
    for kind in ["spot:", "perp:", "future:"] {
        assert!(out.contains(kind), "help place missing `{kind}`:\n{out}");
    }
}

#[test]
fn help_for_cancel_mentions_kinds() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Help {
            cmd: Some("cancel".to_string()),
        },
    )
    .unwrap();
    assert!(out.contains("spot"), "{out}");
    assert!(out.contains("perp"), "{out}");
}

#[test]
fn help_for_faucet_mentions_env_vars() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Help {
            cmd: Some("faucet".to_string()),
        },
    )
    .unwrap();
    assert!(
        out.contains("UL_FAUCET_MNEMONIC"),
        "help faucet missing UL_FAUCET_MNEMONIC:\n{out}"
    );
    assert!(
        out.contains("MYWALLET3_MNEMONIC"),
        "help faucet missing MYWALLET3_MNEMONIC:\n{out}"
    );
}

#[test]
fn help_for_book_mentions_spot_route() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Help {
            cmd: Some("book".to_string()),
        },
    )
    .unwrap();
    assert!(out.contains("spot:"), "{out}");
}

#[test]
fn help_for_perp_place_marked_deprecated() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Help {
            cmd: Some("perp-place".to_string()),
        },
    )
    .unwrap();
    assert!(
        out.contains("DEPRECATED") || out.contains("deprecated"),
        "perp-place help should mark it deprecated:\n{out}"
    );
}

#[test]
fn wallets_dispatch_on_empty_store_renders_header_and_hint() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut prompts = ScriptedPrompts::new();
    let out = repl::dispatch(&store, &mut prompts, None, None, Command::Wallets).unwrap();
    assert!(out.contains("label"), "{out}");
    assert!(out.contains("(no wallets"), "{out}");
}
