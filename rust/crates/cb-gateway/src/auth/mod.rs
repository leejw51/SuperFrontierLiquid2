//! Identity, sessions and the custodial signing key.
//!
//! ## Login proves control of an address
//!
//! Per `docs/api-contract.md` §2, authentication is **address plus
//! signature**, and the scheme is the ecosystem's: EIP-55 addresses, EIP-191
//! `personal_sign`, BIP-44 derivation at `m/44'/60'/0'/0/{index}`. The
//! mnemonic never reaches the server on the real path — the client signs a
//! server-issued nonce and the gateway recovers the signer.
//!
//! There is no registration step. A first successful login creates the
//! account, which is why the login request carries a `handle`.
//!
//! ## Two addresses
//!
//! Signing in proves control of an EVM address; it does not hand the gateway a
//! key, so the gateway cannot sign chain transactions as that address. Every
//! account therefore has two, and the contract makes the distinction visible:
//!
//! | | What it is | Who holds the key |
//! |---|---|---|
//! | `login_address` | the address the user signed in with | the user |
//! | `trading_address` | the on-chain account holding their test money | the gateway |
//!
//! The trading key is `HKDF-SHA256(server_secret, login_address)` — see
//! [`eth::derive_trading_key`]. **It is never stored**, because it can always
//! be recomputed, which is strictly better than encrypting it at rest: there
//! is no key material in the database to leak, and no key to lose. One login
//! address always maps to the same trading account.
//!
//! This is a deliberate simulator trade-off. The money is test money the
//! faucet mints, custodial signing is what makes a trade one click instead of
//! a wallet prompt, and a production system would have the chain accept orders
//! signed by the user's own key instead.
//!
//! ## Session semantics
//!
//! A session is an opaque bearer token; the database stores only its hash, so
//! a database dump does not yield working sessions. Expiry, revocation and
//! "never existed" are distinguished internally and deliberately collapsed to
//! one `401` at the HTTP boundary — telling an attacker which of the three
//! they hit is free information.

pub mod challenge;
pub mod eth;
pub mod rotation;
pub mod secrets;

pub use challenge::{Challenge, ChallengeStore};

/// The stored hash of a session token.
///
/// Published so the WebSocket hub can bind a connection to the session that
/// opened it without holding the token itself.
pub fn session_token_hash(token: &str) -> String {
    secrets::hash_session_token(token)
}

use crate::error::{ApiError, ApiResult, ErrorCode};
use cb_ports::{AppDatabase, Clock, PortError, Session, User};
use cb_types::Address;
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

/// Longest a handle may be.
const HANDLE_MAX: usize = 20;
/// Shortest a handle may be.
const HANDLE_MIN: usize = 3;

/// What the `recovery_hash` column holds for a signature-authenticated
/// account.
///
/// The port model's field was designed for a recovery-code scheme the contract
/// has since replaced. There is no stored credential to hash any more — the
/// credential is a signature the client produces — so the column records that
/// fact rather than a hash of nothing.
const SIGNATURE_AUTH_MARKER: &str = "eip191:no-stored-credential";

/// Why a session was refused.
///
/// Kept apart from the HTTP layer so the audit log can record *which* failure
/// happened even though the response says only `401`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFailure {
    /// No `Authorization` header, or one that is not a bearer token.
    Missing,
    /// A token no session was ever issued for.
    Unknown,
    /// A session past its expiry.
    Expired,
    /// A session revoked before it expired.
    Revoked,
    /// A session whose user has since been deleted.
    OrphanedSession,
}

impl AuthFailure {
    /// The message the browser sees. Deliberately uniform.
    pub fn message(self) -> &'static str {
        "missing or invalid session"
    }

    /// The reason recorded in logs and the audit trail.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unknown => "unknown",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::OrphanedSession => "orphaned",
        }
    }
}

impl From<AuthFailure> for ApiError {
    fn from(failure: AuthFailure) -> Self {
        ApiError::new(ErrorCode::Unauthorized, failure.message())
    }
}

/// The result of a successful login.
#[derive(Debug, Clone)]
pub struct Authenticated {
    /// The account.
    pub user: User,
    /// The address the user signed in with.
    pub login_address: Address,
    /// The on-chain account the gateway trades from on their behalf.
    pub trading_address: Address,
    /// The plaintext session token. Returned once, never stored.
    pub session_token: String,
    /// When the session was issued.
    pub issued_at: DateTime<Utc>,
    /// When it stops being accepted.
    pub expires_at: DateTime<Utc>,
    /// Whether this login created the account.
    pub is_new: bool,
}

/// A role the frontend uses to decide what to show.
///
/// Not a security boundary: every endpoint that cares enforces the role
/// itself, whatever the client believes.
pub const ROLE_USER: &str = "user";
/// The operator role.
pub const ROLE_ADMIN: &str = "admin";

/// Login, sessions and the derived trading key.
pub struct AuthService {
    db: Arc<dyn AppDatabase>,
    directory: Arc<crate::directory::Directory>,
    clock: Arc<dyn Clock>,
    challenges: ChallengeStore,
    session_ttl: Duration,
    /// Login addresses that hold the operator role.
    ///
    /// The role is bound to an address rather than to a handle because an
    /// address is cryptographically controlled and a handle is not: handles
    /// are self-chosen and first-come, so a handle-bound role belongs to
    /// whoever registers it first.
    admin_addresses: Vec<Address>,
    /// Handles that hold the operator role. Empty unless the operator passed
    /// `--insecure-admin-by-handle`; see [`Self::with_admin_handles`].
    admin_handles: Vec<String>,
    /// Serialises handle changes, so two renames cannot both pass the
    /// uniqueness check and both write.
    rename_lock: tokio::sync::Mutex<()>,
    /// Trading address to user id.
    ///
    /// Trading addresses are derived, not stored, so there is no column to
    /// look one up by — and the watcher sees a block full of *trading*
    /// addresses that it has to attribute to accounts. The index is built by
    /// deriving every user's trading address once and is rebuilt only when the
    /// user count changes, so the bot traffic that dominates a block (and
    /// belongs to no gateway account) costs a lookup miss rather than a rescan.
    trading_index: dashmap::DashMap<Address, String>,
    /// How many users the index was built from.
    indexed_users: std::sync::atomic::AtomicU64,
    /// The secret every trading key is derived from. Rotating it orphans every
    /// trading account, which is why it is configuration with no default.
    server_secret: Vec<u8>,
}

