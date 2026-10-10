//! The artefacts the repository actually ships, read from disk at test time.
//!
//! The distinction this module exists to enforce: a test that builds its own
//! genesis file and its own environment proves that the *node* works. It says
//! nothing about whether the files a person gets when they clone the repository
//! work together. Two of the five faults this suite was written for were
//! exactly that — a treasury address in genesis whose private key was in no
//! environment file, and a genesis default that referenced a variable nothing
//! set. So everything here reads the shipped file, never a fixture.
//!
//! `.env.example` is the shipped environment; `.env` is the developer's copy of
//! it and may have been edited. Checks about what the product promises read
//! `.env.example`. The running stack prefers `.env` when it exists, because
//! that is what the developer's own `make up` would use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The repository root, found by walking up from this crate.
///
/// `CARGO_MANIFEST_DIR` is `<repo>/rust/crates/cb-e2e`, and it is set by cargo
/// for every build of this crate, so this needs no search and cannot pick up a
/// different checkout.
pub fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(3)
        .expect("CARGO_MANIFEST_DIR is <repo>/rust/crates/cb-e2e")
        .to_path_buf()
}

/// `<repo>/rust`, where the genesis files, the Makefile and `.env` live.
pub fn rust_dir() -> PathBuf {
    repo_root().join("rust")
}

/// Reads a shipped file, failing with the path rather than with `No such file`.
pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read shipped file {}: {err}", path.display()))
}

/// The shipped environment template, `rust/.env.example`.
pub fn env_example_path() -> PathBuf {
    rust_dir().join(".env.example")
}

/// Parses an environment file with the **product's own** parser.
///
/// Using `cb_genesis::parse_dotenv` rather than a second implementation is
/// deliberate: the shipped parser does not strip a trailing `# comment` from a
/// value, and a test with its own lenient parser would happily accept a file
/// the node rejects. The point of these checks is to agree with the node.
pub fn parse_env(path: &Path) -> BTreeMap<String, String> {
    let text = read(path);
    cb_genesis::parse_dotenv(&text)
        .unwrap_or_else(|err| panic!("{} is not parseable by cb-genesis: {err}", path.display()))
        .into_iter()
        .collect()
}

/// The shipped environment as a map.
pub fn shipped_env() -> BTreeMap<String, String> {
    parse_env(&env_example_path())
}

/// The environment a running stack should inherit: the developer's `.env` when
/// they have one, otherwise the shipped template.
///
/// A stack booted from `.env.example` is the interesting case for a fresh
/// clone, and it is what continuous integration gets.
pub fn stack_env() -> BTreeMap<String, String> {
    let dotenv = rust_dir().join(".env");
    if dotenv.exists() {
        parse_env(&dotenv)
    } else {
        shipped_env()
    }
}

/// One line of a genesis file, with comments and blanks already dropped.
pub struct GenesisLine {
    /// 1-based line number in the file, for a failure message that points at it.
    pub number: usize,
    /// The raw text, before `${VAR}` substitution.
    pub text: String,
}

impl GenesisLine {
    /// The `kind` field, read textually so this works before substitution.
    pub fn kind(&self) -> Option<&str> {
        let rest = self.text.split_once("\"kind\":\"")?.1;
        rest.split_once('"').map(|(kind, _)| kind)
    }
}

/// The record lines of a genesis file.
///
/// Comment lines are dropped **here** rather than by the caller, because the
/// header comments of `genesis-full.jsonl` document the substitution syntax and
/// therefore contain a literal `${VAR}`. A naive scan for `${...}` over the
/// whole file reports a missing variable called `VAR` and sends whoever reads
/// the failure looking for a variable that does not exist.
pub fn genesis_lines(path: &Path) -> Vec<GenesisLine> {
    read(path)
        .lines()
        .enumerate()
        .filter_map(|(index, raw)| {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            Some(GenesisLine {
                number: index + 1,
                text: line.to_string(),
            })
        })
        .collect()
}

/// Every `${VAR}` referenced by a genesis record line, with its line number.
pub fn referenced_variables(path: &Path) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for line in genesis_lines(path) {
        let mut rest = line.text.as_str();
        while let Some(start) = rest.find("${") {
            let after = &rest[start + 2..];
            match after.find('}') {
                Some(end) => {
                    found.push((line.number, after[..end].to_string()));
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
    }
    found
}

/// The genesis file `make init` applies, read out of `rust/Makefile`.
///
/// This is read rather than hardcoded on purpose. Fault 5 was a default that
/// pointed at a genesis file without the advertised markets; a test that named
/// `genesis-full.jsonl` itself would have passed throughout, because the file
/// was fine and the default was not.
pub fn default_genesis_path() -> PathBuf {
    let makefile = rust_dir().join("Makefile");
    let text = read(&makefile);
    let name = text
        .lines()
        .find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("GENESIS")?.trim_start();
            let rest = rest.strip_prefix("?=").or_else(|| rest.strip_prefix('='))?;
            Some(rest.trim().to_string())
        })
        .unwrap_or_else(|| {
            panic!(
                "no `GENESIS ?= ...` assignment in {} — the default genesis is what \
                 `make init` applies, and this suite has to know which file that is",
                makefile.display()
            )
        });
    rust_dir().join(name)
}

/// Where a released binary would be, honouring `CARGO_TARGET_DIR`.
fn target_dir() -> PathBuf {
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => rust_dir().join("target"),
    }
}

/// Locates a product binary, or explains exactly how to produce it.
///
/// Order: an explicit `CB_E2E_<NAME>_BIN` override (which `make e2e` sets, so
/// the suite never guesses and never silently tests a stale build), then
/// `target/release`, then `target/debug`.
pub fn binary(name: &str) -> PathBuf {
    let override_var = format!(
        "CB_E2E_{}_BIN",
        name.trim_start_matches("cb-").to_uppercase()
    );
    if let Some(path) = std::env::var_os(&override_var) {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "{override_var} points at {} which is not a file",
            path.display()
        );
        return path;
    }
    let target = target_dir();
    for profile in ["release", "debug"] {
        let candidate = target.join(profile).join(name);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "cannot find the `{name}` binary.\n\
         Looked in {release} and {debug}, and at ${override_var}.\n\
         \n\
         This suite drives the real binaries, so they have to exist. Build them:\n\
         \n    cargo build --release -p cb-node -p cb-gateway\n\n\
         or run the whole suite with `make e2e`, which builds first.",
        release = target.join("release").join(name).display(),
        debug = target.join("debug").join(name).display(),
    );
}
