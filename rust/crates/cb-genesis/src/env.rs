//! Minimal `.env` parser for genesis-time env-var substitution.
//!
//! No external `dotenv` crate — the format we accept is intentionally small:
//!
//! - Each non-blank, non-`#` line is `KEY=VALUE` (an optional leading
//!   `export ` is stripped, so the same file can be used with shell
//!   `source .env.example`).
//! - Whitespace around `KEY` and `VALUE` is trimmed.
//! - `VALUE` may be wrapped in matching `"…"` or `'…'`; the quotes are stripped
//!   verbatim (no escape processing inside).
//! - No multiline strings, no shell expansion.
//!
//! Real env vars take precedence: a key already present in the process
//! environment is never overwritten. This means the `.env` file is a
//! convenience for operators (commit-friendly defaults), not a security
//! boundary — anything in the real shell wins.

use anyhow::{anyhow, Context, Result};
use std::path::Path;

/// Parse a `.env` blob into `(key, value)` pairs in source order. Errors on
/// malformed lines so config typos are loud, not silent.
pub fn parse_dotenv(text: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Strip optional `export ` prefix so `source .env` works in shells too.
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let (k, v) = line.split_once('=').ok_or_else(|| {
            anyhow!(
                ".env line {}: expected `KEY=VALUE`, got `{}`",
                lineno + 1,
                line
            )
        })?;
        let key = k.trim().to_string();
        if key.is_empty() {
            return Err(anyhow!(".env line {}: empty key", lineno + 1));
        }
        // Reject keys that aren't shell-identifier-shaped — caught early so
        // the user sees a clear error, not a confusing downstream failure.
        if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(anyhow!(
                ".env line {}: key `{key}` must be ASCII alphanumeric + `_`",
                lineno + 1
            ));
        }
        let value = strip_quotes(v.trim()).to_string();
        out.push((key, value));
    }
    Ok(out)
}

fn strip_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if first == last && (first == b'"' || first == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Read a `.env` file at `path` and apply each `(key, value)` to the process
/// environment via `std::env::set_var` — but **only** when the key is not
/// already set. Returns `Ok(0)` (no-op) when `path` does not exist.
///
/// Caller must invoke this before any code that reads the env vars (i.e.
/// before `expand_env_vars`). It is the caller's responsibility to ensure
/// no other thread is reading or writing the environment concurrently.
pub fn apply_dotenv(path: &Path) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read .env file at {}", path.display()))?;
    let pairs = parse_dotenv(&text)?;
    let mut applied = 0;
    for (k, v) in pairs {
        if std::env::var_os(&k).is_none() {
            // SAFETY: tests + production both call this before spawning
            // threads that observe the env. See module docs.
            std::env::set_var(&k, &v);
            applied += 1;
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_simple_key_value() {
        let pairs = parse_dotenv("FOO=bar\nBAZ=qux\n").unwrap();
        assert_eq!(
            pairs,
            vec![("FOO".into(), "bar".into()), ("BAZ".into(), "qux".into())]
        );
    }

    #[test]
    fn strips_export_prefix() {
        // Same file works with shell `source .env` *and* our parser.
        let pairs = parse_dotenv("export FOO=bar\n  export   BAZ=qux\n").unwrap();
        assert_eq!(
            pairs,
            vec![("FOO".into(), "bar".into()), ("BAZ".into(), "qux".into())]
        );
    }

    #[test]
    fn skips_blanks_and_comments() {
        let s = "
# top comment
FOO=1

# another
BAR=two
";
        let pairs = parse_dotenv(s).unwrap();
        assert_eq!(
            pairs,
            vec![("FOO".into(), "1".into()), ("BAR".into(), "two".into())]
        );
    }

    #[test]
    fn strips_matching_quotes() {
        let pairs = parse_dotenv(
            r#"A="hello world"
B='single quoted'
C=unquoted has spaces
"#,
        )
        .unwrap();
        assert_eq!(pairs[0], ("A".into(), "hello world".into()));
        assert_eq!(pairs[1], ("B".into(), "single quoted".into()));
        // Unquoted: leading whitespace already trimmed by `trim()`; embedded
        // spaces are preserved verbatim.
        assert_eq!(pairs[2], ("C".into(), "unquoted has spaces".into()));
    }

    #[test]
    fn mismatched_quotes_are_kept_verbatim() {
        let pairs = parse_dotenv(r#"A="oops'"#).unwrap();
        assert_eq!(pairs[0].1, r#""oops'"#);
    }

    #[test]
    fn malformed_line_errors() {
        let err = parse_dotenv("no_equals_sign_here\n").unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn empty_key_errors() {
        let err = parse_dotenv("=value\n").unwrap_err();
        assert!(err.to_string().contains("empty key"));
    }

    #[test]
    fn invalid_key_chars_error() {
        let err = parse_dotenv("MY-KEY=1\n").unwrap_err();
        assert!(err.to_string().contains("ASCII alphanumeric"));
    }

    #[test]
    fn apply_dotenv_does_not_clobber_real_env() {
        // Pre-set a var; the .env attempt to set it must be a no-op.
        std::env::set_var("UL_DOTENV_TEST_REAL", "from_real_env");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "UL_DOTENV_TEST_REAL=from_dotenv").unwrap();
        writeln!(f, "UL_DOTENV_TEST_FRESH=from_dotenv").unwrap();
        let applied = apply_dotenv(&path).unwrap();
        assert_eq!(applied, 1, "only the unset var gets applied");
        assert_eq!(
            std::env::var("UL_DOTENV_TEST_REAL").unwrap(),
            "from_real_env"
        );
        assert_eq!(
            std::env::var("UL_DOTENV_TEST_FRESH").unwrap(),
            "from_dotenv"
        );
    }

    #[test]
    fn apply_dotenv_missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env"); // does not exist
        let n = apply_dotenv(&path).unwrap();
        assert_eq!(n, 0);
    }
}
