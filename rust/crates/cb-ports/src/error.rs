//! The single error type every port returns.
//!
//! Ports deliberately expose a *closed* error enum rather than
//! `anyhow::Error`. Callers branch on these variants to decide whether to
//! retry, surface a user-facing message, or fail the request — and that
//! decision must not depend on which backend is plugged in. A SQLite unique
//! violation and a PostgreSQL unique violation both arrive as
//! [`PortError::Conflict`].

use std::time::Duration;

/// Result alias for port operations.
pub type PortResult<T> = Result<T, PortError>;

/// Backend-independent failure modes.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    /// The requested entity does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The write conflicts with existing state, such as a unique constraint.
    #[error("conflict: {0}")]
    Conflict(String),

    /// The caller supplied something the port rejects outright.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The backend is reachable but refused on authorisation grounds.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// The backend is unreachable or unhealthy. Retryable.
    #[error("backend unavailable: {0}")]
    Unavailable(String),

    /// The operation exceeded its deadline. Retryable.
    #[error("timed out after {0:?}")]
    Timeout(Duration),

    /// The caller exceeded a quota. Retry after the given delay, if known.
    #[error("rate limited{}", .retry_after.map(|d| format!(", retry in {d:?}")).unwrap_or_default())]
    RateLimited {
        /// How long the caller should wait before retrying, when the backend says.
        retry_after: Option<Duration>,
    },

    /// Optimistic concurrency failure: the row changed under us. Retryable.
    #[error("concurrent modification: {0}")]
    Contention(String),

    /// Serialisation or schema mismatch between stored and expected shape.
    #[error("encoding error: {0}")]
    Encoding(String),

    /// Anything the backend could not classify.
    #[error("internal error: {0}")]
    Internal(String),
}

impl PortError {
    /// Whether a caller may reasonably retry the same operation.
    ///
    /// Retrying a [`PortError::Conflict`] or [`PortError::InvalidArgument`]
    /// will fail identically, so those return `false`. Transport and
    /// contention failures return `true`.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            PortError::Unavailable(_)
                | PortError::Timeout(_)
                | PortError::RateLimited { .. }
                | PortError::Contention(_)
        )
    }

    /// Convenience constructor for the common internal case.
    pub fn internal(msg: impl Into<String>) -> Self {
        PortError::Internal(msg.into())
    }

    /// Convenience constructor for the common unavailable case.
    pub fn unavailable(msg: impl Into<String>) -> Self {
        PortError::Unavailable(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification_is_stable() {
        assert!(PortError::Unavailable("down".into()).is_retryable());
        assert!(PortError::Timeout(Duration::from_secs(1)).is_retryable());
        assert!(PortError::RateLimited { retry_after: None }.is_retryable());
        assert!(PortError::Contention("row".into()).is_retryable());

        assert!(!PortError::NotFound("user".into()).is_retryable());
        assert!(!PortError::Conflict("handle".into()).is_retryable());
        assert!(!PortError::InvalidArgument("qty".into()).is_retryable());
        assert!(!PortError::Internal("boom".into()).is_retryable());
    }

    #[test]
    fn rate_limited_message_mentions_delay_when_known() {
        let with = PortError::RateLimited {
            retry_after: Some(Duration::from_secs(30)),
        };
        assert!(with.to_string().contains("retry in"));
        let without = PortError::RateLimited { retry_after: None };
        assert_eq!(without.to_string(), "rate limited");
    }
}
