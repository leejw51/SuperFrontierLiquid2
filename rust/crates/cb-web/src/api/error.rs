//! Gateway errors, and the remedies the interface offers for them.
//!
//! The design system's rejection rule is that the toast's title is the
//! plain-language consequence, its body is the **gateway's own `message`**, and
//! a single-tap remedy appears wherever the code allows one. That only works if
//! `code` is a value the client can branch on, so it is an enum with an explicit
//! unknown variant rather than a string compared at call sites.
//!
//! Nothing here invents an explanation. When the gateway sends a message, the
//! interface shows that message; the local title and remedy are additions beside
//! it, never a replacement for it.

use serde::Deserialize;
use std::collections::BTreeMap;

/// The stable `SCREAMING_SNAKE` identifier the frontend branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    /// Malformed request body or parameter.
    ValidationFailed,
    /// Missing, invalid or expired session. Clears the stored token.
    Unauthorized,
    /// Authenticated but not permitted.
    Forbidden,
    /// Unknown symbol, order or account.
    NotFound,
    /// Registration handle already in use.
    HandleTaken,
    /// A first login for this address arrived without a handle to claim.
    HandleRequired,
    /// The signature does not recover to the claimed address.
    BadSignature,
    /// The login nonce expired or had already been used.
    ChallengeExpired,
    /// The account has been suspended by an operator.
    AccountSuspended,
    /// Faucet claimed too recently.
    FaucetCooldown,
    /// Too many requests.
    RateLimited,
    /// Not enough free balance.
    InsufficientBalance,
    /// The order would breach initial margin.
    InsufficientMargin,
    /// A post-only order would have taken liquidity.
    PostOnlyWouldCross,
    /// A reduce-only order would have grown the position.
    ReduceOnlyWouldIncrease,
    /// The market is suspended or expired.
    MarketHalted,
    /// No fresh mark price for this market.
    OracleStale,
    /// A derivative order priced outside the band around the mark. The
    /// gateway sends the band as `min_price` and `max_price` in `details`.
    PriceOutOfBand,
    /// The gateway cannot reach the sequencer.
    NodeUnavailable,
    /// Unexpected failure.
    Internal,
    /// A code this build does not know about. The gateway's message still shows.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    /// The plain-language consequence used as a toast title.
    ///
    /// It says what happened to the user's intent, not what the server's
    /// internal state is, and it never apologises.
    #[must_use]
    pub const fn consequence(self) -> &'static str {
        match self {
            Self::ValidationFailed => "Order not sent",
            Self::Unauthorized => "Signed out",
            Self::Forbidden => "Not allowed",
            Self::NotFound => "Not found",
            Self::HandleTaken => "That handle is taken",
            Self::HandleRequired => "Pick a handle",
            Self::BadSignature => "That signature did not match",
            Self::ChallengeExpired => "That sign-in expired",
            Self::AccountSuspended => "This account is suspended",
            Self::FaucetCooldown => "Claimed too soon",
            Self::RateLimited => "Too many requests",
            Self::InsufficientBalance => "Not enough balance",
            Self::InsufficientMargin => "Not enough margin",
            Self::PostOnlyWouldCross => "Post-only order rejected",
            Self::ReduceOnlyWouldIncrease => "Reduce-only order rejected",
            Self::MarketHalted => "Market is halted",
            Self::OracleStale => "No fresh price",
            Self::PriceOutOfBand => "Price too far from the mark",
            Self::NodeUnavailable => "Can't reach the sequencer",
            Self::Internal => "Something failed on the gateway",
            Self::Unknown => "Request rejected",
        }
    }
}

/// A single-tap remedy the interface can offer for a rejection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remedy {
    /// Move a post-only price to the far side of the spread.
    Reprice,
    /// Open the faucet, because the account is out of funds.
    OpenFaucet,
    /// Reduce the order size until it fits the available margin.
    ReduceSize,
    /// Nothing the interface can do in one tap.
    None,
}

impl Remedy {
    /// The button label, or `None` when there is nothing to offer.
    #[must_use]
    pub const fn label(&self) -> Option<&'static str> {
        match self {
            Self::Reprice => Some("Reprice"),
            Self::OpenFaucet => Some("Open the faucet"),
            Self::ReduceSize => Some("Reduce size"),
            Self::None => None,
        }
    }
}

/// The `error` object the gateway sends on any non-2xx response.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GatewayError {
    /// The stable identifier.
    pub code: ErrorCode,
    /// The gateway's own human-facing sentence. Shown verbatim.
    pub message: String,
    /// Optional structured context — the same object a client-side guard should
    /// have used, which is how the guard and the rejection stay in agreement.
    #[serde(default)]
    pub details: BTreeMap<String, String>,
}

impl GatewayError {
    /// The remedy the code allows, if any.
    #[must_use]
    pub const fn remedy(&self) -> Remedy {
        match self.code {
            ErrorCode::PostOnlyWouldCross => Remedy::Reprice,
            ErrorCode::InsufficientBalance => Remedy::OpenFaucet,
            ErrorCode::InsufficientMargin => Remedy::ReduceSize,
            _ => Remedy::None,
        }
    }
}

