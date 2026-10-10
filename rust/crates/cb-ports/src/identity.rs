//! Who the user is.
//!
//! The simulator authenticates with a handle and a recovery code it generates
//! itself. A deployment would put single sign-on here instead. Session
//! management sits behind the same port so that the gateway's authorisation
//! layer does not care which one is wired in.

use crate::error::PortResult;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// An authenticated subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Stable internal user id.
    pub user_id: String,
    /// Display handle.
    pub handle: String,
    /// On-chain address controlled on the user's behalf.
    pub address: String,
    /// When the bearing session expires.
    pub expires_at: DateTime<Utc>,
}

/// Authentication and session issuance.
#[async_trait]
pub trait IdentityProvider: Send + Sync + 'static {
    /// Create a new subject, returning the identity and the one-time recovery
    /// secret. The secret is never retrievable again.
    async fn register(&self, handle: &str) -> PortResult<(Identity, String)>;

    /// Exchange a handle and recovery secret for a session.
    async fn authenticate(&self, handle: &str, secret: &str) -> PortResult<Identity>;

    /// Resolve a bearer token to a subject.
    ///
    /// Returns [`crate::PortError::PermissionDenied`] for an expired or
    /// revoked token, distinct from [`crate::PortError::NotFound`] for a token
    /// that never existed — the gateway logs these differently.
    async fn resolve_token(&self, token: &str) -> PortResult<Identity>;

    /// Invalidate a token.
    async fn revoke_token(&self, token: &str) -> PortResult<()>;

    /// Purge expired sessions. Returns how many were removed.
    async fn sweep_expired(&self, now: DateTime<Utc>) -> PortResult<u64>;
}
