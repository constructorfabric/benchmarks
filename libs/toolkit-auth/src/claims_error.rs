use thiserror::Error;

/// Errors that can occur during JWT claims validation and processing
#[derive(Debug, Error)]
pub enum ClaimsError {
    #[error("Invalid signature or key")]
    InvalidSignature,

    #[error("Invalid issuer: expected one of {expected:?}, got {actual}")]
    InvalidIssuer {
        expected: Vec<String>,
        actual: String,
    },

    #[error("Invalid audience: expected one of {expected:?}, got {actual:?}")]
    InvalidAudience {
        expected: Vec<String>,
        actual: Vec<String>,
    },

    #[error("Token expired")]
    Expired,

    #[error("Token not yet valid (nbf check failed)")]
    NotYetValid,

    #[error("Malformed claims: {0}")]
    Malformed(String),

    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Missing required claim: {0}")]
    MissingClaim(String),

    #[error("Invalid claim format: {field} - {reason}")]
    InvalidClaimFormat { field: String, reason: String },

    #[error("Unknown key ID after refresh")]
    UnknownKidAfterRefresh,

    #[error("JWT decode failed: {0}")]
    DecodeFailed(String),

    #[error("JWKS fetch failed: {0}")]
    JwksFetchFailed(String),

    #[error("Unknown key ID: {0}")]
    UnknownKeyId(String),

    #[error("Unsupported JWT algorithm: {0}")]
    UnsupportedAlgorithm(String),
}

// Conversion from ClaimsError to AuthError for backward compatibility.
// TODO(DE1302): `AuthError::ValidationFailed` only accepts a String, so the
// remaining ClaimsError variants collapse via `.to_string()`. Introduce a
// structured `ValidationFailed(Box<ClaimsError>)` (or `#[from]` variant) and
// remove this allow once callers can match on the source.
#[allow(unknown_lints, de1302_error_from_to_string)]
impl From<ClaimsError> for crate::errors::AuthError {
    fn from(err: ClaimsError) -> Self {
        match err {
            ClaimsError::Expired => crate::errors::AuthError::TokenExpired,
            ClaimsError::InvalidSignature => {
                crate::errors::AuthError::InvalidToken("Invalid signature".into())
            }
            ClaimsError::InvalidIssuer { expected, actual } => {
                crate::errors::AuthError::IssuerMismatch {
                    expected: expected.join(", "),
                    actual,
                }
            }
            ClaimsError::InvalidAudience { expected, actual } => {
                crate::errors::AuthError::AudienceMismatch { expected, actual }
            }
            ClaimsError::JwksFetchFailed(msg) => crate::errors::AuthError::JwksFetchFailed(msg),
            ClaimsError::UnsupportedAlgorithm(alg) => {
                crate::errors::AuthError::UnsupportedAlgorithm(alg)
            }
            other => crate::errors::AuthError::ValidationFailed(other.to_string()),
        }
    }
}
