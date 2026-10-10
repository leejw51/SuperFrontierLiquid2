//! Interactive REPL. Reads a line, runs it through clap, dispatches to the
//! same `Command` enum used by the binary one-shot path. Empty lines no-op;
//! Ctrl-D (EOF) exits. We do not link `rustyline` — the prompt is plain
//! `print! + flush + read_line`, which is enough for v1 and keeps the
//! dependency surface small.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::time::Duration;

use crate::cli::{parse_repl_line, Command};
use crate::client::{self, SubmitOutcome};
use crate::fmt as ulfmt;
use crate::store::{Session, Store};
use crate::wallet::WalletEntry;
use cb_tx::{SignedTx, Transaction, TxBody};

/// REPL entry-point used by `main`. Reads from stdin, writes to stdout, and
/// uses real-tty `StdPrompts` for interactive `login` prompts.
pub fn run(store: Store) -> Result<()> {
    // Important: don't hold `stdin().lock()` or `stdout().lock()` across the
    // REPL loop. `prompts.read_line` / `print!` inside `dispatch` re-lock the
    // same handles, and on macOS the nested lock blocks reads silently
    // (manifests as `login` accepting the label and then never showing the
    // mnemonic prompt). Pass unlocked handles; std locks per call.
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut output = std::io::stdout();
    let mut prompts = crate::prompts::StdPrompts;
    run_inner(
        store,
        &mut input,
        &mut output,
        &mut prompts,
        /*auto_login=*/ true,
    )
}

/// Test-friendly variant: drive the REPL with arbitrary `BufRead` / `Write`
/// and an injectable `Prompts`. Tests use [`crate::prompts::ScriptedPrompts`]
/// to replay canned answers without needing a real tty.
pub fn run_with_io<R: BufRead, W: Write>(
    store: Store,
    input: &mut R,
    output: &mut W,
    prompts: &mut dyn crate::prompts::Prompts,
) -> Result<()> {
    run_inner(store, input, output, prompts, /*auto_login=*/ false)
}

fn run_inner<R: BufRead, W: Write>(
    store: Store,
    input: &mut R,
    output: &mut W,
    prompts: &mut dyn crate::prompts::Prompts,
    auto_login: bool,
) -> Result<()> {
    write_banner(&store, output)?;
    if auto_login && !has_current_wallet(&store) {
        writeln!(output, "no wallet loaded \u{2014} entering login")?;
        let cmd = Command::Login {
            label: None,
            mnemonic: None,
            index: None,
            force: false,
        };
        match dispatch(&store, prompts, None, None, cmd) {
            Ok(text) => {
                if !text.is_empty() {
                    output.write_all(text.as_bytes())?;
                    if !text.ends_with('\n') {
                        writeln!(output)?;
                    }
                }
            }
            Err(e) => writeln!(output, "login error: {e}")?,
        }
    }
    loop {
        write!(output, "ul> ")?;
        output.flush()?;
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D)
            writeln!(output)?;
            return Ok(());
        }
        match parse_repl_line(&line) {
            Ok(None) => continue,
            Ok(Some(cli)) => {
                let cmd = match cli.cmd {
                    Some(c) => c,
                    None => {
                        writeln!(output, "(empty command — try `help`)")?;
                        continue;
                    }
                };
                if matches!(cmd, Command::Quit) {
                    return Ok(());
                }
                let outcome = dispatch(&store, prompts, cli.rpc, cli.chain_id, cmd);
                match outcome {
                    Ok(text) => {
                        if !text.is_empty() {
                            output.write_all(text.as_bytes())?;
                            if !text.ends_with('\n') {
                                writeln!(output)?;
                            }
                        }
                    }
                    Err(e) => {
                        writeln!(output, "error: {e}")?;
                    }
                }
            }
            Err(e) => {
                // clap prints the error itself; render its display form so
                // the REPL stays a single output stream.
                writeln!(output, "{}", e.to_string().trim_end())?;
            }
        }
    }
}