impl std::fmt::Debug for AuthService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthService")
            .field("session_ttl", &self.session_ttl)
            .field("outstanding_challenges", &self.challenges.len())
            .finish_non_exhaustive()
    }
}

impl AuthService {
    /// Build the service.
    ///
    /// `server_secret` is the material every trading key is derived from, and
    /// it is checked here rather than at the call site because this is the one
    /// place every deployment and every test passes through. A secret that is
    /// empty, shorter than
    /// [`secrets::SERVER_SECRET_MIN_BYTES`], or one of the values published in
    /// this repository is refused outright; one that is merely unimpressive is
    /// accepted with a warning.
    pub fn new(
        db: Arc<dyn AppDatabase>,
        directory: Arc<crate::directory::Directory>,
        clock: Arc<dyn Clock>,
        session_ttl: Duration,
        server_secret: impl AsRef<[u8]>,
    ) -> ApiResult<Self> {
        let server_secret = server_secret.as_ref().to_vec();
        match secrets::check_server_secret(&server_secret) {
            Ok(secrets::SecretStrength::Strong) => {}
            Ok(secrets::SecretStrength::Weak(why)) => {
                tracing::warn!(
                    target: "cb_gateway::auth",
                    reason = why,
                    "the configured server secret looks weak. Every account's trading key is \
                     HKDF(server_secret, login_address) and login addresses are public, so \
                     guessing this value yields every user's on-chain key. Generate one with \
                     `openssl rand -hex 32`."
                );
            }
            Err(e) => return Err(ApiError::internal(e.to_string())),
        }
        Ok(Self {
            db,
            directory,
            clock,
            // The contract's five minutes.
            challenges: ChallengeStore::new(Duration::minutes(5)),
            session_ttl,
            admin_addresses: Vec::new(),
            admin_handles: Vec::new(),
            rename_lock: tokio::sync::Mutex::new(()),
            trading_index: dashmap::DashMap::new(),
            indexed_users: std::sync::atomic::AtomicU64::new(u64::MAX),
            server_secret,
        })
    }

    /// Name the login addresses that hold the operator role.
    ///
    /// This is the supported operator binding: an address is owned by whoever
    /// holds its key, so it cannot be claimed by registering first.
    pub fn with_admin_addresses(mut self, addresses: Vec<Address>) -> Self {
        self.admin_addresses = addresses;
        self
    }

    /// Name the handles that hold the operator role.
    ///
    /// **This binding is unsafe and is kept only for the local workflow.** A
    /// handle is a self-chosen string: on a fresh store the first account to
    /// log in choosing it receives the operator role. The composition root
    /// refuses to pass anything here unless `--insecure-admin-by-handle` was
    /// given, and warns when it does.
    pub fn with_admin_handles(mut self, handles: Vec<String>) -> Self {
        self.admin_handles = handles
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        self
    }

    /// The account directory, for suspensions and session metadata.
    pub fn directory(&self) -> &crate::directory::Directory {
        &self.directory
    }

    /// The outstanding login challenges.
    ///
    /// Exposed so the single-use property can be asserted directly on the
    /// store, which is where it is enforced.
    pub fn challenges(&self) -> &ChallengeStore {
        &self.challenges
    }