/// The envelope wrapping [`GatewayError`].
#[derive(Debug, Clone, Deserialize)]
pub struct ErrorEnvelope {
    /// The error itself.
    pub error: GatewayError,
}

/// Anything that can go wrong between a click and a rendered result.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ApiError {
    /// The gateway answered with a structured error. This is the good case: the
    /// interface has a code to branch on and a sentence to show.
    #[error("{}", .0.message)]
    Gateway(GatewayError),
    /// The request never completed — no network, DNS failure, CORS.
    #[error("the gateway did not answer: {0}")]
    Transport(String),
    /// The gateway answered with something this build cannot read. Reported as
    /// itself rather than silently coerced into a default value.
    #[error("the gateway's answer could not be read: {0}")]
    Decode(String),
}

impl ApiError {
    /// The error code, where there is one.
    #[must_use]
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Gateway(error) => Some(error.code),
            _ => None,
        }
    }

    /// True when the session is gone and the stored token must be cleared.
    #[must_use]
    pub const fn is_unauthorized(&self) -> bool {
        matches!(
            self,
            Self::Gateway(GatewayError {
                code: ErrorCode::Unauthorized,
                ..
            })
        )
    }

    /// The toast title: the plain-language consequence.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Gateway(error) => error.code.consequence(),
            Self::Transport(_) => "Can't reach the gateway",
            Self::Decode(_) => "Unreadable answer",
        }
    }

    /// The toast body: the gateway's own words wherever it supplied them.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::Gateway(error) => error.message.clone(),
            Self::Transport(detail) => {
                format!("The request never reached it. {detail}")
            }
            Self::Decode(detail) => {
                format!("It answered, but not in a shape this build understands. {detail}")
            }
        }
    }

    /// The single-tap remedy, if the code allows one.
    #[must_use]
    pub fn remedy(&self) -> Remedy {
        match self {
            Self::Gateway(error) => error.remedy(),
            _ => Remedy::None,
        }
    }

    /// A value from the gateway's `details` object.
    #[must_use]
    pub fn detail(&self, key: &str) -> Option<&str> {
        match self {
            Self::Gateway(error) => error.details.get(key).map(String::as_str),
            _ => None,
        }
    }
}

/// A convenient alias for fallible gateway calls.
pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_contracts_error_envelope_parses_and_keeps_its_details() {
        let json = r#"{"error":{"code":"INSUFFICIENT_BALANCE",
            "message":"need 1000.00 USD, have 250.00",
            "details":{"required":"1000.00","available":"250.00"}}}"#;
        let envelope: ErrorEnvelope = serde_json::from_str(json).expect("contract example parses");
        let error = ApiError::Gateway(envelope.error);
        assert_eq!(error.code(), Some(ErrorCode::InsufficientBalance));
        assert_eq!(error.title(), "Not enough balance");
        assert_eq!(error.message(), "need 1000.00 USD, have 250.00");
        assert_eq!(error.remedy(), Remedy::OpenFaucet);
        assert_eq!(error.detail("required"), Some("1000.00"));
    }

    #[test]
    fn a_price_outside_the_band_is_named_and_carries_the_band() {
        let json = r#"{"error":{"code":"PRICE_OUT_OF_BAND",
            "message":"price must be within 10% of the mark",
            "details":{"min_price":"45000","max_price":"55000"}}}"#;
        let envelope: ErrorEnvelope = serde_json::from_str(json).expect("band error parses");
        let error = ApiError::Gateway(envelope.error);
        assert_eq!(error.code(), Some(ErrorCode::PriceOutOfBand));
        assert_eq!(error.title(), "Price too far from the mark");
        assert_eq!(error.detail("min_price"), Some("45000"));
        assert_eq!(error.detail("max_price"), Some("55000"));
    }

    #[test]
    fn an_unknown_code_still_shows_the_gateways_message() {
        let json = r#"{"error":{"code":"SOMETHING_NEW","message":"a new failure"}}"#;
        let envelope: ErrorEnvelope = serde_json::from_str(json).expect("unknown codes parse");
        assert_eq!(envelope.error.code, ErrorCode::Unknown);
        assert_eq!(envelope.error.message, "a new failure");
    }

    #[test]
    fn only_three_codes_carry_a_one_tap_remedy() {
        let remedy_for = |code| {
            GatewayError {
                code,
                message: String::new(),
                details: BTreeMap::new(),
            }
            .remedy()
        };
        assert_eq!(remedy_for(ErrorCode::PostOnlyWouldCross), Remedy::Reprice);
        assert_eq!(
            remedy_for(ErrorCode::InsufficientMargin),
            Remedy::ReduceSize
        );
        assert_eq!(remedy_for(ErrorCode::MarketHalted), Remedy::None);
    }
}