/// Execute one parsed `Command`. Returns the text to print (may be empty).
/// One-shot binary mode reuses this directly. `prompts` is consulted by
/// `Login` when any of `--label`/`--mnemonic`/`--index` is omitted; other
/// commands ignore it.
pub fn dispatch(
    store: &Store,
    prompts: &mut dyn crate::prompts::Prompts,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    cmd: Command,
) -> Result<String> {
    match cmd {
        Command::Login {
            label,
            mnemonic,
            index,
            force,
        } => cmd_login(store, prompts, label, mnemonic, index, force),
        Command::Select { label } => cmd_select(store, label),
        Command::Wallets => cmd_wallets(store),
        Command::Balance { addr } => cmd_balance(store, rpc_override, addr),
        Command::Send { to, asset, amount } => {
            cmd_send(store, rpc_override, chain_override, to, asset, amount)
        }
        Command::Addr { mnemonic, index } => cmd_addr(prompts, mnemonic, index),
        Command::SetRpc { addr } => cmd_set_rpc(store, addr),
        Command::SetChainId { chain_id } => cmd_set_chain_id(store, chain_id),
        Command::Position { addr, json } => cmd_position(store, rpc_override, addr, json),
        Command::Margin { addr, json } => cmd_margin(store, rpc_override, addr, json),
        Command::Funding { market, json } => cmd_funding(store, rpc_override, market, json),
        Command::Mark { market, json } => cmd_mark(store, rpc_override, market, json),
        Command::Book {
            market,
            depth,
            json,
        } => cmd_book(store, rpc_override, market, depth, json),
        Command::Markets { json } => cmd_markets(store, rpc_override, json),
        Command::Place {
            market,
            side,
            qty,
            price,
            flags,
        } => cmd_place(
            store,
            rpc_override,
            chain_override,
            market,
            side,
            qty,
            price,
            flags,
        ),
        Command::Cancel { kind, order_id } => {
            cmd_cancel(store, rpc_override, chain_override, kind, order_id)
        }
        Command::Faucet { amount } => cmd_faucet(store, rpc_override, chain_override, amount),
        Command::PerpPlace {
            market,
            side,
            qty,
            price,
            flags,
        } => {
            // Deprecated alias: forward to the unified `place` handler. The
            // deprecation note must surface even on error (e.g. RPC down)
            // so operators always see the migration hint, never a bare
            // connection error that leaves them guessing why the new path
            // would have helped.
            match cmd_place(
                store,
                rpc_override,
                chain_override,
                market,
                side,
                qty,
                price,
                flags,
            ) {
                Ok(body) => Ok(format!(
                    "note: `perp-place` is deprecated; use `place perp:N ...`\n{body}"
                )),
                Err(e) => Err(anyhow!(
                    "note: `perp-place` is deprecated; use `place perp:N ...`: {e}"
                )),
            }
        }
        Command::PerpCancel { order_id } => match cmd_cancel(
            store,
            rpc_override,
            chain_override,
            "perp".to_string(),
            order_id,
        ) {
            Ok(body) => Ok(format!(
                "note: `perp-cancel` is deprecated; use `cancel perp <id>`\n{body}"
            )),
            Err(e) => Err(anyhow!(
                "note: `perp-cancel` is deprecated; use `cancel perp <id>`: {e}"
            )),
        },
        Command::Liquidate { addr, market } => {
            cmd_liquidate(store, rpc_override, chain_override, addr, market)
        }
        Command::Oracle {
            market,
            price,
            height,
        } => cmd_oracle(store, rpc_override, chain_override, market, price, height),
        Command::Help { cmd } => cmd_help(cmd),
        Command::Quit => Ok(String::new()),
    }
}

// ---------- command handlers ----------

fn cmd_login(
    store: &Store,
    prompts: &mut dyn crate::prompts::Prompts,
    label: Option<String>,
    mnemonic: Option<String>,
    index: Option<u32>,
    force: bool,
) -> Result<String> {
    // Resolve each field: prefer the CLI flag, else prompt. Mnemonic always
    // goes through `read_password` (terminal echo OFF) when prompted.
    let label = match label {
        Some(s) if !s.trim().is_empty() => s,
        Some(_) => return Err(anyhow!("label must not be empty")),
        None => {
            let s = prompts.read_line("Label: ")?;
            if s.trim().is_empty() {
                return Err(anyhow!("label must not be empty"));
            }
            s
        }
    };

    let mnemonic = match mnemonic {
        Some(s) => s,
        None => {
            let s = prompts.read_password("Mnemonic (hidden): ")?;
            if s.is_empty() {
                return Err(anyhow!("mnemonic must not be empty"));
            }
            s
        }
    };

    let index = match index {
        Some(n) => n,
        None => {
            let raw = prompts.read_line("Wallet index [0]: ")?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                0
            } else {
                trimmed
                    .parse::<u32>()
                    .map_err(|_| anyhow!("wallet index must be a non-negative integer"))?
            }
        }
    };

    let entry = WalletEntry::derive(&label, &mnemonic, index)?;
    store.upsert_wallet(entry.clone(), force)?;
    let mut sess = store.load_session()?;
    sess.current_wallet = Some(label.clone());
    store.save_session(&sess)?;
    Ok(format!(
        "logged in as {} (0x{}); set as current\n",
        label, entry.address
    ))
}