    /// Validate a handle against the contract's rules.
    ///
    /// 3–20 characters of `[a-z0-9_]`, unique case-insensitively. Uppercase
    /// input is folded rather than rejected: a user who types `Satoshi` meant
    /// `satoshi`, and uniqueness ignores case anyway.
    pub fn normalize_handle(input: &str) -> ApiResult<String> {
        let handle = input.trim().to_ascii_lowercase();
        if handle.len() < HANDLE_MIN || handle.len() > HANDLE_MAX {
            return Err(ApiError::validation(format!(
                "handle must be {HANDLE_MIN}–{HANDLE_MAX} characters"
            )));
        }
        if !handle
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(ApiError::validation(
                "handle may contain only lowercase letters, digits and underscores",
            ));
        }
        Ok(handle)
    }

    /// `POST /auth/challenge`: issue a nonce for `address` to sign.
    ///
    /// Issuing does not evict an outstanding challenge for the same address:
    /// this endpoint takes no authentication and cannot, so a caller who names
    /// somebody else's address must not be able to invalidate what that person
    /// is mid-way through signing. See [`crate::auth::challenge`].
    pub fn challenge(&self, address: &Address) -> Challenge {
        self.challenges.issue(address, self.clock.now())
    }

    /// `POST /auth/login`: verify a signature over the outstanding challenge.
    ///
    /// A first successful login creates the account, so `handle` is required
    /// when there is no account yet and ignored when there is.
    pub async fn login(
        &self,
        address: &Address,
        signature: &str,
        handle: Option<&str>,
    ) -> ApiResult<Authenticated> {
        let now = self.clock.now();
        let live = self.challenges.live(address, now);
        if live.is_empty() {
            return Err(ApiError::new(
                ErrorCode::ChallengeExpired,
                "no outstanding challenge for that address; request one from \
                 POST /auth/challenge and sign it within five minutes",
            ));
        }

        // A signature over *any* live challenge logs in, because a second
        // `POST /auth/challenge` for this address may have been made by
        // somebody else entirely (see [`challenge`]). The matched one is then
        // retired, so the signature is still single-use; a signature that
        // matches nothing retires nothing, so a failed attempt — anyone's —
        // cannot knock a real login off its nonce.
        let mut matched = None;
        for challenge in &live {
            match eth::recover_personal_sign(&challenge.message, signature) {
                Ok(recovered) if recovered == *address => {
                    matched = Some(challenge.clone());
                    break;
                }
                // Recovery either failed or produced somebody else. Neither
                // says which, deliberately: a failed login is not an oracle.
                _ => continue,
            }
        }
        let Some(challenge) = matched else {
            return Err(ApiError::new(
                ErrorCode::BadSignature,
                "signature does not match the claimed address",
            ));
        };
        // Retiring is the atomic single-use point, so its result decides: two
        // concurrent logins presenting the same captured signature both see it
        // in `live()` and both verify, and exactly one of them removes it. The
        // loser is a replay and is refused.
        if !self.challenges.retire(address, &challenge.nonce) {
            return Err(ApiError::new(
                ErrorCode::ChallengeExpired,
                "that challenge has already been used; request another from \
                 POST /auth/challenge",
            ));
        }

        self.establish(address, handle, now).await
    }

    /// `POST /auth/dev-login`: derive a key from a mnemonic and log in.
    ///
    /// The gateway signs its own challenge with the derived key and discards
    /// it immediately, which makes this exactly the normal login with the
    /// client's half performed server-side. The mnemonic is not stored and the
    /// derived login key does not outlive this call — the *trading* key is
    /// derived from the login address like any other account's.
    pub async fn dev_login(
        &self,
        mnemonic: &str,
        index: u32,
        handle: Option<&str>,
    ) -> ApiResult<Authenticated> {
        let (secret, address) = eth::derive_from_mnemonic(mnemonic, index)
            .map_err(|e| ApiError::validation(e.to_string()))?;
        let now = self.clock.now();
        // Go through the same challenge and signature path the real login
        // uses, so the two cannot drift apart.
        let challenge = self.challenges.issue(&address, now);
        let signature = eth::sign_personal_sign(&secret, &challenge.message)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        let recovered = eth::recover_personal_sign(&challenge.message, &signature)
            .map_err(|e| ApiError::internal(e.to_string()))?;
        if recovered != address {
            return Err(ApiError::internal(
                "the gateway's own signature did not verify",
            ));
        }
        self.challenges.retire(&address, &challenge.nonce);
        // The login key has done its job. Nothing keeps it: it is a local
        // that goes out of scope here.
        let _ = secret;

        self.establish(&address, handle, now).await
    }

    /// Create or find the account and issue a session.
    async fn establish(
        &self,
        address: &Address,
        handle: Option<&str>,
        now: DateTime<Utc>,
    ) -> ApiResult<Authenticated> {
        let rendered = eth::checksum_address(address);
        let existing = self.db.users().by_address(&rendered).await?;

        let (user, is_new) = match existing {
            Some(user) => {
                self.db.users().touch_last_seen(&user.user_id, now).await?;
                (user, false)
            }
            None => {
                let handle = handle.ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::HandleRequired,
                        "a handle is required the first time an address signs in",
                    )
                })?;
                let handle = Self::normalize_handle(handle)?;
                let user = User {
                    user_id: new_user_id(),
                    handle: handle.clone(),
                    address: rendered,
                    recovery_hash: SIGNATURE_AUTH_MARKER.to_string(),
                    is_bot: false,
                    created_at: now,
                    last_seen_at: now,
                };
                match self.db.users().create(&user).await {
                    Ok(()) => {}
                    Err(PortError::Conflict(_)) => {
                        return Err(ApiError::new(
                            ErrorCode::HandleTaken,
                            format!("handle `{handle}` is already taken"),
                        ))
                    }
                    Err(e) => return Err(e.into()),
                }
                (user, true)
            }
        };

        // Suspension is checked at login as well as on every request, so a
        // suspended account cannot even obtain a fresh session to try with.
        if self.directory.is_suspended(&user.user_id) {
            return Err(ApiError::new(
                ErrorCode::AccountSuspended,
                "this account has been suspended by an operator",
            ));
        }

        let (_key, trading_address) = self.trading_key_for(address)?;
        self.trading_index
            .insert(trading_address, user.user_id.clone());
        let (token, expires_at) = self.issue_session(&user.user_id, now).await?;
        Ok(Authenticated {
            user,
            login_address: *address,
            trading_address,
            session_token: token,
            issued_at: now,
            expires_at,
            is_new,
        })
    }

    /// Derive the trading key and address for a login address.
    pub fn trading_key_for(
        &self,
        login_address: &Address,
    ) -> ApiResult<(cb_crypto::SecretKey, Address)> {
        eth::derive_trading_key(&self.server_secret, login_address)
            .map_err(|e| ApiError::internal(format!("derive trading key: {e}")))
    }

    /// The trading address for a login address.
    pub fn trading_address_for(&self, login_address: &Address) -> ApiResult<Address> {
        Ok(self.trading_key_for(login_address)?.1)
    }

    /// Resolve a trading address back to the account that owns it.
    ///
    /// Returns `None` for an address the gateway has no account for, which is
    /// the common case: bot traders submit to the sequencer directly and every
    /// block is full of addresses that belong to nobody here.
    pub async fn user_by_trading_address(&self, address: &Address) -> ApiResult<Option<User>> {
        if let Some(user_id) = self.trading_index.get(address).map(|e| e.value().clone()) {
            return Ok(self.db.users().by_id(&user_id).await?);
        }
        // A miss is only worth a rebuild when the account set has actually
        // grown; otherwise every bot transaction would trigger a full rescan.
        let count = self.db.users().count(true).await?;
        if self
            .indexed_users
            .load(std::sync::atomic::Ordering::Relaxed)
            == count
        {
            return Ok(None);
        }
        self.rebuild_trading_index(count).await?;
        match self.trading_index.get(address).map(|e| e.value().clone()) {
            Some(user_id) => Ok(self.db.users().by_id(&user_id).await?),
            None => Ok(None),
        }
    }

    async fn rebuild_trading_index(&self, count: u64) -> ApiResult<()> {
        for user in self.db.users().list(true, 1_000_000).await? {
            if let Ok((_key, trading)) = self.trading_key_of(&user) {
                self.trading_index.insert(trading, user.user_id.clone());
            }
        }
        self.indexed_users
            .store(count, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// The trading key for a stored user record.
    pub fn trading_key_of(&self, user: &User) -> ApiResult<(cb_crypto::SecretKey, Address)> {
        let login = eth::parse_checked_address(&user.address).map_err(|_| {
            ApiError::internal("the stored login address for this account is malformed")
        })?;
        self.trading_key_for(&login)
    }

    /// Revoke one session. Idempotent: logging out twice is not an error.
    pub async fn logout(&self, token: &str) -> ApiResult<()> {
        let hash = secrets::hash_session_token(token);
        match self.db.sessions().revoke(&hash).await {
            Ok(()) | Err(PortError::NotFound(_)) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve a bearer token to its user, or say why not.
    pub async fn authenticate(&self, token: &str) -> Result<User, AuthFailure> {
        if token.trim().is_empty() {
            return Err(AuthFailure::Missing);
        }
        let hash = secrets::hash_session_token(token);
        let session = match self.db.sessions().by_token_hash(&hash).await {
            Ok(Some(session)) => session,
            _ => return Err(AuthFailure::Unknown),
        };
        // The lookup was by hash, but compare explicitly so the check does not
        // depend on the storage layer's comparison being constant time.
        if !secrets::session_token_matches(token, &session.token_hash) {
            return Err(AuthFailure::Unknown);
        }
        if session.revoked {
            return Err(AuthFailure::Revoked);
        }
        if session.expires_at <= self.clock.now() {
            return Err(AuthFailure::Expired);
        }
        match self.db.users().by_id(&session.user_id).await {
            Ok(Some(user)) => Ok(user),
            _ => Err(AuthFailure::OrphanedSession),
        }
    }

    /// When a session was issued and when it expires.
    pub async fn session_window(
        &self,
        token: &str,
    ) -> ApiResult<Option<(DateTime<Utc>, DateTime<Utc>)>> {
        let hash = secrets::hash_session_token(token);
        Ok(self
            .db
            .sessions()
            .by_token_hash(&hash)
            .await?
            .map(|s| (s.created_at, s.expires_at)))
    }

    /// The roles a user holds.
    ///
    /// The operator role is granted on the account's **login address** first.
    /// The handle comparison below is reached only when the deployment opted
    /// into it explicitly, and it is the weaker test by construction: a handle
    /// can be chosen, an address has to be controlled.
    pub fn roles_of(&self, user: &User) -> Vec<String> {
        let mut roles = vec![ROLE_USER.to_string()];
        if self.is_admin_address(&user.address) || self.is_admin_handle(user) {
            roles.push(ROLE_ADMIN.to_string());
        }
        roles
    }

    /// Whether a stored login address is one of the configured operators.
    ///
    /// The stored form is parsed rather than string-compared so that a
    /// difference in checksum casing cannot decide who is an operator.
    fn is_admin_address(&self, stored: &str) -> bool {
        if self.admin_addresses.is_empty() {
            return false;
        }
        match eth::parse_checked_address(stored) {
            Ok(address) => self.admin_addresses.contains(&address),
            Err(_) => false,
        }
    }

    /// Whether a user's current handle is a configured operator handle.
    fn is_admin_handle(&self, user: &User) -> bool {
        if self.admin_handles.is_empty() {
            return false;
        }
        let handle = self.effective_handle(user);
        self.admin_handles.contains(&handle)
    }

    /// Whether a session, named by the hash it is stored under, may still be
    /// used.
    ///
    /// The same checks `authenticate` makes, minus the token comparison, so an
    /// open WebSocket can recheck the session it authenticated with long ago.
    /// A session is not a one-time gate: it expires, it can be revoked, and the
    /// account behind it can be suspended, all while a socket stays open.
    pub async fn session_is_live(&self, token_hash: &str) -> bool {
        let Ok(Some(session)) = self.db.sessions().by_token_hash(token_hash).await else {
            return false;
        };
        if session.revoked || session.expires_at <= self.clock.now() {
            return false;
        }
        // A suspension is deliberately NOT session death: the contract leaves a
        // suspended account's positions open, so it keeps the reads — and the
        // account channel those reads are mirrored on — that let the person see
        // the risk they are still carrying. What it loses is every mutating
        // request, which `crate::http::Caller` refuses.
        matches!(self.db.users().by_id(&session.user_id).await, Ok(Some(_)))
    }

    /// Whether a user holds the operator role.
    pub fn is_admin(&self, user: &User) -> bool {
        self.roles_of(user).iter().any(|r| r == ROLE_ADMIN)
    }

    /// Drop sessions and challenges that have expired.
    pub async fn purge_expired_sessions(&self) -> ApiResult<u64> {
        let now = self.clock.now();
        self.challenges.prune(now);
        Ok(self.db.sessions().delete_expired(now).await?)
    }

    async fn issue_session(
        &self,
        user_id: &str,
        now: DateTime<Utc>,
    ) -> ApiResult<(String, DateTime<Utc>)> {
        let token = secrets::generate_session_token();
        let expires_at = now + self.session_ttl;
        let token_hash = secrets::hash_session_token(&token);
        let session = Session {
            token_hash: token_hash.clone(),
            user_id: user_id.to_string(),
            created_at: now,
            expires_at,
            revoked: false,
        };
        self.db.sessions().create(&session).await?;

        // The metadata the user page shows. The request context — user agent,
        // client address — is attached by the HTTP layer once it has them.
        let meta = crate::directory::SessionMeta {
            session_id: crate::directory::Directory::session_id_for(&token_hash),
            token_hash,
            user_id: user_id.to_string(),
            issued_at: now,
            expires_at,
            last_used_at: now,
            user_agent: None,
            ip_prefix: None,
        };
        if let Err(e) = self.directory.record_session(&meta) {
            tracing::warn!(
                target: "cb_gateway::auth",
                error = %e,
                "session metadata not recorded; the session itself is still valid"
            );
        }
        Ok((token, expires_at))
    }

    /// Attach the request context to the session just issued.
    pub fn describe_session(
        &self,
        user_id: &str,
        token: &str,
        user_agent: Option<&str>,
        client_address: &str,
    ) {
        let token_hash = secrets::hash_session_token(token);
        let session_id = crate::directory::Directory::session_id_for(&token_hash);
        if let Ok(Some(mut meta)) = self.directory.session(user_id, &session_id) {
            meta.user_agent = user_agent.map(|agent| agent.chars().take(200).collect());
            meta.ip_prefix = crate::directory::Directory::truncate_address(client_address);
            let _ = self.directory.record_session(&meta);
        }
    }

    /// Revoke one of a user's sessions by its short id.
    ///
    /// Returns the revoked session's token hash, which is what the caller needs
    /// to close the sockets that session opened.
    pub async fn revoke_session(&self, user_id: &str, session_id: &str) -> ApiResult<String> {
        let meta = self
            .directory
            .session(user_id, session_id)?
            .ok_or_else(|| ApiError::not_found(format!("no session `{session_id}`")))?;
        match self.db.sessions().revoke(&meta.token_hash).await {
            Ok(()) | Err(PortError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
        self.directory.forget_session(user_id, session_id)?;
        Ok(meta.token_hash)
    }

    /// Revoke every session but the one making the request.
    pub async fn revoke_other_sessions(&self, user_id: &str, keep_token: &str) -> ApiResult<u64> {
        let keep =
            crate::directory::Directory::session_id_for(&secrets::hash_session_token(keep_token));
        let mut revoked = 0;
        for meta in self.directory.sessions_for(user_id)? {
            if meta.session_id == keep {
                continue;
            }
            self.revoke_session(user_id, &meta.session_id).await?;
            revoked += 1;
        }
        Ok(revoked)
    }

    /// The sessions recorded for a user.
    pub fn sessions_of(&self, user_id: &str) -> ApiResult<Vec<crate::directory::SessionMeta>> {
        Ok(self.directory.sessions_for(user_id)?)
    }

    /// The handle a user currently goes by.
    ///
    /// `UserRepository` has no update method, so a handle change is recorded
    /// in the directory rather than on the user row. Everything that renders a
    /// handle goes through here so the two cannot disagree.
    pub fn effective_handle(&self, user: &User) -> String {
        self.directory
            .handle_of(&user.user_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| user.handle.clone())
    }

    /// Change a user's handle.
    ///
    /// Uniqueness is checked against the repository *and* the directory, under
    /// a lock, so two renames cannot both pass the check. See
    /// [`crate::directory`] for why the change is not written to the user row.
    ///
    /// **A handle you rename away from is retired, not released.** The
    /// repository still holds it on the original row and has no update method,
    /// so it stays reserved to the account that first took it. Freeing it
    /// would need `UserRepository::rename`, and quietly letting somebody else
    /// take a name that still points at your row would be worse than keeping
    /// it.
    pub async fn rename(&self, user: &User, handle: &str) -> ApiResult<String> {
        let handle = Self::normalize_handle(handle)?;
        let _guard = self.rename_lock.lock().await;
        if handle == self.effective_handle(user) {
            return Ok(handle);
        }
        let taken_in_repository = self
            .db
            .users()
            .by_handle(&handle)
            .await?
            .is_some_and(|other| other.user_id != user.user_id);
        let taken_in_directory = self
            .directory
            .owner_of_handle(&handle)?
            .is_some_and(|owner| owner != user.user_id);
        if taken_in_repository || taken_in_directory {
            return Err(ApiError::new(
                ErrorCode::HandleTaken,
                format!("handle `{handle}` is already taken"),
            ));
        }
        self.directory.set_handle(&user.user_id, &handle)?;
        Ok(handle)
    }
}

/// A short, stable, URL-safe user id, as the contract's `usr_7c9a1f`.
fn new_user_id() -> String {
    let uuid = uuid::Uuid::new_v4();
    format!("usr_{}", &uuid.simple().to_string()[..12])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Directory;
    use crate::memory::{InMemoryDatabase, InMemoryKv};
    use cb_ports::FixedClock;

    /// The contract's canonical all-zero-entropy phrase.
    const ABANDON: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
                           abandon abandon abandon about";
    const ABANDON_ADDRESS: &str = "0x9858EfFD232B4033E47d90003D41EC34EcaEda94";

    fn service() -> (Arc<InMemoryDatabase>, FixedClock, Arc<AuthService>) {
        let db = InMemoryDatabase::shared();
        let directory = Arc::new(Directory::new(Arc::new(InMemoryKv::new())));
        let clock = FixedClock::new(
            DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
                .expect("static")
                .with_timezone(&Utc),
        );
        let service = AuthService::new(
            db.clone(),
            directory,
            Arc::new(clock.clone()),
            Duration::hours(24),
            "a-server-secret-2b71e4c09fd3a856",
        )
        .expect("the harness secret clears the floor");
        (db, clock, Arc::new(service))
    }

    /// Sign the outstanding challenge the way a wallet would.
    fn sign_challenge(auth: &AuthService, mnemonic: &str, index: u32) -> (Address, String) {
        let (secret, address) = eth::derive_from_mnemonic(mnemonic, index).expect("derive");
        let challenge = auth.challenge(&address);
        let signature = eth::sign_personal_sign(&secret, &challenge.message).expect("sign");
        (address, signature)
    }

    #[tokio::test]
    async fn a_first_login_creates_the_account_and_says_so() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let result = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect("login");
        assert!(result.is_new);
        assert_eq!(result.user.handle, "satoshi");
        assert_eq!(result.user.address, ABANDON_ADDRESS);
        assert_eq!(
            eth::checksum_address(&result.login_address),
            ABANDON_ADDRESS
        );
        assert!(result.session_token.starts_with("sess_"));
    }

    #[tokio::test]
    async fn the_trading_address_is_derived_and_is_not_the_login_address() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let result = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect("login");
        assert_ne!(result.trading_address, result.login_address);
        // Deterministic: the same login always reaches the same money.
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let again = auth.login(&address, &signature, None).await.expect("login");
        assert_eq!(again.trading_address, result.trading_address);
    }

    #[tokio::test]
    async fn the_gateway_holds_the_trading_key_for_every_account() {
        // Whether the account signed in with a signature or a mnemonic, the
        // trading key is derived, so trading always works.
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let result = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect("login");
        let (key, derived) = auth.trading_key_of(&result.user).expect("trading key");
        assert_eq!(derived, result.trading_address);
        let public = cb_crypto::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &key);
        assert_eq!(cb_crypto::address_from_pubkey(&public), derived);
    }

    #[tokio::test]
    async fn a_second_login_finds_the_account_and_ignores_the_handle() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let first = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect("first");

        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let second = auth
            .login(&address, &signature, Some("somebody_else"))
            .await
            .expect("second");
        assert!(!second.is_new);
        assert_eq!(second.user.user_id, first.user.user_id);
        assert_eq!(
            second.user.handle, "satoshi",
            "the handle is not reassigned"
        );
        assert_ne!(second.session_token, first.session_token);
    }

    #[tokio::test]
    async fn a_first_login_without_a_handle_returns_handle_required() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let err = auth
            .login(&address, &signature, None)
            .await
            .expect_err("no handle");
        assert_eq!(err.code, ErrorCode::HandleRequired);
        assert_eq!(err.code.status().as_u16(), 422);
    }

    #[tokio::test]
    async fn a_signature_for_another_address_is_refused_without_naming_the_signer() {
        let (_db, _clock, auth) = service();
        let (_, claimed) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        let (other_secret, _) = eth::derive_from_mnemonic(ABANDON, 1).expect("derive");
        let challenge = auth.challenge(&claimed);
        let signature = eth::sign_personal_sign(&other_secret, &challenge.message).expect("sign");

        let err = auth
            .login(&claimed, &signature, Some("satoshi"))
            .await
            .expect_err("mismatch");
        assert_eq!(err.code, ErrorCode::BadSignature);
        assert!(
            !err.message.contains("0x"),
            "the error must not reveal which address it recovered: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn a_login_without_a_challenge_is_refused() {
        let (_db, _clock, auth) = service();
        let (secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        let signature = eth::sign_personal_sign(&secret, "some other message").expect("sign");
        let err = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect_err("no challenge");
        assert_eq!(err.code, ErrorCode::ChallengeExpired);
    }

    #[tokio::test]
    async fn a_signature_cannot_be_replayed() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        auth.login(&address, &signature, Some("satoshi"))
            .await
            .expect("first");
        let err = auth
            .login(&address, &signature, Some("satoshi"))
            .await
            .expect_err("replay");
        assert_eq!(err.code, ErrorCode::ChallengeExpired);
    }

    #[tokio::test]
    async fn a_stranger_cannot_knock_a_pending_login_off_its_challenge() {
        // Finding 7. The victim requests a challenge and starts signing it; an
        // unauthenticated attacker requests one for the same address. The
        // victim's signature must still log in.
        let (_db, _clock, auth) = service();
        let (secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        let victims = auth.challenge(&address);
        let _attackers = auth.challenge(&address);
        let signature = eth::sign_personal_sign(&secret, &victims.message).expect("sign");

        auth.login(&address, &signature, Some("satoshi"))
            .await
            .expect("the victim's signature still logs in");

        // And it is still single-use afterwards.
        assert_eq!(
            auth.login(&address, &signature, Some("satoshi"))
                .await
                .expect_err("replay")
                .code,
            ErrorCode::BadSignature
        );
    }

    #[tokio::test]
    async fn a_failed_login_does_not_retire_anybody_elses_challenge() {
        // The other half of finding 7: moving the eviction from
        // POST /auth/challenge to POST /auth/login would be the same denial.
        let (_db, _clock, auth) = service();
        let (secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        let issued = auth.challenge(&address);
        let signature = eth::sign_personal_sign(&secret, &issued.message).expect("sign");

        for bad in ["0xdeadbeef", "not hex"] {
            assert_eq!(
                auth.login(&address, bad, Some("satoshi"))
                    .await
                    .expect_err("garbage")
                    .code,
                ErrorCode::BadSignature
            );
        }
        auth.login(&address, &signature, Some("satoshi"))
            .await
            .expect("the real signature survived the noise");
    }

    #[tokio::test]
    async fn retiring_the_matched_challenge_is_what_makes_a_signature_single_use() {
        // The store's removal, not the verification, is the atomic point: two
        // logins racing with the same captured signature both verify, and only
        // the one that removes the challenge is allowed through.
        let (_db, _clock, auth) = service();
        let (secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        let issued = auth.challenge(&address);
        let signature = eth::sign_personal_sign(&secret, &issued.message).expect("sign");

        assert!(auth.challenges().retire(&address, &issued.nonce), "first");
        assert!(
            !auth.challenges().retire(&address, &issued.nonce),
            "a second retire of the same nonce reports that it was gone"
        );
        assert_eq!(
            auth.login(&address, &signature, Some("satoshi"))
                .await
                .expect_err("the challenge is gone")
                .code,
            ErrorCode::ChallengeExpired
        );
    }

    #[tokio::test]
    async fn a_challenge_older_than_five_minutes_is_refused() {
        let (_db, clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        clock.advance(Duration::minutes(6));
        assert_eq!(
            auth.login(&address, &signature, Some("satoshi"))
                .await
                .expect_err("expired")
                .code,
            ErrorCode::ChallengeExpired
        );
    }

    #[tokio::test]
    async fn a_malformed_signature_is_refused_rather_than_panicking() {
        let (_db, _clock, auth) = service();
        let (_secret, address) = eth::derive_from_mnemonic(ABANDON, 0).expect("derive");
        for bad in ["", "0x", "0xdeadbeef", "not hex"] {
            auth.challenge(&address);
            let err = auth
                .login(&address, bad, Some("satoshi"))
                .await
                .expect_err("malformed");
            assert_eq!(err.code, ErrorCode::BadSignature, "`{bad}`");
        }
    }

    #[tokio::test]
    async fn a_taken_handle_is_rejected_case_insensitively() {
        let (_db, _clock, auth) = service();
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        auth.login(&address, &signature, Some("satoshi"))
            .await
            .expect("first");

        let (address, signature) = sign_challenge(&auth, ABANDON, 1);
        let err = auth
            .login(&address, &signature, Some("SATOSHI"))
            .await
            .expect_err("clash");
        assert_eq!(err.code, ErrorCode::HandleTaken);
    }

    #[tokio::test]
    async fn handles_outside_the_contract_rules_are_rejected() {
        let (_db, _clock, auth) = service();
        for (index, bad) in ["ab", "has space", "hy-phen", "dot."].iter().enumerate() {
            let (address, signature) = sign_challenge(&auth, ABANDON, index as u32);
            assert_eq!(
                auth.login(&address, &signature, Some(bad))
                    .await
                    .expect_err("invalid")
                    .code,
                ErrorCode::ValidationFailed,
                "`{bad}` should not be a valid handle"
            );
        }
    }

    #[tokio::test]
    async fn the_development_login_derives_the_canonical_address() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("tester"))
            .await
            .expect("dev login");
        assert_eq!(result.user.address, ABANDON_ADDRESS);
        assert!(result.is_new);
        // It reaches the same trading account a signature login would.
        let (address, signature) = sign_challenge(&auth, ABANDON, 0);
        let signed_in = auth.login(&address, &signature, None).await.expect("login");
        assert_eq!(signed_in.trading_address, result.trading_address);
    }

    #[tokio::test]
    async fn a_bad_mnemonic_on_the_development_login_is_a_validation_error() {
        let (_db, _clock, auth) = service();
        let err = auth
            .dev_login("not a mnemonic", 0, Some("tester"))
            .await
            .expect_err("bad mnemonic");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
    }

    #[tokio::test]
    async fn an_unusable_server_secret_is_refused_at_construction() {
        // Finding 2. The construction site is the choke point every
        // deployment and every test passes through, so the floor lives here
        // rather than in the composition root.
        let build = |secret: &str| {
            AuthService::new(
                InMemoryDatabase::shared(),
                Arc::new(Directory::new(Arc::new(InMemoryKv::new()))),
                Arc::new(FixedClock::new(Utc::now())),
                Duration::hours(1),
                secret,
            )
        };
        assert!(build("").is_err(), "an empty secret");
        assert!(build("too-short").is_err(), "under the length floor");
        assert!(
            build("local-development-secret-do-not-reuse").is_err(),
            "the value shipped in rust/.env.example is published, so it secures nothing"
        );
        assert!(build("a-server-secret-2b71e4c09fd3a856").is_ok());
    }

    #[tokio::test]
    async fn authentication_distinguishes_the_failure_modes_internally() {
        let (_db, clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        let token = result.session_token;

        assert_eq!(
            auth.authenticate(&token).await.expect("valid").handle,
            "satoshi"
        );
        assert_eq!(
            auth.authenticate("").await.expect_err("empty"),
            AuthFailure::Missing
        );
        assert_eq!(
            auth.authenticate("sess_notarealtoken")
                .await
                .expect_err("unknown"),
            AuthFailure::Unknown
        );

        clock.advance(Duration::hours(25));
        assert_eq!(
            auth.authenticate(&token).await.expect_err("expired"),
            AuthFailure::Expired
        );
    }

    #[tokio::test]
    async fn a_revoked_session_stops_working_immediately() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        auth.logout(&result.session_token).await.expect("logout");
        assert_eq!(
            auth.authenticate(&result.session_token)
                .await
                .expect_err("revoked"),
            AuthFailure::Revoked
        );
        auth.logout(&result.session_token)
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn one_users_token_never_resolves_to_another_user() {
        let (_db, _clock, auth) = service();
        let alice = auth
            .dev_login(ABANDON, 0, Some("alice"))
            .await
            .expect("alice");
        let bob = auth.dev_login(ABANDON, 1, Some("bob")).await.expect("bob");
        assert_ne!(alice.user.address, bob.user.address);
        assert_ne!(alice.trading_address, bob.trading_address);
        assert_eq!(
            auth.authenticate(&alice.session_token)
                .await
                .expect("alice")
                .user_id,
            alice.user.user_id
        );
    }

    #[tokio::test]
    async fn a_trading_address_resolves_back_to_its_account() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");

        let found = auth
            .user_by_trading_address(&result.trading_address)
            .await
            .expect("lookup")
            .expect("the account is found");
        assert_eq!(found.user_id, result.user.user_id);

        // An address belonging to nobody — a bot trading over RPC — resolves
        // to nothing rather than erroring.
        assert!(auth
            .user_by_trading_address(&Address([0xbb; 20]))
            .await
            .expect("lookup")
            .is_none());
    }

    #[tokio::test]
    async fn an_account_created_before_this_process_started_still_resolves() {
        // The index is derived, not stored, so a gateway that restarts has to
        // rebuild it from the user table rather than from its own memory.
        let (db, clock, _auth) = service();
        let directory = Arc::new(crate::directory::Directory::new(
            Arc::new(InMemoryKv::new()),
        ));
        let first = AuthService::new(
            db.clone(),
            Arc::clone(&directory),
            Arc::new(clock.clone()),
            Duration::hours(24),
            "a-server-secret-2b71e4c09fd3a856",
        )
        .expect("service");
        let result = first
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");

        // A fresh service over the same database and secret, with an empty index.
        let restarted = AuthService::new(
            db,
            directory,
            Arc::new(clock),
            Duration::hours(24),
            "a-server-secret-2b71e4c09fd3a856",
        )
        .expect("service");
        let found = restarted
            .user_by_trading_address(&result.trading_address)
            .await
            .expect("lookup")
            .expect("the account is found after a restart");
        assert_eq!(found.user_id, result.user.user_id);
    }

    #[tokio::test]
    async fn a_suspended_account_cannot_obtain_a_session() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        auth.directory()
            .suspend(&crate::directory::Suspension {
                user_id: result.user.user_id.clone(),
                reason: "abusive order flow".into(),
                by: "0xoperator".into(),
                at: Utc::now(),
            })
            .expect("suspend");

        let err = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect_err("suspended");
        assert_eq!(err.code, ErrorCode::AccountSuspended);
        assert_eq!(err.code.status().as_u16(), 403);
    }

    #[tokio::test]
    async fn roles_default_to_user_and_admins_are_named_in_configuration() {
        let (db, directory, clock) = (
            InMemoryDatabase::shared(),
            Arc::new(Directory::new(Arc::new(InMemoryKv::new()))),
            FixedClock::new(Utc::now()),
        );
        let auth = AuthService::new(
            db,
            directory,
            Arc::new(clock),
            Duration::hours(1),
            "roles-test-server-secret-1c8f05de73ba",
        )
        .expect("service")
        .with_admin_handles(vec!["Operator".into()]);

        let user = User {
            user_id: "usr_1".into(),
            handle: "satoshi".into(),
            address: ABANDON_ADDRESS.into(),
            recovery_hash: SIGNATURE_AUTH_MARKER.into(),
            is_bot: false,
            created_at: Utc::now(),
            last_seen_at: Utc::now(),
        };
        assert_eq!(auth.roles_of(&user), vec!["user"]);
        assert!(!auth.is_admin(&user));
        let operator = User {
            handle: "operator".into(),
            ..user
        };
        assert_eq!(auth.roles_of(&operator), vec!["user", "admin"]);
        assert!(auth.is_admin(&operator));
    }

    #[tokio::test]
    async fn the_operator_role_follows_the_login_address_not_the_handle() {
        // Finding 1. The handle `admin` is first-come and self-chosen, so a
        // stranger who registers it must NOT receive the role; the operator is
        // named by an address, which is held by whoever holds its key.
        let (db, directory, clock) = (
            InMemoryDatabase::shared(),
            Arc::new(Directory::new(Arc::new(InMemoryKv::new()))),
            FixedClock::new(Utc::now()),
        );
        let (_key, operator_address) = eth::derive_from_mnemonic(ABANDON, 7).expect("derive");
        let auth = AuthService::new(
            db,
            directory,
            Arc::new(clock),
            Duration::hours(1),
            "roles-test-server-secret-1c8f05de73ba",
        )
        .expect("service")
        .with_admin_addresses(vec![operator_address]);

        let squatter = User {
            user_id: "usr_squatter".into(),
            // The very handle the local workflow used to grant the role on.
            handle: "admin".into(),
            address: ABANDON_ADDRESS.into(),
            recovery_hash: SIGNATURE_AUTH_MARKER.into(),
            is_bot: false,
            created_at: Utc::now(),
            last_seen_at: Utc::now(),
        };
        assert_eq!(
            auth.roles_of(&squatter),
            vec!["user"],
            "claiming the handle `admin` must not grant the operator role"
        );
        assert!(!auth.is_admin(&squatter));

        let operator = User {
            user_id: "usr_operator".into(),
            handle: "someone_else".into(),
            address: eth::checksum_address(&operator_address),
            ..squatter.clone()
        };
        assert!(
            auth.is_admin(&operator),
            "the configured address holds the role whatever it calls itself"
        );

        // The stored address is compared as an address, so lower-case storage
        // decides the same way the checksummed form does.
        let lowercased = User {
            address: format!("0x{}", hex::encode(operator_address.0)),
            ..operator.clone()
        };
        assert!(auth.is_admin(&lowercased));
    }

    #[tokio::test]
    async fn renaming_into_an_admin_handle_grants_nothing_when_admins_are_addresses() {
        // The escalation-by-rename path from finding 1: with the role bound to
        // an address there is no handle to rename into.
        let (db, directory, clock) = (
            InMemoryDatabase::shared(),
            Arc::new(Directory::new(Arc::new(InMemoryKv::new()))),
            FixedClock::new(Utc::now()),
        );
        let (_key, operator_address) = eth::derive_from_mnemonic(ABANDON, 7).expect("derive");
        let auth = AuthService::new(
            db,
            Arc::clone(&directory),
            Arc::new(clock),
            Duration::hours(1),
            "roles-test-server-secret-1c8f05de73ba",
        )
        .expect("service")
        .with_admin_addresses(vec![operator_address]);

        let alice = auth
            .dev_login(ABANDON, 0, Some("alice"))
            .await
            .expect("login");
        assert_eq!(auth.roles_of(&alice.user), vec!["user"]);
        auth.rename(&alice.user, "operator").await.expect("rename");
        assert_eq!(
            auth.roles_of(&alice.user),
            vec!["user"],
            "a rename must not move the operator role"
        );
    }

    #[tokio::test]
    async fn a_handle_change_takes_effect_and_retires_the_old_name() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        let renamed = auth
            .rename(&result.user, "satoshi_v2")
            .await
            .expect("rename");
        assert_eq!(renamed, "satoshi_v2");
        assert_eq!(auth.effective_handle(&result.user), "satoshi_v2");

        // The old name is retired, not released: the repository still holds it
        // on the original row, and it has no update method.
        assert_eq!(
            auth.dev_login(ABANDON, 1, Some("satoshi"))
                .await
                .expect_err("retired")
                .code,
            ErrorCode::HandleTaken
        );
    }

    #[tokio::test]
    async fn a_handle_change_to_a_taken_name_is_refused() {
        let (_db, _clock, auth) = service();
        auth.dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        let other = auth
            .dev_login(ABANDON, 1, Some("hal"))
            .await
            .expect("login");
        assert_eq!(
            auth.rename(&other.user, "satoshi")
                .await
                .expect_err("taken")
                .code,
            ErrorCode::HandleTaken
        );
    }

    #[tokio::test]
    async fn renaming_to_the_same_handle_is_a_no_op() {
        let (_db, _clock, auth) = service();
        let result = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        assert_eq!(
            auth.rename(&result.user, "SATOSHI").await.expect("no-op"),
            "satoshi"
        );
    }

    #[tokio::test]
    async fn sessions_are_listed_and_can_be_revoked_individually() {
        let (_db, _clock, auth) = service();
        let first = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        let second = auth.dev_login(ABANDON, 0, None).await.expect("login");
        let user_id = &first.user.user_id;

        let sessions = auth.sessions_of(user_id).expect("list");
        assert_eq!(sessions.len(), 2);

        let target = crate::directory::Directory::session_id_for(&secrets::hash_session_token(
            &first.session_token,
        ));
        auth.revoke_session(user_id, &target).await.expect("revoke");
        assert_eq!(
            auth.authenticate(&first.session_token)
                .await
                .expect_err("revoked"),
            AuthFailure::Revoked
        );
        // The other session is untouched.
        auth.authenticate(&second.session_token)
            .await
            .expect("still valid");
        assert_eq!(auth.sessions_of(user_id).expect("list").len(), 1);
    }

    #[tokio::test]
    async fn revoking_all_sessions_keeps_the_one_making_the_request() {
        let (_db, _clock, auth) = service();
        let first = auth
            .dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        let second = auth.dev_login(ABANDON, 0, None).await.expect("login");
        let third = auth.dev_login(ABANDON, 0, None).await.expect("login");
        let user_id = &first.user.user_id;

        let revoked = auth
            .revoke_other_sessions(user_id, &third.session_token)
            .await
            .expect("revoke all");
        assert_eq!(revoked, 2);
        auth.authenticate(&third.session_token).await.expect("kept");
        assert!(auth.authenticate(&first.session_token).await.is_err());
        assert!(auth.authenticate(&second.session_token).await.is_err());
    }

    #[tokio::test]
    async fn one_users_session_cannot_be_revoked_through_another() {
        let (_db, _clock, auth) = service();
        let alice = auth
            .dev_login(ABANDON, 0, Some("alice"))
            .await
            .expect("alice");
        let bob = auth.dev_login(ABANDON, 1, Some("bob")).await.expect("bob");
        let alices_session = crate::directory::Directory::session_id_for(
            &secrets::hash_session_token(&alice.session_token),
        );
        assert!(
            auth.revoke_session(&bob.user.user_id, &alices_session)
                .await
                .is_err(),
            "bob must not be able to name alice's session id"
        );
        auth.authenticate(&alice.session_token)
            .await
            .expect("untouched");
    }

    #[tokio::test]
    async fn expired_sessions_and_challenges_are_purged() {
        let (_db, clock, auth) = service();
        auth.dev_login(ABANDON, 0, Some("satoshi"))
            .await
            .expect("login");
        auth.challenge(&Address([5; 20]));
        assert_eq!(auth.purge_expired_sessions().await.expect("purge"), 0);
        clock.advance(Duration::hours(25));
        assert_eq!(auth.purge_expired_sessions().await.expect("purge"), 1);
        assert!(auth.challenges.is_empty());
    }
}
