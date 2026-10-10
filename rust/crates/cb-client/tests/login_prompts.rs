//! Interactive `login` flow with hidden mnemonic input.
//!
//! `cmd_login` accepts each of `--label`, `--mnemonic`, `--index` as either
//! a flag or an interactive prompt. The mnemonic is read via the password
//! channel (terminal echo OFF in production); these tests use
//! `ScriptedPrompts` to pop canned answers without touching a real tty.
//!
//! What we pin:
//!   - All-flags path: no prompt is consulted; wallet derived as expected.
//!   - All-prompts path: label/mnemonic/index are read in order; the
//!     mnemonic comes from the password queue, not the visible queue.
//!   - Mixed path: explicit `--label` skips the label prompt.
//!   - Default index: blank input on the index prompt yields 0.
//!   - Bad index: non-numeric input is rejected with a clear error.
//!   - Empty mnemonic prompt: rejected with a clear error.
//!   - Empty label prompt: rejected with a clear error.
//!   - Mnemonic flag override path: the prompt log NEVER contains the
//!     mnemonic value (it is only ever read, never echoed back).
//!
//! All mnemonics are real BIP-39 phrases; any other shape is rejected by
//! the migrated `WalletEntry::derive`.

use cb_client::cli::Command;
use cb_client::prompts::ScriptedPrompts;
use cb_client::repl;
use cb_client::store::Store;
use cb_client::wallet::WalletEntry;
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
fn login_with_all_flags_does_not_prompt() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new(); // empty — any read would error
    let cmd = Command::Login {
        label: Some("alice".into()),
        mnemonic: Some(ABANDON.into()),
        index: Some(0),
        force: false,
    };
    let text = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    assert!(text.contains("logged in as alice"), "{text}");
    assert!(prompts.prompt_log.is_empty(), "no prompts should be issued");

    // Wallet was actually persisted.
    let saved = store.find_wallet("alice").unwrap().expect("alice");
    let expected = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    assert_eq!(saved.address, expected.address);
    assert_eq!(saved.privkey, expected.privkey);
    // Pin the canonical Ethereum address: this is what MetaMask shows for
    // the same mnemonic at the default account.
    assert_eq!(
        saved.address.to_lowercase(),
        "9858effd232b4033e47d90003d41ec34ecaeda94",
    );
}

#[test]
fn login_with_no_flags_prompts_label_then_password_then_index() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["alice".into(), "0".into()])
        .with_passwords(vec![LEGAL.into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: None,
        force: false,
    };
    let text = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    assert!(text.contains("logged in as alice"), "{text}");

    // Order matters: Label (line) → Mnemonic (password) → Index (line).
    assert_eq!(prompts.prompt_log.len(), 3);
    assert_eq!(prompts.prompt_log[0], "Label: ");
    assert_eq!(prompts.prompt_log[1], "Mnemonic (hidden): ");
    assert_eq!(prompts.prompt_log[2], "Wallet index [0]: ");

    // Mnemonic value never appears in the visible prompt log — it was read
    // via the password channel.
    for entry in &prompts.prompt_log {
        assert!(
            !entry.contains(LEGAL),
            "mnemonic must not leak into prompt strings: {entry:?}",
        );
    }

    // The persisted wallet matches what we'd get with the same inputs.
    let saved = store.find_wallet("alice").unwrap().expect("alice");
    let expected = WalletEntry::derive("alice", LEGAL, 0).unwrap();
    assert_eq!(saved.address, expected.address);
}

#[test]
fn login_label_flag_skips_label_prompt() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["3".into()]) // only the index prompt
        .with_passwords(vec![ABANDON.into()]);
    let cmd = Command::Login {
        label: Some("bob".into()),
        mnemonic: None,
        index: None,
        force: false,
    };
    let text = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    assert!(text.contains("logged in as bob"));
    assert_eq!(
        prompts.prompt_log,
        vec!["Mnemonic (hidden): ", "Wallet index [0]: "],
        "label flag must skip the label prompt",
    );
    let saved = store.find_wallet("bob").unwrap().expect("bob");
    assert_eq!(saved.index, 3);
}

#[test]
fn login_blank_index_prompt_defaults_to_zero() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["alice".into(), "".into()]) // blank index
        .with_passwords(vec![ABANDON.into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: None,
        force: false,
    };
    let text = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    assert!(text.contains("logged in as alice"));
    let saved = store.find_wallet("alice").unwrap().expect("alice");
    assert_eq!(saved.index, 0);
}

#[test]
fn login_non_numeric_index_is_rejected() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["alice".into(), "not-a-number".into()])
        .with_passwords(vec![ABANDON.into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: None,
        force: false,
    };
    let err = repl::dispatch(&store, &mut prompts, None, None, cmd).unwrap_err();
    assert!(err.to_string().contains("non-negative integer"));
    // No wallet should have been persisted.
    assert!(store.find_wallet("alice").unwrap().is_none());
}

#[test]
fn login_empty_mnemonic_prompt_is_rejected() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["alice".into()])
        .with_passwords(vec!["".into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: None,
        force: false,
    };
    let err = repl::dispatch(&store, &mut prompts, None, None, cmd).unwrap_err();
    assert!(err.to_string().contains("mnemonic must not be empty"));
}

#[test]
fn login_invalid_bip39_mnemonic_is_rejected() {
    // The pre-migration deriver hashed any string straight to a key. Under
    // strict BIP-39 a non-wordlist phrase must be rejected with a helpful
    // hint that mentions "BIP-39".
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new();
    let cmd = Command::Login {
        label: Some("alice".into()),
        mnemonic: Some("hello world".into()),
        index: Some(0),
        force: false,
    };
    let err = repl::dispatch(&store, &mut prompts, None, None, cmd).unwrap_err();
    assert!(
        err.to_string().contains("BIP-39"),
        "expected BIP-39 hint, got: {err}",
    );
    assert!(
        store.find_wallet("alice").unwrap().is_none(),
        "no wallet should be persisted on bad mnemonic",
    );
}

#[test]
fn login_empty_label_prompt_is_rejected() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["   ".into()]) // whitespace-only label
        .with_passwords(vec![ABANDON.into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: None,
        force: false,
    };
    let err = repl::dispatch(&store, &mut prompts, None, None, cmd).unwrap_err();
    assert!(err.to_string().contains("label must not be empty"));
}

#[test]
fn login_index_flag_skips_index_prompt() {
    let (_tmp, store) = fresh_store();
    let mut prompts = ScriptedPrompts::new()
        .with_lines(vec!["alice".into()])
        .with_passwords(vec![ABANDON.into()]);
    let cmd = Command::Login {
        label: None,
        mnemonic: None,
        index: Some(7),
        force: false,
    };
    let text = repl::dispatch(&store, &mut prompts, None, None, cmd).expect("dispatch");
    assert!(text.contains("logged in as alice"));
    assert_eq!(
        prompts.prompt_log,
        vec!["Label: ", "Mnemonic (hidden): "],
        "index flag must skip the index prompt",
    );
    assert_eq!(store.find_wallet("alice").unwrap().unwrap().index, 7);
}