fn cmd_select(store: &Store, label: String) -> Result<String> {
    let entry = store
        .find_wallet(&label)?
        .ok_or_else(|| anyhow!("no wallet with label '{label}'"))?;
    let mut sess = store.load_session()?;
    sess.current_wallet = Some(label.clone());
    store.save_session(&sess)?;
    Ok(format!("current wallet: {} (0x{})\n", label, entry.address))
}

fn cmd_wallets(store: &Store) -> Result<String> {
    let entries = store.list_wallets()?;
    let sess = store.load_session()?;
    Ok(ulfmt::render_wallets(
        &entries,
        sess.current_wallet.as_deref(),
    ))
}

fn cmd_balance(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    addr_arg: Option<String>,
) -> Result<String> {
    let sess = store.load_session()?;
    let addr = match addr_arg {
        Some(s) => client::parse_address(&s)?,
        None => current_wallet(store, &sess)?.address()?,
    };
    let rpc = effective_rpc(rpc_override, &sess)?;
    // Single LocalSet round-trip: get_account + chain_info. denom is purely
    // cosmetic (relabels the AssetId::Usd row); on chain_info failure we
    // fall back to "usd" rather than failing the balance command.
    let (state, denom) = client::run_call(rpc, |c| async move {
        let st = client::get_account(&c, &addr).await?;
        let denom = client::chain_info(&c).await.ok().map(|(_, d)| d);
        Ok::<_, anyhow::Error>((st, denom))
    })?;
    Ok(ulfmt::render_balance(&addr, &state, denom.as_deref()))
}

fn cmd_send(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    to: String,
    asset: String,
    amount: u128,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let from = wallet.address()?;
    let to_addr = client::parse_address(&to)?;
    let asset = client::parse_asset(&asset)?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let sk = wallet.secret_key()?;

    let (tx_hash, used_nonce, outcome) = client::run_call(rpc, move |c| async move {
        let st = client::get_account(&c, &from).await?;
        let nonce = st.nonce;
        let tx = Transaction {
            nonce,
            chain_id,
            body: TxBody::Transfer {
                to: to_addr,
                asset,
                amount,
            },
        };
        let signed = SignedTx::sign_with(&sk, tx).context("sign tx")?;
        let hash = signed.hash();
        let outcome = client::submit_tx(&c, &signed).await?;
        Ok::<_, anyhow::Error>((hash, nonce, outcome))
    })?;

    match outcome {
        SubmitOutcome::Ok => Ok(format!(
            "submitted: tx_hash=0x{} nonce={used_nonce}\n",
            hex::encode(tx_hash.0),
        )),
        SubmitOutcome::Rejected { code, message } => Err(anyhow!("rejected ({code}): {message}")),
    }
}

// (Helper removed — `send` now reports the actual signing nonce.)

fn cmd_addr(
    prompts: &mut dyn crate::prompts::Prompts,
    mnemonic: Option<String>,
    index: Option<u32>,
) -> Result<String> {
    use std::io::{BufRead, IsTerminal};
    let mnemonic = match mnemonic {
        Some(s) => s,
        None => {
            if std::io::stdin().is_terminal() {
                prompts.read_password("Mnemonic (hidden): ")?
            } else {
                let mut s = String::new();
                std::io::stdin()
                    .lock()
                    .read_line(&mut s)
                    .context("read mnemonic from stdin")?;
                s.trim_end_matches(['\r', '\n']).to_string()
            }
        }
    };
    if mnemonic.trim().is_empty() {
        return Err(anyhow!("mnemonic must not be empty"));
    }
    let entry = WalletEntry::derive("addr", &mnemonic, index.unwrap_or(0))?;
    Ok(format!("0x{}\n", entry.address))
}

