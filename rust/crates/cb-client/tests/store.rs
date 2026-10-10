//! `~/.cb_client` persistence — append, lookup, session roundtrip,
//! and Unix file-mode tightening.
//!
//! All wallet entries here use real BIP-39 phrases. Different `(mnemonic,
//! index)` pairs are how we obtain distinct addresses without juggling
//! many phrases.

use cb_client::store::{Session, Store};
use cb_client::wallet::WalletEntry;
use std::fs;
use tempfile::TempDir;

const ABANDON: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
const LEGAL: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

fn fresh() -> (TempDir, Store) {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path().to_path_buf()).expect("open store");
    (tmp, store)
}

#[test]
fn empty_store_lists_no_wallets() {
    let (_t, s) = fresh();
    let entries = s.list_wallets().unwrap();
    assert!(entries.is_empty());
}

#[test]
fn append_and_list_wallets_preserves_order() {
    let (_t, s) = fresh();
    let a = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    let b = WalletEntry::derive("bob", ABANDON, 1).unwrap();
    let c = WalletEntry::derive("carol", ABANDON, 2).unwrap();
    s.upsert_wallet(a.clone(), false).unwrap();
    s.upsert_wallet(b.clone(), false).unwrap();
    s.upsert_wallet(c.clone(), false).unwrap();
    let labels: Vec<String> = s
        .list_wallets()
        .unwrap()
        .into_iter()
        .map(|w| w.label)
        .collect();
    assert_eq!(labels, vec!["alice", "bob", "carol"]);
}

#[test]
fn duplicate_label_without_force_errors() {
    let (_t, s) = fresh();
    let a = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    s.upsert_wallet(a.clone(), false).unwrap();
    let again = WalletEntry::derive("alice", ABANDON, 1).unwrap();
    let err = s.upsert_wallet(again, false).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
}

#[test]
fn duplicate_label_with_force_replaces_in_place() {
    let (_t, s) = fresh();
    let a = WalletEntry::derive("alice", ABANDON, 0).unwrap();
    s.upsert_wallet(a.clone(), false).unwrap();
    let bob = WalletEntry::derive("bob", ABANDON, 5).unwrap();
    s.upsert_wallet(bob.clone(), false).unwrap();
    let new_alice = WalletEntry::derive("alice", LEGAL, 0).unwrap();
    s.upsert_wallet(new_alice.clone(), true).unwrap();
    let entries = s.list_wallets().unwrap();
    assert_eq!(entries.len(), 2);
    // alice is still first (order preserved on rewrite); content is new.
    assert_eq!(entries[0].label, "alice");
    assert_eq!(entries[0].address, new_alice.address);
    assert_eq!(entries[1].label, "bob");
}

#[test]
fn find_wallet_returns_none_for_unknown_label() {
    let (_t, s) = fresh();
    s.upsert_wallet(WalletEntry::derive("a", ABANDON, 0).unwrap(), false)
        .unwrap();
    assert!(s.find_wallet("missing").unwrap().is_none());
    assert!(s.find_wallet("a").unwrap().is_some());
}

#[test]
fn session_roundtrips_and_default_when_absent() {
    let (_t, s) = fresh();
    let default = s.load_session().unwrap();
    assert_eq!(default, Session::default());

    let updated = Session {
        rpc_addr: "10.0.0.1:9000".to_string(),
        chain_id: 42,
        current_wallet: Some("alice".to_string()),
    };
    s.save_session(&updated).unwrap();
    let loaded = s.load_session().unwrap();
    assert_eq!(loaded, updated);
}

#[test]
fn session_file_is_single_line() {
    let (_t, s) = fresh();
    s.save_session(&Session::default()).unwrap();
    let raw = fs::read_to_string(s.session_path()).unwrap();
    let line_count = raw.lines().count();
    assert_eq!(
        line_count, 1,
        "session.json should be a single JSON line; got {line_count}: {raw:?}"
    );
}

#[cfg(unix)]
#[test]
fn unix_modes_are_tight() {
    use std::os::unix::fs::PermissionsExt;
    let (_t, s) = fresh();
    s.upsert_wallet(WalletEntry::derive("a", ABANDON, 0).unwrap(), false)
        .unwrap();
    s.save_session(&Session::default()).unwrap();

    let dir_meta = fs::metadata(s.root()).unwrap();
    let dir_mode = dir_meta.permissions().mode() & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "store dir should be 0700, was {dir_mode:o}"
    );

    let wallets_meta = fs::metadata(s.wallets_path()).unwrap();
    let wm = wallets_meta.permissions().mode() & 0o777;
    assert_eq!(wm, 0o600, "wallets.jsonl should be 0600, was {wm:o}");

    let session_meta = fs::metadata(s.session_path()).unwrap();
    let sm = session_meta.permissions().mode() & 0o777;
    assert_eq!(sm, 0o600, "session.json should be 0600, was {sm:o}");
}
