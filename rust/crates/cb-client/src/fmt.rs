//! Output formatting. Kept deliberately terse and ANSI-free in v1 so the
//! REPL output is easy to assert against in tests; the designer's spec may
//! upgrade this later.

use cb_state::AccountState;
use cb_types::{Address, AssetId};

use crate::wallet::WalletEntry;

/// Resolve the user-facing label for the chain's native USD asset. The
/// chain's `chain_info.denom` is authoritative — when it's set (typical:
/// `"ULD"`), every USD-asset row should display that token name; the bare
/// string `"usd"` is only used as a last-resort fallback when the RPC is
/// unreachable.
pub fn uld_label(denom: Option<&str>) -> String {
    match denom {
        Some(d) if !d.trim().is_empty() => d.to_ascii_lowercase(),
        _ => "usd".to_string(),
    }
}

/// Render an `AssetId` using the chain's `denom` for `Usd` (so a chain
/// configured with `denom = "ULD"` shows `uld` everywhere) and the canonical
/// short name for the others.
///
/// Non-USD labels come straight from `cb-types` rather than a local match, so
/// listing an asset needs no edit here and the CLI can never disagree with the
/// canonical spelling.
pub fn asset_label(a: cb_types::AssetId, denom: Option<&str>) -> String {
    match a {
        cb_types::AssetId::Usd => uld_label(denom),
        other => other.lower_symbol().to_string(),
    }
}

/// Render an address as a short prefix `0xabcd…1234`.
pub fn short_addr(a: &Address) -> String {
    let h = hex::encode(a.0);
    format!("0x{}…{}", &h[..4], &h[h.len() - 4..])
}

/// Render the full address with `0x` prefix.
pub fn full_addr(a: &Address) -> String {
    format!("0x{}", hex::encode(a.0))
}

/// Render the wallet list. Always prints a header so the format is stable.
pub fn render_wallets(entries: &[WalletEntry], current: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("label                pubkey            privkey   address\n");
    if entries.is_empty() {
        out.push_str("(no wallets — try `login` and follow the prompts)\n");
        return out;
    }
    for w in entries {
        let cur_marker = if Some(w.label.as_str()) == current {
            "*"
        } else {
            " "
        };
        let addr = format!("0x{}", w.address);
        let line = format!(
            "{cur} {label:<19}  {pk:<16}  {sk:<8}  {addr}\n",
            cur = cur_marker,
            label = truncate(&w.label, 19),
            pk = w.pubkey_prefix(),
            sk = w.privkey_masked(),
            addr = addr,
        );
        out.push_str(&line);
    }
    out
}

/// Render `getAccount` output as a stable balance table with one row per
/// listed asset (zero-rows included). Width-padded so columns line up.
///
/// Rows are driven by `AssetId::ALL`, so a newly listed asset appears without
/// an edit here — an account that holds it must never be shown a balance table
/// that silently omits it.
pub fn render_balance(addr: &Address, s: &AccountState, denom: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "address: {}   nonce: {}\n",
        full_addr(addr),
        s.nonce
    ));
    out.push_str("asset    available    locked\n");
    for asset in AssetId::ALL {
        let avail = s.available(asset);
        let locked = s.locked_of(asset);
        let label = asset_label(asset, denom);
        out.push_str(&format!(
            "{label:<5}    {avail:<11}  {locked}\n",
            label = label,
            avail = avail,
            locked = locked,
        ));
    }
    out
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