fn cmd_set_rpc(store: &Store, addr: SocketAddr) -> Result<String> {
    let mut sess = store.load_session()?;
    sess.rpc_addr = addr.to_string();
    store.save_session(&sess)?;
    Ok(format!("rpc_addr = {addr}\n"))
}

fn cmd_set_chain_id(store: &Store, chain_id: u32) -> Result<String> {
    let mut sess = store.load_session()?;
    sess.chain_id = chain_id;
    store.save_session(&sess)?;
    Ok(format!("chain_id = {chain_id}\n"))
}

// ---------------------------------------------------------------------------
// Derivatives commands (M6)
// ---------------------------------------------------------------------------

fn resolve_addr(
    store: &Store,
    sess: &Session,
    addr_arg: Option<String>,
) -> Result<cb_types::Address> {
    match addr_arg {
        Some(s) => client::parse_address(&s),
        None => current_wallet(store, sess)?.address(),
    }
}

fn cmd_position(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    addr_arg: Option<String>,
    json: bool,
) -> Result<String> {
    let sess = store.load_session()?;
    let addr = resolve_addr(store, &sess, addr_arg)?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    let positions = client::run_call(rpc, |c| async move {
        crate::derivs::rpc_get_positions(&c, &addr).await
    })?;
    Ok(crate::derivs::render_positions(&addr, &positions, json))
}

fn cmd_margin(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    addr_arg: Option<String>,
    json: bool,
) -> Result<String> {
    let sess = store.load_session()?;
    let addr = resolve_addr(store, &sess, addr_arg)?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    let m = client::run_call(rpc, |c| async move {
        crate::derivs::rpc_get_margin(&c, &addr).await
    })?;
    Ok(crate::derivs::render_margin(&addr, &m, json))
}

fn cmd_funding(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    market: String,
    json: bool,
) -> Result<String> {
    let sess = store.load_session()?;
    let market = crate::derivs::parse_market(&market)?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    // We also need the market's funding cadence to compute next-tick height.
    // Best-effort: getPerpMarket → next_tick = last_tick + cadence; if the
    // perp record is missing the next-tick is reported as 0 (display only).
    let (f, perp) = client::run_call(rpc, |c| async move {
        let f = crate::derivs::rpc_get_funding(&c, &market).await?;
        let perps = crate::derivs::rpc_list_perp_markets(&c).await.ok();
        let perp = perps.and_then(|v| v.into_iter().find(|p| p.id == market));
        Ok::<_, anyhow::Error>((f, perp))
    })?;
    let next_tick = perp
        .map(|p| {
            let cadence = u64::from(p.funding_interval_blocks);
            if cadence == 0 {
                0
            } else {
                f.last_tick_height.saturating_add(cadence.max(1))
            }
        })
        .unwrap_or(0);
    Ok(crate::derivs::render_funding(&market, &f, next_tick, json))
}

fn cmd_mark(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    market: String,
    json: bool,
) -> Result<String> {
    let sess = store.load_session()?;
    let market = crate::derivs::parse_market(&market)?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    let m = client::run_call(rpc, |c| async move {
        crate::derivs::rpc_get_mark(&c, &market).await
    })?;
    Ok(crate::derivs::render_mark(&market, &m, json))
}

fn cmd_book(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    market: String,
    depth: Option<u32>,
    json: bool,
) -> Result<String> {
    let sess = store.load_session()?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    let depth = depth.unwrap_or(10);
    match crate::spot::parse_market_ref(&market)? {
        crate::spot::MarketRef::Spot(mid) => {
            // S4 wires the real `getOrderBook` RPC; until it lands locally
            // the server stub returns "not implemented" and we render a
            // graceful one-liner instead of crashing the REPL.
            match client::run_call(rpc, |c| async move {
                crate::spot::rpc_get_spot_book(&c, mid, depth).await
            }) {
                Ok(snap) => Ok(crate::spot::render_spot_book(&snap, json)),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not implemented") || msg.contains("getOrderBook") {
                        Ok(format!(
                            "spot book unavailable: {msg}\n\
                             (S4 wires the real RPC; rebuild the sequencer once it lands)\n"
                        ))
                    } else {
                        Err(e)
                    }
                }
            }
        }
        crate::spot::MarketRef::Deriv(did) => {
            let snap = client::run_call(rpc, |c| async move {
                crate::derivs::rpc_get_perp_book(&c, &did, depth).await
            })?;
            Ok(crate::derivs::render_book(&snap, json))
        }
    }
}

