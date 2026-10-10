//! `clap` definitions. The same `Command` enum is used by both the binary
//! entry-point (`cb-client login ...`) and the REPL dispatcher
//! (`ul> login ...`) — the REPL just splits the line by whitespace and hands
//! it back to clap as if it were `argv`.
//!
//! Subcommand names live here as the single source of truth so the REPL
//! `help` output and `--help` from the binary stay in sync automatically.

use clap::{Parser, Subcommand};
use std::net::SocketAddr;

#[derive(Parser, Debug)]
#[command(
    name = "cb-client",
    about = "SuperFrontierLiquid REPL/CLI client",
    version,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Override the configured RPC address for this invocation only.
    /// Persisted state is untouched; use `set-rpc` to change the default.
    #[arg(long, global = true)]
    pub rpc: Option<SocketAddr>,

    /// Override the configured chain id for this invocation only.
    #[arg(long, global = true)]
    pub chain_id: Option<u32>,

    #[command(subcommand)]
    pub cmd: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Derive a wallet from `(mnemonic, index)` and store it locally.
    ///
    /// Any of `--label`, `--mnemonic`, `--index` may be omitted; when omitted
    /// the value is read interactively. The mnemonic is read with terminal
    /// echo OFF (like a password prompt), so it never lands in shell history
    /// or `ps` output.
    Login {
        #[arg(long)]
        label: Option<String>,
        /// Free-form mnemonic. If omitted, prompted with hidden input.
        #[arg(long)]
        mnemonic: Option<String>,
        /// Wallet index (default 0). If omitted, prompted with default 0.
        #[arg(long)]
        index: Option<u32>,
        /// Replace an existing entry with the same label.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Switch the current wallet to `<label>`.
    Select { label: String },
    /// List stored wallets.
    Wallets,
    /// Show on-chain account state for the given address (or current wallet).
    Balance {
        /// Hex-encoded address (`0x...`). Defaults to the current wallet.
        addr: Option<String>,
    },
    /// Sign and submit a Transfer tx with the current wallet.
    Send {
        /// Recipient address (hex, 0x-prefixed or not).
        to: String,
        /// One of `btc`, `eth`, `usd` / `uld` (case-insensitive; uld is an alias for usd).
        asset: String,
        /// Amount as a u128 decimal.
        amount: u128,
    },
    /// Derive an address from `(mnemonic, index)` and print `0x{40 hex}`.
    /// Stateless: no wallet store I/O, no network. If `--mnemonic` is
    /// omitted: prompt hidden when stdin is a TTY, else read one line from
    /// stdin (for scripting / piping into `make testblockchain`).
    Addr {
        /// Free-form mnemonic. If omitted, read from TTY (hidden) or stdin.
        #[arg(long)]
        mnemonic: Option<String>,
        /// Wallet index (default 0).
        #[arg(long)]
        index: Option<u32>,
    },
    /// Update persisted RPC address.
    SetRpc { addr: SocketAddr },
    /// Update persisted chain id.
    SetChainId { chain_id: u32 },
    // ---------------------------------------------------------------------
    // Derivatives reads (M6). Each command takes an optional `--json` flag
    // that switches the renderer from the human-readable table to a single
    // JSON line for scripting.
    // ---------------------------------------------------------------------
    /// Show open positions for `<addr>` (default: current wallet).
    Position {
        /// Hex-encoded address (`0x...`). Defaults to the current wallet.
        addr: Option<String>,
        /// Emit JSON instead of the human table.
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show margin account (collateral, unrealized PnL, bad debt) for `<addr>`.
    Margin {
        addr: Option<String>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show current funding index + last rate + next-tick height for a perp.
    Funding {
        /// Market reference: `perp:N` or `future:N`.
        market: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show current mark price + components for a derivative market.
    Mark {
        market: String,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show top N levels of an orderbook (spot or perp/future).
    Book {
        /// `spot:btc_usd` | `spot:eth_usd` | `perp:N` | `future:N`.
        market: String,
        /// Levels per side (default 10).
        depth: Option<u32>,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List all spot + perpetual + dated futures markets.
    Markets {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Sign + submit a place order for spot, perp, or futures.
    ///
    /// Routing is by `<market>` prefix:
    ///   `spot:btc_usd` / `spot:eth_usd` → spot CLOB (`PlaceOrder`)
    ///   `perp:N`                        → perpetual book (`PlacePerpOrder`)
    ///   `future:N`                      → dated future book (`PlacePerpOrder`)
    ///
    /// Spot prices use the same 1e8-scaled u128 semantics as perp. The
    /// executor enforces `price % MarketConfig.tick_size == 0`. `reduce-only`
    /// is rejected at parse time for `spot:` markets.
    Place {
        /// Market reference (`spot:`, `perp:`, or `future:`).
        market: String,
        /// `buy` or `sell`.
        side: String,
        /// Quantity in base units (u128 decimal).
        qty: u128,
        /// Limit price as a u128 decimal. 1e8-scaled for both spot and perp.
        price: u128,
        /// One of `gtc` (default) | `ioc` | `fok` | `post-only` | `reduce-only`.
        /// `reduce-only` only valid for `perp:` / `future:`.
        #[arg(num_args = 0..)]
        flags: Vec<String>,
    },
    /// Sign + submit a cancel for a resting spot or perp order.
    ///
    /// `<kind>` selects the chain CF the order lives in:
    ///   `spot` → `CancelOrder`
    ///   `perp` → `CancelPerpOrder` (covers both perpetuals and dated futures)
    Cancel {
        /// `spot` | `perp`.
        kind: String,
        /// Order id, hex-encoded (with or without `0x`).
        order_id: String,
    },
    /// Mint test ULD into the current wallet by signing a `Transfer` with the
    /// faucet mnemonic (`UL_FAUCET_MNEMONIC`, falling back to
    /// `MYWALLET3_MNEMONIC` from `scripts/perps_env.sh`).
    Faucet {
        /// Amount in raw ULD (u128 decimal). Default = 1_000_000_000_000.
        amount: Option<u128>,
    },
    /// (deprecated — use `place`) Sign + submit a `PlacePerpOrder`.
    PerpPlace {
        market: String,
        side: String,
        qty: u128,
        price: u128,
        #[arg(num_args = 0..)]
        flags: Vec<String>,
    },
    /// (deprecated — use `cancel perp`) Sign + submit a `CancelPerpOrder`.
    PerpCancel { order_id: String },
    /// Sign + submit a `LiquidationTrigger` for `(owner, market)`.
    Liquidate {
        /// Owner address whose position to attempt to liquidate.
        addr: String,
        /// Market reference: `perp:N`.
        market: String,
    },
    /// Sign + submit an `OracleUpdate` for a market. The current wallet MUST
    /// be the market's `oracle_authority` (set in genesis); the executor
    /// rejects any other sender with `notAuthorized`.
    Oracle {
        /// Market reference: `perp:N` or `future:N`.
        market: String,
        /// Mark price as a 1e8-scaled u128 decimal. Example: $50_000.00 BTC
        /// is 5_000_000_000_000.
        price: u128,
        /// Source height tag — must be >= the previously-posted height for
        /// this market or the executor returns `oracleStale`. Use `--height
        /// $(curl ... head_height)` to follow the chain head; in scripts a
        /// monotonic counter (1, 2, 3, ...) also works.
        #[arg(long)]
        height: u64,
    },
    /// Print help; with `[cmd]`, print help for that subcommand.
    Help { cmd: Option<String> },
    /// Exit the REPL. (Same as `exit`.)
    #[command(alias = "exit")]
    Quit,
}

/// Parse a single REPL line into a `Cli`. Returns `Ok(None)` for an empty or
/// whitespace-only line. The REPL passes `argv[0]` of `"cb-client"` to keep
/// clap happy.
pub fn parse_repl_line(line: &str) -> Result<Option<Cli>, clap::Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let mut argv: Vec<&str> = vec!["cb-client"];
    argv.extend(trimmed.split_whitespace());
    Cli::try_parse_from(argv).map(Some)
}
