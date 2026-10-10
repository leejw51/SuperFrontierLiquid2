//! The error envelope from the API contract, and the one place that decides
//! which HTTP status a failure carries.
//!
//! The frontend branches on `code`, so the mapping from an internal failure
//! to a `SCREAMING_SNAKE` identifier is a published interface, not an
//! implementation detail. It lives here, once, and every handler returns
//! [`ApiError`] rather than building a response by hand — a handler that
//! invents its own status is how two endpoints end up disagreeing about what
//! "not enough money" looks like.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use cb_ports::PortError;
use serde::Serialize;
use serde_json::Value;

/// Result alias for handlers.
pub type ApiResult<T> = Result<T, ApiError>;

/// The stable error identifiers from `docs/api-contract.md` §1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Malformed request body or parameter.
    ValidationFailed,
    /// Missing, invalid or expired session.
    Unauthorized,
    /// Authenticated but not permitted.
    Forbidden,
    /// Unknown symbol, order or account.
    NotFound,
    /// Registration handle already in use.
    HandleTaken,
    /// A first login for this address arrived without a handle.
    HandleRequired,
    /// The signature does not recover to the claimed address.
    BadSignature,
    /// The login nonce has expired or was already used.
    ChallengeExpired,
    /// The account has been suspended by an operator.
    AccountSuspended,
    /// Faucet claimed too recently.
    FaucetCooldown,
    /// Too many requests.
    RateLimited,
    /// Not enough free balance.
    InsufficientBalance,
    /// Order would breach initial margin.
    InsufficientMargin,
    /// Post-only order would take liquidity.
    PostOnlyWouldCross,
    /// Reduce-only order would grow the position.
    ReduceOnlyWouldIncrease,
    /// A derivative order priced outside the chain's band around the mark
    /// (`cb_engine::DERIVATIVE_PRICE_BAND_BPS`, 10%). The chain refuses it
    /// with `ExecError::PriceOutOfBand`; the gateway refuses it first.
    PriceOutOfBand,
    /// Market suspended or expired.
    MarketHalted,
    /// No fresh mark price for this market.
    OracleStale,
    /// The gateway cannot reach the sequencer.
    NodeUnavailable,
    /// The contract describes the action, and this chain cannot perform it.
    ///
    /// Distinct from `FORBIDDEN`, which means the caller may not; this means
    /// nobody can, through this gateway, however they are authenticated. It
    /// exists so an operator reading an audit trail can tell "refused" from
    /// "done" — an endpoint that submits a transaction it knows the chain will
    /// reject, and then records a success, is worse than one that says no.
    NotSupported,
    /// Unexpected failure.
    Internal,
}

