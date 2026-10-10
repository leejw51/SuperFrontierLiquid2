//! On-disk persistence under `~/.cb_client/`.
//!
//! Layout:
//!   * `wallets.jsonl` — one JSON object per line (`WalletEntry`).
//!   * `session.json`  — single-line JSON object (`Session`). The single-line
//!     form keeps it jsonl-compatible if we ever decide to merge.
//!
//! Permissions on Unix are tightened to 0700 / 0600 so a stray group-readable
//! umask doesn't leak the privkey-bearing wallets file. We do this best-effort
//! on each write so the bits stay correct even if a third party loosens them.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::wallet::WalletEntry;

const WALLETS_FILE: &str = "wallets.jsonl";
const SESSION_FILE: &str = "session.json";
const DEFAULT_RPC: &str = "127.0.0.1:7700";
const DEFAULT_CHAIN_ID: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub rpc_addr: String,
    pub chain_id: u32,
    pub current_wallet: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            rpc_addr: DEFAULT_RPC.to_string(),
            chain_id: DEFAULT_CHAIN_ID,
            current_wallet: None,
        }
    }
}

impl Session {
    pub fn rpc_socket(&self) -> Result<SocketAddr> {
        self.rpc_addr
            .parse()
            .with_context(|| format!("invalid rpc_addr in session: {}", self.rpc_addr))
    }
}

/// File-system handle. All reads parse from disk every call so the REPL
/// always reflects an external `set-rpc` etc. (no in-memory cache).
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (and lazily create) the store rooted at `dir`.
    pub fn open(dir: PathBuf) -> Result<Self> {
        ensure_dir(&dir)?;
        Ok(Store { root: dir })
    }

    /// Default location: `$HOME/.cb_client`.
    pub fn open_default() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("could not resolve $HOME"))?;
        Self::open(home.join(".cb_client"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wallets_path(&self) -> PathBuf {
        self.root.join(WALLETS_FILE)
    }

    pub fn session_path(&self) -> PathBuf {
        self.root.join(SESSION_FILE)
    }

    /// Read every wallet entry, in insertion order. Missing file yields
    /// an empty Vec (first run).
    pub fn list_wallets(&self) -> Result<Vec<WalletEntry>> {
        let path = self.wallets_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mut out = Vec::new();
        for (i, line) in BufReader::new(f).lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), i + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: WalletEntry = serde_json::from_str(&line)
                .with_context(|| format!("parse wallet entry at line {}", i + 1))?;
            out.push(entry);
        }
        Ok(out)
    }

    pub fn find_wallet(&self, label: &str) -> Result<Option<WalletEntry>> {
        Ok(self.list_wallets()?.into_iter().find(|w| w.label == label))
    }

    /// Append a wallet. Refuses to add an entry whose label already exists
    /// unless `force` — in that case the file is rewritten with the new
    /// entry replacing the old one (insertion order otherwise preserved).
    pub fn upsert_wallet(&self, entry: WalletEntry, force: bool) -> Result<()> {
        let mut existing = self.list_wallets()?;
        if let Some(pos) = existing.iter().position(|w| w.label == entry.label) {
            if !force {
                return Err(anyhow!(
                    "wallet with label '{}' already exists (use --force to overwrite)",
                    entry.label
                ));
            }
            existing[pos] = entry;
            self.rewrite_wallets(&existing)?;
            return Ok(());
        }
        self.append_wallet(&entry)
    }

    fn append_wallet(&self, entry: &WalletEntry) -> Result<()> {
        let path = self.wallets_path();
        let mut opts = OpenOptions::new();
        opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&path)
            .with_context(|| format!("open {} for append", path.display()))?;
        let mut s = serde_json::to_string(entry).context("encode wallet entry")?;
        s.push('\n');
        f.write_all(s.as_bytes())
            .with_context(|| format!("append to {}", path.display()))?;
        tighten_file_mode(&path)?;
        Ok(())
    }

    fn rewrite_wallets(&self, entries: &[WalletEntry]) -> Result<()> {
        let path = self.wallets_path();
        let tmp = path.with_extension("jsonl.tmp");
        {
            let mut opts = OpenOptions::new();
            opts.create(true).write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts
                .open(&tmp)
                .with_context(|| format!("open {} for rewrite", tmp.display()))?;
            for e in entries {
                let mut s = serde_json::to_string(e).context("encode wallet entry")?;
                s.push('\n');
                f.write_all(s.as_bytes())
                    .with_context(|| format!("write to {}", tmp.display()))?;
            }
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        tighten_file_mode(&path)?;
        Ok(())
    }

    pub fn load_session(&self) -> Result<Session> {
        let path = self.session_path();
        if !path.exists() {
            return Ok(Session::default());
        }
        let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if raw.trim().is_empty() {
            return Ok(Session::default());
        }
        serde_json::from_str(&raw).with_context(|| format!("parse session file {}", path.display()))
    }

    pub fn save_session(&self, s: &Session) -> Result<()> {
        let path = self.session_path();
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string(s).context("encode session")?;
        {
            let mut opts = OpenOptions::new();
            opts.create(true).write(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts
                .open(&tmp)
                .with_context(|| format!("open {} for write", tmp.display()))?;
            f.write_all(body.as_bytes())
                .with_context(|| format!("write to {}", tmp.display()))?;
        }
        fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
        tighten_file_mode(&path)?;
        Ok(())
    }
}

fn ensure_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = fs::Permissions::from_mode(0o700);
        fs::set_permissions(dir, perm).with_context(|| format!("chmod 0700 {}", dir.display()))?;
    }
    Ok(())
}

fn tighten_file_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, perm)
            .with_context(|| format!("chmod 0600 {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path; // pacify compiler on non-unix
    }
    Ok(())
}
