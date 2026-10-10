//! Interactive input abstraction for `login`.
//!
//! `login` accepts `--label`, `--mnemonic`, `--index` as flags, but any
//! omitted value is read interactively. The mnemonic must NEVER appear in
//! shell history or `ps` output, so it's read with terminal echo OFF — same
//! UX as `sudo` or `git commit --amend` opening the editor for a password.
//! We use `rpassword`'s silent read for that channel.
//!
//! This module owns that I/O so the rest of the client doesn't reach into
//! `rpassword` directly. Tests use [`ScriptedPrompts`] to inject canned
//! answers without needing a real tty.

use anyhow::{Context, Result};

/// Provider of interactive prompts. Two channels:
///
/// - `read_line` — visible stdin, used for label and index.
/// - `read_password` — terminal echo OFF, used for mnemonic.
///
/// Both methods take the prompt string the user sees so the implementation
/// can flush and lay it out consistently.
pub trait Prompts {
    fn read_line(&mut self, prompt: &str) -> Result<String>;
    fn read_password(&mut self, prompt: &str) -> Result<String>;
}

/// Production prompts: writes to stdout, reads from stdin / `/dev/tty`.
/// `read_password` uses `rpassword`, which switches the terminal into
/// no-echo mode for the duration of the read.
pub struct StdPrompts;

impl Prompts for StdPrompts {
    fn read_line(&mut self, prompt: &str) -> Result<String> {
        use std::io::{stdout, BufRead, Write};
        print!("{prompt}");
        stdout().flush().context("flush prompt")?;
        let mut s = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut s)
            .context("read line from stdin")?;
        // Strip the trailing newline (and a Windows-style CR if present).
        let s = s.trim_end_matches(['\r', '\n']).to_string();
        Ok(s)
    }

    fn read_password(&mut self, prompt: &str) -> Result<String> {
        use std::io::Write;
        // Write the prompt via stdout (visible) and use rpassword only for the
        // silent read. rpassword's own prompt-write goes to stderr/tty in a
        // way that has been observed to drop on macOS, so we own the prompt.
        print!("{prompt}");
        std::io::stdout().flush().context("flush password prompt")?;
        rpassword::read_password().context("read password from tty")
    }
}

/// Test-friendly prompts that pop pre-canned answers from in-memory queues.
/// Public so integration tests can construct one without feature flags.
///
/// Each call to `read_line`/`read_password` pops the next response from the
/// matching queue. Calls also append the prompt string to `prompt_log` so
/// tests can assert the user sees the right cues. Running out of canned
/// answers is an error (caller didn't pre-load enough responses).
pub struct ScriptedPrompts {
    pub lines: std::collections::VecDeque<String>,
    pub passwords: std::collections::VecDeque<String>,
    pub prompt_log: Vec<String>,
}

impl ScriptedPrompts {
    pub fn new() -> Self {
        Self {
            lines: std::collections::VecDeque::new(),
            passwords: std::collections::VecDeque::new(),
            prompt_log: Vec::new(),
        }
    }

    pub fn with_lines<I: IntoIterator<Item = String>>(mut self, items: I) -> Self {
        self.lines.extend(items);
        self
    }

    pub fn with_passwords<I: IntoIterator<Item = String>>(mut self, items: I) -> Self {
        self.passwords.extend(items);
        self
    }
}

impl Default for ScriptedPrompts {
    fn default() -> Self {
        Self::new()
    }
}

impl Prompts for ScriptedPrompts {
    fn read_line(&mut self, prompt: &str) -> Result<String> {
        self.prompt_log.push(prompt.to_string());
        self.lines.pop_front().ok_or_else(|| {
            anyhow::anyhow!("ScriptedPrompts: no more `lines` for prompt {prompt:?}")
        })
    }

    fn read_password(&mut self, prompt: &str) -> Result<String> {
        self.prompt_log.push(prompt.to_string());
        self.passwords.pop_front().ok_or_else(|| {
            anyhow::anyhow!("ScriptedPrompts: no more `passwords` for prompt {prompt:?}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_pops_in_order_and_records_prompts() {
        let mut p = ScriptedPrompts::new()
            .with_lines(vec!["alice".into(), "0".into()])
            .with_passwords(vec!["secret phrase".into()]);
        assert_eq!(p.read_line("Label: ").unwrap(), "alice");
        assert_eq!(p.read_password("Mnemonic: ").unwrap(), "secret phrase");
        assert_eq!(p.read_line("Index [0]: ").unwrap(), "0");
        assert_eq!(p.prompt_log, vec!["Label: ", "Mnemonic: ", "Index [0]: "]);
    }

    #[test]
    fn scripted_errors_when_queue_empty() {
        let mut p = ScriptedPrompts::new();
        let err = p.read_line("X").unwrap_err();
        assert!(err.to_string().contains("no more `lines`"));
        let err = p.read_password("Y").unwrap_err();
        assert!(err.to_string().contains("no more `passwords`"));
    }
}