impl ErrorCode {
    /// The wire identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ValidationFailed => "VALIDATION_FAILED",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::HandleTaken => "HANDLE_TAKEN",
            Self::HandleRequired => "HANDLE_REQUIRED",
            Self::BadSignature => "BAD_SIGNATURE",
            Self::ChallengeExpired => "CHALLENGE_EXPIRED",
            Self::AccountSuspended => "ACCOUNT_SUSPENDED",
            Self::FaucetCooldown => "FAUCET_COOLDOWN",
            Self::RateLimited => "RATE_LIMITED",
            Self::InsufficientBalance => "INSUFFICIENT_BALANCE",
            Self::InsufficientMargin => "INSUFFICIENT_MARGIN",
            Self::PostOnlyWouldCross => "POST_ONLY_WOULD_CROSS",
            Self::ReduceOnlyWouldIncrease => "REDUCE_ONLY_WOULD_INCREASE",
            Self::PriceOutOfBand => "PRICE_OUT_OF_BAND",
            Self::MarketHalted => "MARKET_HALTED",
            Self::OracleStale => "ORACLE_STALE",
            Self::NodeUnavailable => "NODE_UNAVAILABLE",
            Self::NotSupported => "NOT_SUPPORTED",
            Self::Internal => "INTERNAL",
        }
    }

    /// The HTTP status this code is served with.
    pub fn status(self) -> StatusCode {
        match self {
            Self::ValidationFailed => StatusCode::BAD_REQUEST,
            Self::Unauthorized | Self::BadSignature | Self::ChallengeExpired => {
                StatusCode::UNAUTHORIZED
            }
            Self::Forbidden | Self::AccountSuspended => StatusCode::FORBIDDEN,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::HandleTaken => StatusCode::CONFLICT,
            Self::FaucetCooldown | Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::HandleRequired
            | Self::InsufficientBalance
            | Self::InsufficientMargin
            | Self::PostOnlyWouldCross
            | Self::ReduceOnlyWouldIncrease
            | Self::PriceOutOfBand
            | Self::MarketHalted
            | Self::OracleStale => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NodeUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::NotSupported => StatusCode::NOT_IMPLEMENTED,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// A failure, ready to be rendered as the contract's error envelope.
#[derive(Debug)]
pub struct ApiError {
    /// Stable identifier the frontend branches on.
    pub code: ErrorCode,
    /// Human-facing explanation.
    pub message: String,
    /// Optional structured context.
    pub details: Option<Value>,
}

impl ApiError {
    /// Build an error with no details.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    /// Attach structured context.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// `400 VALIDATION_FAILED`.
    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ValidationFailed, message)
    }

    /// `401 UNAUTHORIZED`.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, message)
    }

    /// `403 FORBIDDEN`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, message)
    }

    /// `404 NOT_FOUND`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// `503 NODE_UNAVAILABLE`.
    pub fn node_unavailable(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NodeUnavailable, message)
    }

    /// `500 INTERNAL`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

/// A port failure becomes an API failure without the caller deciding.
///
/// The classification is deliberately conservative: anything retryable at the
/// port layer is a transport problem the browser should see as `503`, because
/// the alternative — a `500` — tells the frontend to stop rather than retry.
impl From<PortError> for ApiError {
    fn from(e: PortError) -> Self {
        match e {
            PortError::NotFound(ref what) => ApiError::not_found(format!("{what} not found")),
            PortError::Conflict(ref what) => {
                ApiError::new(ErrorCode::HandleTaken, format!("conflict: {what}"))
            }
            PortError::InvalidArgument(ref what) => ApiError::validation(what.clone()),
            PortError::PermissionDenied(ref what) => ApiError::forbidden(what.clone()),
            PortError::Unavailable(_) | PortError::Timeout(_) | PortError::Contention(_) => {
                ApiError::node_unavailable(e.to_string())
            }
            PortError::RateLimited { .. } => {
                ApiError::new(ErrorCode::RateLimited, "upstream rate limit")
            }
            PortError::Encoding(_) | PortError::Internal(_) => ApiError::internal(e.to_string()),
        }
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: Inner<'a>,
}

#[derive(Serialize)]
struct Inner<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: &'a Option<Value>,
}

/// What a client is told when the real message is a backend detail.
///
/// The request id is already on the response as `x-request-id`, and the full
/// message is in the log line beside it, so an operator can join the two in
/// one grep. The caller gets nothing they can use to map our internals.
const OPAQUE_INTERNAL: &str = "the gateway could not complete this request;                                quote the x-request-id header when reporting it";

/// What a client is told when a dependency is down.
const OPAQUE_UNAVAILABLE: &str = "the exchange is temporarily unable to reach the sequencer;                                   retry shortly";

impl ErrorCode {
    /// Whether a message with this code may be shown to the caller.
    ///
    /// `INTERNAL` and `NODE_UNAVAILABLE` messages are built from lower-layer
    /// error strings — `PortError::Internal(e.to_string())`, a connection
    /// refusal naming the sequencer's address — so they describe the server,
    /// not the request. Everything else is a statement about what the caller
    /// sent and is meant to be read by them (finding 8).
    fn message_is_for_the_client(self) -> bool {
        !matches!(self, Self::Internal | Self::NodeUnavailable)
    }

