//! `cb-client` binary entry-point.
//!
//! With no subcommand, drops into the REPL. With a subcommand, runs it once
//! and exits — same code path as the REPL dispatcher in `repl::dispatch`.

use cb_client::{
    cli::{Cli, Command},
    prompts::StdPrompts,
    repl, Store,
};
use clap::Parser;

fn main() -> anyhow::Result<()> {
    // Tracing init is intentionally minimal: we never log secret material,
    // and the REPL must never have stray log lines interleave with `ul> `.
    let _ = tracing_subscriber_init();

    let cli = Cli::parse();
    let store = Store::open_default()?;

    match cli.cmd.clone() {
        Some(Command::Quit) => Ok(()),
        Some(cmd) => {
            // One-shot: still need real-tty prompts so an interactive
            // `cb-client login` (no flags) prompts the operator for the
            // mnemonic with terminal echo OFF.
            let mut prompts = StdPrompts;
            let text = repl::dispatch(&store, &mut prompts, cli.rpc, cli.chain_id, cmd)?;
            if !text.is_empty() {
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            Ok(())
        }
        None => repl::run(store),
    }
}

/// Best-effort tracing subscriber. Honours `RUST_LOG`; defaults to `warn`
/// because at `info` capnp-rpc chatter would interleave with the prompt.
/// We fall through silently if a subscriber is already installed.
fn tracing_subscriber_init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()?;
    Ok(())
}