fn cmd_markets(store: &Store, rpc_override: Option<SocketAddr>, json: bool) -> Result<String> {
    let sess = store.load_session()?;
    let rpc = effective_rpc(rpc_override, &sess)?;
    let (perps, futures) = client::run_call(rpc, |c| async move {
        let perps = crate::derivs::rpc_list_perp_markets(&c).await?;
        let futures = crate::derivs::rpc_list_future_markets(&c).await?;
        Ok::<_, anyhow::Error>((perps, futures))
    })?;
    Ok(crate::derivs::render_markets(&perps, &futures, json))
}

/// Generate a per-call `client_order_id` cookie. v1 uses the wall-clock
/// nanos packed into the first 8 bytes; higher-level tooling can pass its
/// own once that surface lands. Same scheme used by both spot and perp
/// `place` to keep one helper.
fn fresh_client_order_id() -> cb_types::H256 {
    let mut cid = [0u8; 32];
    cid[..8].copy_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            .to_be_bytes(),
    );
    cb_types::H256(cid)
}

#[allow(clippy::too_many_arguments)]
fn cmd_place(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    market: String,
    side: String,
    qty: u128,
    price: u128,
    flags: Vec<String>,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let from = wallet.address()?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let sk = wallet.secret_key()?;
    let side_parsed = crate::derivs::parse_side(&side)?;
    let client_order_id = fresh_client_order_id();
    let market_ref = crate::spot::parse_market_ref(&market)?;
    let (tx_hash, used_nonce, outcome) = match market_ref {
        crate::spot::MarketRef::Spot(mid) => {
            let (tif, post_only) = crate::spot::parse_spot_flags(&flags)?;
            crate::spot::submit_spot_place(
                rpc,
                chain_id,
                sk,
                from,
                mid,
                side_parsed,
                qty,
                price,
                tif,
                post_only,
                client_order_id,
            )?
        }
        crate::spot::MarketRef::Deriv(did) => {
            let (tif, post_only, reduce_only) = crate::derivs::parse_perp_flags(&flags)?;
            crate::derivs::submit_perp_place(
                rpc,
                chain_id,
                sk,
                from,
                did,
                side_parsed,
                qty,
                price,
                tif,
                post_only,
                reduce_only,
                client_order_id,
            )?
        }
    };
    match outcome {
        crate::client::SubmitOutcome::Ok => Ok(format!(
            "submitted: tx_hash=0x{} nonce={used_nonce}\n",
            hex::encode(tx_hash.0),
        )),
        crate::client::SubmitOutcome::Rejected { code, message } => {
            Err(anyhow!("rejected ({code}): {message}"))
        }
    }
}

fn cmd_cancel(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    kind: String,
    order_id: String,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let from = wallet.address()?;
    let oid = crate::derivs::parse_h256(&order_id)?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let sk = wallet.secret_key()?;
    let (tx_hash, used_nonce, outcome) = match kind.to_ascii_lowercase().as_str() {
        "spot" => crate::spot::submit_spot_cancel(rpc, chain_id, sk, from, oid)?,
        "perp" | "future" | "futures" => {
            crate::derivs::submit_perp_cancel(rpc, chain_id, sk, from, oid)?
        }
        other => {
            return Err(anyhow!(
                "unknown cancel kind `{other}` (expected `spot` or `perp`)"
            ))
        }
    };
    match outcome {
        crate::client::SubmitOutcome::Ok => Ok(format!(
            "submitted: tx_hash=0x{} nonce={used_nonce}\n",
            hex::encode(tx_hash.0),
        )),
        crate::client::SubmitOutcome::Rejected { code, message } => {
            Err(anyhow!("rejected ({code}): {message}"))
        }
    }
}

