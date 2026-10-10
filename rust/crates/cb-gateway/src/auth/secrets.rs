//! Session tokens, and why they are hashed with SHA-256 rather than Argon2.
//!
//! A session token is machine generated with 256 bits from the OS CSPRNG.
//! There is nothing to guess, so a memory-hard hash would buy no security and
//! would put roughly a hundred milliseconds of Argon2 on **every
//! authenticated request**. SHA-256 is the right primitive for hashing
//! something that is already unguessable.
//!
//! Argon2 still earns its place in this crate — [`crate::auth::vault`] uses it
//! to stretch a configured secret, which *is* guessable — but not here.
//!
//! What is persisted is the hash. The plaintext exists in transit and in the
//! client's hands, never in the database.

use rand::RngCore;
use subtle::ConstantTimeEq;

/// Bytes of entropy in a session token.
const TOKEN_BYTES: usize = 32;

/// The shortest server secret the gateway will start with.
///
/// Every account's trading key is `HKDF-SHA256(server_secret, login_address)`
/// and login addresses are public, so the secret is the only thing standing
/// between a stranger and every user's on-chain key. Thirty-two bytes is the
/// output width of the hash underneath it; less than that is a guessable
/// passphrase wearing a key's job.
pub const SERVER_SECRET_MIN_BYTES: usize = 32;

/// Secrets that have been published and can therefore never be secret again.
///
/// The first entry shipped in the git-tracked `rust/.env.example` and was
/// copied verbatim into every developer's `.env`, so anyone with a clone of
/// the repository could derive any user's trading key from their public login
/// address. Refusing it by exact match is cheap and stops a deployment
/// inheriting it silently. The second is the placeholder that replaced it,
/// which exists precisely to be refused.
const PUBLISHED_SECRETS: &[&str] = &[
    "local-development-secret-do-not-reuse",
    "replace-me-run-make-env",
];

/// How good a configured server secret looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretStrength {
    /// Nothing to complain about.
    Strong,
    /// Usable, but it does not look machine-generated. The reason is meant to
    /// be logged.
    Weak(&'static str),
}

/// Why a server secret cannot be used at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerSecretError {
    /// No secret at all.
    #[error(
        "the server secret must not be empty; every deployment would otherwise derive the \
         same trading accounts"
    )]
    Empty,
    /// Shorter than [`SERVER_SECRET_MIN_BYTES`].
    #[error(
        "the server secret is {0} bytes; at least {SERVER_SECRET_MIN_BYTES} are required. \
         Every trading key is HKDF(server_secret, login_address) and login addresses are \
         public, so a guessable secret hands over every account. Generate one with \
         `openssl rand -hex 32`, or run `make env` which writes a fresh one per machine"
    )]
    TooShort(usize),
    /// A value that is published in the repository's history.
    #[error(
        "the server secret is a published placeholder value. It is in the repository, so it is \
         known to everyone, and anyone holding it can derive every user's trading key from \
         their public login address. Generate a real one with `openssl rand -hex 32`, or run \
         `make env`"
    )]
    Published,
}

/// Check a configured server secret before anything is derived from it.
///
/// Returns the strength so the caller can warn about a secret that is legal
/// but unimpressive; returns an error for the three cases that must stop the
/// process.
pub fn check_server_secret(secret: &[u8]) -> Result<SecretStrength, ServerSecretError> {
    if secret.is_empty() {
        return Err(ServerSecretError::Empty);
    }
    if PUBLISHED_SECRETS
        .iter()
        .any(|known| known.as_bytes() == secret)
    {
        return Err(ServerSecretError::Published);
    }
    if secret.len() < SERVER_SECRET_MIN_BYTES {
        return Err(ServerSecretError::TooShort(secret.len()));
    }

    // Not an entropy estimate — there is no honest way to measure the entropy
    // of a value you have only one sample of. These are two cheap shapes that
    // a generated secret almost never has and a hand-typed one usually does.
    let mut seen = [false; 256];
    let mut distinct = 0usize;
    for byte in secret {
        if !seen[*byte as usize] {
            seen[*byte as usize] = true;
            distinct += 1;
        }
    }
    if distinct < 12 {
        return Ok(SecretStrength::Weak(
            "it uses fewer than 12 distinct byte values",
        ));
    }
    if secret
        .iter()
        .all(|b| b.is_ascii_lowercase() || *b == b'-' || *b == b'_' || *b == b' ')
    {
        return Ok(SecretStrength::Weak(
            "it looks like a hand-typed phrase rather than a generated key",
        ));
    }
    Ok(SecretStrength::Strong)
}

