//! `addr` subcommand: stateless deriver used by the testblockchain harness.
//!
//! In-process tests drive `repl::dispatch` for the explicit-flag paths; the
//! stdin-pipe test spawns the real binary so we exercise the non-TTY branch
//! that `IsTerminal` selects.
//!
//! All test mnemonics are real BIP-39 phrases — `WalletEntry::derive`
//! rejects anything that fails the BIP-39 wordlist + checksum check.

use cb_client::cli::Command;
use cb_client::prompts::ScriptedPrompts;
use cb_client::repl;
use cb_client::store::Store;
use cb_client::wallet::WalletEntry;
use std::io::Write;
use std::process::{Command as ProcCommand, Stdio};
use tempfile::TempDir;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

fn fresh_store() -> (TempDir, Store) {
    let tmp = TempDir::new().unwrap();
    let s = Store::open(tmp.path().to_path_buf()).unwrap();
    (tmp, s)
}

#[test]
fn addr_with_explicit_args_prints_expected_hex() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new(); // empty — must not be consulted
    let cmd = Command::Addr {
        mnemonic: Some(ABANDON.into()),
        index: Some(7),
    };
    let out = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    let expected = WalletEntry::derive("addr", ABANDON, 7).unwrap();
    assert_eq!(out, format!("0x{}\n", expected.address));
    assert!(prompts.prompt_log.is_empty(), "no prompts on flag path");
}

#[test]
fn addr_default_index_is_zero() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new();
    let with_default = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Addr {
            mnemonic: Some(LEGAL.into()),
            index: None,
        },
    )
    .unwrap();
    let with_zero = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Addr {
            mnemonic: Some(LEGAL.into()),
            index: Some(0),
        },
    )
    .unwrap();
    assert_eq!(with_default, with_zero);
}

#[test]
fn addr_rejects_empty_mnemonic() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new();
    let err = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Addr {
            mnemonic: Some("   ".into()),
            index: None,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("mnemonic"),
        "expected mnemonic error, got: {err}"
    );
}

#[test]
fn addr_rejects_invalid_bip39_mnemonic() {
    // Pre-migration this would have hashed straight through; under strict
    // BIP-39 it must surface a clear error.
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new();
    let err = repl::dispatch(
        &store,
        &mut prompts,
        None,
        None,
        Command::Addr {
            mnemonic: Some("not a real bip39 phrase".into()),
            index: None,
        },
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("BIP-39"),
        "expected BIP-39 error, got: {err}",
    );
}

#[test]
fn addr_reads_mnemonic_from_stdin_pipe() {
    // Spawn the real binary with stdin piped (non-TTY), no --mnemonic flag.
    // Compare to the expected hex computed in-process.
    let bin = env!("CARGO_BIN_EXE_cb-client");
    let mnemonic = ABANDON;
    let mut child = ProcCommand::new(bin)
        .arg("addr")
        .arg("--index")
        .arg("3")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cb-client");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{mnemonic}").unwrap();
    }
    let output = child.wait_with_output().expect("wait");
    assert!(
        output.status.success(),
        "non-zero exit: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = WalletEntry::derive("addr", mnemonic, 3).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    // The binary may emit a trailing extra newline via main.rs's "ensure
    // trailing \n" path; compare on the trimmed first line.
    let first_line = stdout.lines().next().unwrap_or("");
    assert_eq!(first_line, format!("0x{}", expected.address));
}