    /// The message a client sees in place of a suppressed one.
    fn opaque_message(self) -> &'static str {
        match self {
            Self::NodeUnavailable => OPAQUE_UNAVAILABLE,
            _ => OPAQUE_INTERNAL,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.code == ErrorCode::Internal {
            tracing::error!(code = self.code.as_str(), message = %self.message, "request failed");
        } else {
            tracing::debug!(code = self.code.as_str(), message = %self.message, "request rejected");
        }
        let shown = if self.code.message_is_for_the_client() {
            self.message.as_str()
        } else {
            self.code.opaque_message()
        };
        let body = Json(Body {
            error: Inner {
                code: self.code.as_str(),
                message: shown,
                details: &self.details,
            },
        });
        (self.code.status(), body).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn every_code_maps_to_the_status_the_contract_states() {
        let expected = [
            (ErrorCode::ValidationFailed, 400),
            (ErrorCode::Unauthorized, 401),
            (ErrorCode::Forbidden, 403),
            (ErrorCode::NotFound, 404),
            (ErrorCode::HandleTaken, 409),
            (ErrorCode::HandleRequired, 422),
            (ErrorCode::BadSignature, 401),
            (ErrorCode::ChallengeExpired, 401),
            (ErrorCode::AccountSuspended, 403),
            (ErrorCode::FaucetCooldown, 429),
            (ErrorCode::RateLimited, 429),
            (ErrorCode::InsufficientBalance, 422),
            (ErrorCode::InsufficientMargin, 422),
            (ErrorCode::PostOnlyWouldCross, 422),
            (ErrorCode::ReduceOnlyWouldIncrease, 422),
            (ErrorCode::PriceOutOfBand, 422),
            (ErrorCode::MarketHalted, 422),
            (ErrorCode::OracleStale, 422),
            (ErrorCode::NodeUnavailable, 503),
            (ErrorCode::Internal, 500),
        ];
        for (code, status) in expected {
            assert_eq!(code.status().as_u16(), status, "{}", code.as_str());
        }
    }

    #[test]
    fn an_internal_error_does_not_tell_the_client_what_broke() {
        // Finding 8: a 500 used to carry the backend's own error text, and a
        // 503 carried the sequencer's address.
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let render = |error: ApiError| {
            let response = error.into_response();
            let status = response.status().as_u16();
            let bytes = futures::executor::block_on(async {
                to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .expect("body")
            });
            (status, String::from_utf8(bytes.to_vec()).expect("utf8"))
        };

        let (status, body) = render(ApiError::internal(
            "connect to 127.0.0.1:7799: Connection refused (os error 61)",
        ));
        assert_eq!(status, 500);
        assert!(body.contains("INTERNAL"), "the code still classifies it");
        assert!(
            !body.contains("127.0.0.1"),
            "the internal RPC address must not reach the client: {body}"
        );
        assert!(!body.contains("os error"), "{body}");
        assert!(body.contains("x-request-id"), "{body}");

        let (status, body) = render(ApiError::node_unavailable(
            "connect to 127.0.0.1:7799: Connection refused (os error 61)",
        ));
        assert_eq!(status, 503);
        assert!(body.contains("NODE_UNAVAILABLE"));
        assert!(!body.contains("127.0.0.1"), "{body}");

        // A message that is about the caller's request still reaches them.
        let (_, body) = render(ApiError::validation("`limit` must be positive"));
        assert!(body.contains("`limit` must be positive"), "{body}");
    }

    #[test]
    fn a_retryable_port_error_is_a_503_not_a_500() {
        let e: ApiError = PortError::Unavailable("node down".into()).into();
        assert_eq!(e.code, ErrorCode::NodeUnavailable);
        let e: ApiError = PortError::Timeout(Duration::from_secs(2)).into();
        assert_eq!(e.code, ErrorCode::NodeUnavailable);
    }

    #[test]
    fn a_port_conflict_is_the_handle_taken_code() {
        let e: ApiError = PortError::Conflict("handle".into()).into();
        assert_eq!(e.code.status().as_u16(), 409);
    }
}