fn cmd_faucet(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    amount: Option<u128>,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let recipient = wallet.address()?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let amount = amount.unwrap_or(crate::faucet::DEFAULT_FAUCET_AMOUNT);

    // Fail-fast env check: derives the faucet signer (and therefore validates
    // mnemonic + index) before we attempt any RPC. This keeps the `faucet`
    // error message useful when the operator forgets to source perps_env.sh
    // even if the sequencer happens to be unreachable.
    let _ = crate::faucet::resolve_faucet_signer()?;

    // Best-effort denom probe so the receipt prints `ULD` instead of `usd`
    // when the chain advertises a custom denom. Failure is non-fatal — we
    // still want to attempt the transfer.
    let denom = client::run_call(rpc, |c| async move {
        Ok(client::chain_info(&c).await.ok().map(|(_, d)| d))
    })
    .unwrap_or(None);
    let label = ulfmt::uld_label(denom.as_deref());

    let (from, tx_hash, used_nonce, outcome) =
        crate::faucet::submit_faucet(rpc, chain_id, recipient, amount)?;
    match outcome {
        crate::client::SubmitOutcome::Ok => Ok(format!(
            "faucet: sent {amount} {label} from 0x{from} to 0x{rcpt}; \
             tx_hash=0x{tx} nonce={used_nonce}\n",
            from = hex::encode(from.0),
            rcpt = hex::encode(recipient.0),
            tx = hex::encode(tx_hash.0),
        )),
        crate::client::SubmitOutcome::Rejected { code, message } => Err(anyhow!(
            "faucet rejected ({code}): {message} \
             (faucet=0x{from}, recipient=0x{rcpt})",
            from = hex::encode(from.0),
            rcpt = hex::encode(recipient.0),
        )),
    }
}

fn cmd_liquidate(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    addr: String,
    market: String,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let from = wallet.address()?;
    let owner = client::parse_address(&addr)?;
    let market = crate::derivs::parse_market(&market)?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let sk = wallet.secret_key()?;
    let (tx_hash, used_nonce, outcome) =
        crate::derivs::submit_liquidate(rpc, chain_id, sk, from, owner, market)?;
    match outcome {
        crate::client::SubmitOutcome::Ok => Ok(format!(
            "submitted: tx_hash=0x{} nonce={used_nonce}\n",
            hex::encode(tx_hash.0),
        )),
        crate::client::SubmitOutcome::Rejected { code, message } => {
            Err(anyhow!("rejected ({code}): {message}"))
        }
    }
}

fn cmd_oracle(
    store: &Store,
    rpc_override: Option<SocketAddr>,
    chain_override: Option<u32>,
    market: String,
    price: u128,
    height: u64,
) -> Result<String> {
    let sess = store.load_session()?;
    let wallet = current_wallet(store, &sess)?;
    let from = wallet.address()?;
    let market = crate::derivs::parse_market(&market)?;
    let chain_id = chain_override.unwrap_or(sess.chain_id);
    let rpc = effective_rpc(rpc_override, &sess)?;
    let sk = wallet.secret_key()?;
    let (tx_hash, used_nonce, outcome) =
        crate::derivs::submit_oracle_update(rpc, chain_id, sk, from, market, price, height)?;
    match outcome {
        crate::client::SubmitOutcome::Ok => Ok(format!(
            "submitted: tx_hash=0x{} nonce={used_nonce}\n",
            hex::encode(tx_hash.0),
        )),
        crate::client::SubmitOutcome::Rejected { code, message } => {
            Err(anyhow!("rejected ({code}): {message}"))
        }
    }
}

fn cmd_help(target: Option<String>) -> Result<String> {
    let body = match target.as_deref() {
        None => HELP_TEXT.to_string(),
        Some(name) => help_for(name)
            .ok_or_else(|| anyhow!("no help for '{name}'"))?
            .to_string(),
    };
    Ok(body)
}