/// Mint a session token: `sess_` followed by 256 bits of hex.
///
/// The prefix is for humans reading logs; the entropy is all after it.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("sess_{}", hex::encode(bytes))
}

/// Hash a session token for storage and lookup.
pub fn hash_session_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex::encode(hasher.finalize())
}

/// Whether a presented token matches a stored hash, in constant time.
///
/// The lookup is already by hash, so a timing signal here would be hard to
/// exploit — but "hard to exploit" is not a property worth relying on when the
/// alternative is one function call.
pub fn session_token_matches(token: &str, stored_hash: &str) -> bool {
    let computed = hash_session_token(token);
    computed.as_bytes().ct_eq(stored_hash.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret shaped like one `make env` would write.
    fn generated() -> &'static str {
        "9f3c1b7d5e2a40886c19fd734ab05e61c8d2740f9ab31e55d06c28f41b7e93aa"
    }

    #[test]
    fn the_secret_that_shipped_in_the_repository_is_refused_by_name() {
        // Finding 2: this exact string was committed in rust/.env.example and
        // copied into every .env, so it secures nothing. It is long enough to
        // clear the length floor, which is why the blocklist exists.
        let published = b"local-development-secret-do-not-reuse";
        assert!(published.len() >= SERVER_SECRET_MIN_BYTES);
        assert_eq!(
            check_server_secret(published),
            Err(ServerSecretError::Published)
        );
        assert_eq!(
            check_server_secret(b"replace-me-run-make-env"),
            Err(ServerSecretError::Published),
            "the placeholder that replaced it must be refused too"
        );
    }

    #[test]
    fn a_short_or_empty_secret_is_refused() {
        assert_eq!(check_server_secret(b""), Err(ServerSecretError::Empty));
        assert_eq!(
            check_server_secret(b"hunter2"),
            Err(ServerSecretError::TooShort(7))
        );
        let just_under = vec![b'x'; SERVER_SECRET_MIN_BYTES - 1];
        assert!(matches!(
            check_server_secret(&just_under),
            Err(ServerSecretError::TooShort(_))
        ));
    }

    #[test]
    fn a_generated_secret_is_accepted_without_complaint() {
        assert_eq!(
            check_server_secret(generated().as_bytes()),
            Ok(SecretStrength::Strong)
        );
    }

    #[test]
    fn a_long_but_unimpressive_secret_is_allowed_with_a_warning() {
        // Long enough to be legal, obviously not generated. The gateway starts
        // and says so rather than refusing: a length floor is a rule, and
        // "this looks hand-typed" is a judgement.
        let repeated = vec![b'a'; 64];
        assert!(matches!(
            check_server_secret(&repeated),
            Ok(SecretStrength::Weak(_))
        ));
        assert!(matches!(
            check_server_secret(b"correct-horse-battery-staple-and-then-some-more"),
            Ok(SecretStrength::Weak(_))
        ));
    }

    #[test]
    fn the_error_messages_say_how_to_fix_it() {
        for error in [
            ServerSecretError::Published,
            ServerSecretError::TooShort(4),
            ServerSecretError::Empty,
        ] {
            let text = error.to_string();
            assert!(!text.is_empty());
        }
        assert!(ServerSecretError::Published
            .to_string()
            .contains("make env"));
        assert!(ServerSecretError::TooShort(4)
            .to_string()
            .contains("openssl rand"));
    }

    #[test]
    fn tokens_are_unique_and_prefixed() {
        let a = generate_session_token();
        let b = generate_session_token();
        assert_ne!(a, b);
        assert!(a.starts_with("sess_"));
        assert_eq!(a.len(), "sess_".len() + TOKEN_BYTES * 2);
    }

    #[test]
    fn a_token_hash_is_stable_and_does_not_contain_the_token() {
        let token = generate_session_token();
        let hash = hash_session_token(&token);
        assert_eq!(hash, hash_session_token(&token));
        assert!(!hash.contains(token.trim_start_matches("sess_")));
        assert!(session_token_matches(&token, &hash));
        assert!(!session_token_matches("sess_deadbeef", &hash));
    }
}
