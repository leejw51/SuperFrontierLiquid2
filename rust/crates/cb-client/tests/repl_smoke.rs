//! Drive the REPL through `run_with_io` against scripted stdin and assert
//! the output. No network is touched — only commands that don't need RPC
//! are exercised here.

use cb_client::prompts::ScriptedPrompts;
use cb_client::repl;
use cb_client::store::{Session, Store};
use cb_client::wallet::WalletEntry;
use std::io::Cursor;
use tempfile::TempDir;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

fn run_script(input: &str) -> String {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let mut reader = Cursor::new(input.as_bytes().to_vec());
    let mut writer: Vec<u8> = Vec::new();
    let mut prompts = ScriptedPrompts::new();
    repl::run_with_io(store, &mut reader, &mut writer, &mut prompts).expect("repl run");
    String::from_utf8(writer).expect("utf8")
}

/// Same as `run_script` but pre-plants a current wallet (label `trader`) so
/// commands that require one (`place`, `cancel`, `faucet`) don't bail with
/// "no current wallet" before their guard rails fire. The REPL's `login`
/// path can't be driven from a single line because clap splits on
/// whitespace and the mnemonic itself contains spaces — direct store
/// upsert is the test-equivalent.
fn run_script_with_trader(input: &str) -> String {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path().to_path_buf()).unwrap();
    let entry = WalletEntry::derive("trader", ABANDON, 0).unwrap();
    store.upsert_wallet(entry, false).unwrap();
    let mut sess = store.load_session().unwrap_or_else(|_| Session::default());
    sess.current_wallet = Some("trader".to_string());
    store.save_session(&sess).unwrap();
    let mut reader = Cursor::new(input.as_bytes().to_vec());
    let mut writer: Vec<u8> = Vec::new();
    let mut prompts = ScriptedPrompts::new();
    repl::run_with_io(store, &mut reader, &mut writer, &mut prompts).expect("repl run");
    String::from_utf8(writer).expect("utf8")
}

#[test]
fn empty_input_prints_banner_and_exits() {
    let out = run_script("");
    assert!(out.contains("cb-client"), "{out}");
    // Banner must include the four operator-context labels. We don't assert
    // on their values (cwd/store paths are environment-specific; the rpc
    // probe will time out under tests since no sequencer is bound).
    for label in ["cwd:", "store:", "rpc:", "server:"] {
        assert!(out.contains(label), "banner missing `{label}`: {out}");
    }
}

#[test]
fn help_prints_command_list() {
    let out = run_script("help\n");
    assert!(out.contains("login"), "{out}");
    assert!(out.contains("balance"), "{out}");
    assert!(out.contains("send"), "{out}");
    assert!(out.contains("quit"), "{out}");
}

#[test]
fn wallets_on_empty_store_prints_hint() {
    let out = run_script("wallets\n");
    assert!(out.contains("(no wallets"), "{out}");
}

#[test]
fn balance_without_login_errors_clearly() {
    let out = run_script("balance\n");
    assert!(out.contains("error"), "{out}");
    assert!(
        out.contains("no current wallet"),
        "expected guidance about logging in: {out}"
    );
}

#[test]
fn quit_exits_without_running_more() {
    // Second line should never run; if it did we'd hit `wallets` and see
    // the empty-store hint.
    let out = run_script("quit\nwallets\n");
    assert!(!out.contains("(no wallets"), "{out}");
}

#[test]
fn unknown_command_does_not_crash_the_repl() {
    // After a clap parse failure we should still be able to issue `help`
    // on the next line.
    let out = run_script("nosuch\nhelp\n");
    assert!(out.contains("login"), "still running after bad cmd: {out}");
}

// ---------------------------------------------------------------------
// S5 — unified `place` / `cancel` / `faucet` smoke paths.
//
// All four scenarios drive the REPL offline (no sequencer running on the
// canned `127.0.0.1:7700`), so the exact text is governed by the parse /
// pre-RPC validation path. This is deliberate: we want to assert the
// guard rails (reduce-only on spot, missing wallet, deprecation note)
// without a running chain, and the more involved spot+perp end-to-end is
// covered by `make testclient` (S6).
// ---------------------------------------------------------------------

#[test]
fn place_spot_with_reduce_only_is_rejected_at_parse() {
    let out = run_script_with_trader("place spot:btc_usd buy 1 1 reduce-only\n");
    assert!(
        out.contains("reduce-only is not valid for spot"),
        "expected spot reduce-only rejection in output:\n{out}"
    );
}

#[test]
fn cancel_spot_with_well_formed_id_routes_to_spot_path() {
    // No sequencer running, so we expect a connection-shaped error rather
    // than a parse failure. The important contract: `cancel spot <id>`
    // doesn't panic and doesn't produce an "unknown kind" error.
    let out = run_script_with_trader(
        "cancel spot 0x0000000000000000000000000000000000000000000000000000000000000000\n",
    );
    assert!(
        out.contains("error:") || out.contains("rejected"),
        "expected an error from offline cancel, got:\n{out}"
    );
    assert!(
        !out.contains("unknown cancel kind"),
        "spot was misclassified as unknown:\n{out}"
    );
}

#[test]
fn deprecated_perp_place_emits_note_before_running() {
    let out = run_script_with_trader("perp-place perp:0 buy 1 1\n");
    assert!(
        out.contains("`perp-place` is deprecated"),
        "expected deprecation note in output:\n{out}"
    );
}

#[test]
fn deprecated_perp_cancel_emits_note() {
    let out = run_script_with_trader(
        "perp-cancel 0x0000000000000000000000000000000000000000000000000000000000000000\n",
    );
    assert!(
        out.contains("`perp-cancel` is deprecated"),
        "expected deprecation note in output:\n{out}"
    );
}

/// Per-binary mutex for any test in this file that mutates the faucet env
/// vars. Mirrors `crates/cb-client/src/faucet.rs::faucet_env_test_mutex` —
/// they can't share one (separate processes) but each binary needs its own
/// in-process gate so future siblings stay race-free.
fn faucet_env_test_mutex() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn faucet_without_env_errors_with_documented_hint() {
    // Serialize against any sibling test in this binary that also touches
    // these env vars. Today only this test does, but the lock keeps the
    // assertion stable if a future test starts reading the same envs.
    let _g = faucet_env_test_mutex()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let prev_ul = std::env::var("UL_FAUCET_MNEMONIC").ok();
    let prev_my = std::env::var("MYWALLET3_MNEMONIC").ok();
    std::env::remove_var("UL_FAUCET_MNEMONIC");
    std::env::remove_var("MYWALLET3_MNEMONIC");

    let out = run_script_with_trader("faucet 1000\n");

    if let Some(v) = prev_ul {
        std::env::set_var("UL_FAUCET_MNEMONIC", v);
    }
    if let Some(v) = prev_my {
        std::env::set_var("MYWALLET3_MNEMONIC", v);
    }

    assert!(
        out.contains("UL_FAUCET_MNEMONIC"),
        "faucet error should mention UL_FAUCET_MNEMONIC env var:\n{out}"
    );
}