const HELP_TEXT: &str = "\
cb-client commands:
  login [--label <s>] [--mnemonic <s>] [--index <u32>] [--force]
                          Derive a wallet and store it locally.
  select <label>          Switch the current wallet.
  wallets                 List stored wallets.
  balance [<hex>]         Show on-chain account state.
  send <to> <asset> <amt> Sign + submit a Transfer (asset = btc|eth|usd|uld).
  faucet [<amount>]       Mint test ULD into the current wallet.
                          Default amount = 1_000_000_000_000.
  addr [--mnemonic <s>] [--index <u32>]
                          Derive `0x{addr}` from (mnemonic, index). Stateless.
  set-rpc <addr>          Update persisted RPC address.
  set-chain-id <id>       Update persisted chain id.

  -- trading (spot + perp + futures) --
  place <market> <side> <qty> <price> [flags...]
                          market = spot:btc_usd | spot:eth_usd | perp:N | future:N
                          side   = buy | sell
                          flags  = gtc (default) | ioc | fok | post-only | reduce-only
                                   (reduce-only valid only for perp:/future:)
  cancel <kind> <order-id>
                          kind = spot | perp (use perp for futures too).
  book <market> [<depth>] [--json]
                          Top-N levels of a spot or perp/future orderbook.
  markets [--json]        List spot, perpetual, and dated-future markets.
  position [<addr>] [--json]
                          Open derivative positions for `<addr>`.
  margin [<addr>] [--json]
                          Margin-account snapshot (collateral, PnL, bad debt).
  funding <market> [--json]
                          Funding index + rate + next-tick height (perp:/future:).
  mark <market> [--json]  Current mark price + components (perp:/future:).
  liquidate <addr> <market>
                          Submit a LiquidationTrigger for `(addr, market)`.
  oracle <market> <price> --height <u64>
                          Sign + submit an OracleUpdate (current wallet must
                          be the market's oracle_authority).

  -- deprecated aliases --
  perp-place / perp-cancel  Use `place` / `cancel perp` instead.

  help [cmd]              Show this help, or help for one command.
  quit / exit             Leave the REPL.

Global flags (any command): --rpc <addr>, --chain-id <u32>.
Persistence root: ~/.cb_client/   (mnemonics + privkeys live here, mode 0600)
";

fn help_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "login" => "login [--label <s>] [--mnemonic <s>] [--index <u32>] [--force]\n  Derive a deterministic keypair from `(mnemonic, index)` and store it.\n  Omitted flags are prompted; mnemonic input is hidden (no echo).",
        "select" => "select <label>\n  Make the named wallet the current one for subsequent send/balance.",
        "wallets" => "wallets\n  List stored wallets. The current wallet is marked with `*`.",
        "balance" => "balance [<hex>]\n  Print account nonce + balances. Defaults to the current wallet.\n  Native USD asset displays under the chain's `denom` label (e.g. `uld`).",
        "send" => "send <to> <asset> <amount>\n  Sign + submit a Transfer with the current wallet.\n  asset is one of btc | eth | usd | uld (uld is an alias for usd);\n  amount is a u128 decimal.",
        "faucet" => "faucet [<amount>]\n  Sign + submit a Transfer of <amount> ULD from the faucet wallet to the\n  current wallet. Default amount = 1_000_000_000_000 (1T raw ULD).\n\n  Faucet signer is resolved from the environment, in order:\n    1. UL_FAUCET_MNEMONIC (with optional UL_FAUCET_INDEX, default 0)\n    2. MYWALLET3_MNEMONIC (auto-set by `make startperps` /\n       `scripts/perps_env.sh`)\n  No mnemonic? Set UL_FAUCET_MNEMONIC or run inside `make startperps`.",
        "addr" => "addr [--mnemonic <s>] [--index <u32>]\n  Stateless address deriver. Prints `0x{40 hex}` to stdout.\n  Mnemonic is read hidden from TTY, or one line from stdin pipe.",
        "set-rpc" => "set-rpc <addr>\n  Persist the sequencer RPC address.",
        "set-chain-id" => "set-chain-id <u32>\n  Persist the chain id used when signing.",
        "position" => "position [<addr>] [--json]\n  Open positions for `<addr>` (default: current wallet).\n  Output: market, side, qty, entry_px_q8, margin_mode, entry_funding_index.",
        "margin" => "margin [<addr>] [--json]\n  Margin account snapshot (collateral, unrealized PnL, bad debt).",
        "funding" => "funding <market> [--json]\n  Funding index + rate + next-tick block height for `perp:N` / `future:N`.",
        "mark" => "mark <market> [--json]\n  Mark price + premium-EMA + last-computed-at-height.",
        "book" => "book <market> [<depth>] [--json]\n  Top-N levels of an orderbook (default depth = 10).\n  market is `spot:btc_usd`, `spot:eth_usd`, `perp:N`, or `future:N`.\n  Bids are emitted highest-first, asks lowest-first.",
        "markets" => "markets [--json]\n  List every spot, perp, and dated-future market with key params.",
        "place" => "place <market> <side> <qty> <price> [flags...]\n  Sign + submit a place order with the current wallet.\n  market = spot:btc_usd | spot:eth_usd | perp:N | future:N\n  side   = buy | sell\n  qty    = u128 base-units decimal\n  price  = u128 1e8-scaled (spot prices use the same wire scale as perp;\n           the executor enforces price % MarketConfig.tick_size == 0)\n  flags  = gtc (default) | ioc | fok | post-only | reduce-only\n           reduce-only is rejected for `spot:` markets (no field on-wire).",
        "cancel" => "cancel <kind> <order-id>\n  Sign + submit a cancel for an existing order.\n  kind = spot   → CancelOrder (spot CLOB)\n         perp   → CancelPerpOrder (covers perp + dated future books)\n  order-id is 32-byte hex (with or without `0x`).",
        "perp-place" => "perp-place <market> <side> <qty> <price> [gtc|ioc|fok] [post-only] [reduce-only]\n  DEPRECATED: use `place perp:N <side> <qty> <price> ...` instead.\n  Forwards to the unified `place` handler.",
        "perp-cancel" => "perp-cancel <order-id>\n  DEPRECATED: use `cancel perp <order-id>` instead.\n  Forwards to the unified `cancel` handler.",
        "liquidate" => "liquidate <addr> <market>\n  Submit a LiquidationTrigger for the given (owner, market).",
        "oracle" => "oracle <market> <price> --height <u64>\n  Sign + submit an OracleUpdate. price is 1e8-scaled. Sender must be\n  the market's oracle_authority (set in genesis).",
        "help" => "help [cmd]\n  Print general help or help for `cmd`.",
        "quit" | "exit" => "quit | exit\n  Leave the REPL.",
        _ => return None,
    })
}

// ---------- shared helpers ----------

/// Render the multi-line startup banner: working directory, wallet-store
/// root, configured RPC address, and a fast (300ms cap) liveness probe of
/// that RPC. Best-effort throughout — a failing probe or unreadable cwd
/// must never panic, just degrade to `?` / `offline`.
fn has_current_wallet(store: &Store) -> bool {
    match store.load_session() {
        Ok(sess) => match sess.current_wallet.as_ref() {
            Some(label) => store
                .find_wallet(label)
                .map(|w| w.is_some())
                .unwrap_or(false),
            None => false,
        },
        Err(_) => false,
    }
}

fn write_banner<W: Write>(store: &Store, output: &mut W) -> std::io::Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "?".to_string());
    let store_root = store.root().display().to_string();

    // Pull the configured RPC address from the persisted session. If the
    // session file is unreadable, fall through to `?` so the banner still
    // renders rather than blocking the REPL on a config error.
    let (rpc_text, server_text) = match store.load_session() {
        Ok(sess) => {
            let raw = sess.rpc_addr.clone();
            match raw.parse::<SocketAddr>() {
                Ok(addr) => {
                    let online =
                        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300))
                            .is_ok();
                    (raw, if online { "online" } else { "offline" }.to_string())
                }
                Err(_) => (raw, "offline (bad address)".to_string()),
            }
        }
        Err(_) => ("?".to_string(), "offline".to_string()),
    };

    writeln!(output, "cb-client")?;
    writeln!(output, "  cwd:    {cwd}")?;
    writeln!(output, "  store:  {store_root}")?;
    if let Ok(sess) = store.load_session() {
        if let Some(label) = sess.current_wallet.as_ref() {
            if let Ok(Some(entry)) = store.find_wallet(label) {
                writeln!(output, "  wallet: {} (0x{})", entry.label, entry.address)?;
            }
        }
    }
    writeln!(output, "  rpc:    {rpc_text}")?;
    writeln!(output, "  server: {server_text}")?;
    writeln!(output)?;
    writeln!(output, "type `help` for commands, `quit` to exit")?;
    Ok(())
}

fn current_wallet(store: &Store, sess: &Session) -> Result<WalletEntry> {
    let label = sess
        .current_wallet
        .as_ref()
        .ok_or_else(|| anyhow!("no current wallet — run `login` or `select <label>`"))?;
    store
        .find_wallet(label)?
        .ok_or_else(|| anyhow!("current_wallet '{label}' not found in wallets.jsonl"))
}

fn effective_rpc(override_: Option<SocketAddr>, sess: &Session) -> Result<SocketAddr> {
    match override_ {
        Some(a) => Ok(a),
        None => sess.rpc_socket(),
    }
}
