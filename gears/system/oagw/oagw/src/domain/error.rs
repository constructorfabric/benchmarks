//! Domain error taxonomy for the OAGW gear.
//!
//! Every failure the gear can produce is expressed here; the REST layer maps
//! these onto canonical errors and RFC 9457 problem responses.

use thiserror::Error;

/// Which side of the gateway produced a response.
///
/// Serialized into the `X-OAGW-Error-Source` response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway itself produced the response.
    Gateway,
    /// The response was relayed from the upstream.
    Upstream,
}

impl ErrorSource {
    /// The header value for this source.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => "gateway",
            Self::Upstream => "upstream",
        }
    }
}

/// The response header carrying the error source.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// The request header selecting one endpoint of a multi-endpoint pool.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Errors raised by the OAGW domain layer.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DomainError {
    /// A request field failed validation.
    #[error("validation failed for `{field}`: {message}")]
    Validation {
        /// The offending field path.
        field: String,
        /// Why it was rejected.
        message: String,
    },

    /// The addressed resource does not exist within the caller's scope.
    #[error("{resource} `{id}` not found")]
    NotFound {
        /// Resource kind, e.g. `upstream`.
        resource: String,
        /// Identifier that failed to resolve.
        id: String,
    },

    /// A uniqueness constraint rejected the write.
    #[error("{resource} `{key}` already exists")]
    AlreadyExists {
        /// Resource kind.
        resource: String,
        /// The conflicting key.
        key: String,
    },

    /// The write conflicts with existing state.
    #[error("conflict: {message}")]
    Conflict {
        /// What conflicted.
        message: String,
    },

    /// A plugin definition is still referenced and cannot be deleted.
    #[error("plugin `{id}` is still in use")]
    PluginInUse {
        /// The plugin identifier.
        id: String,
        /// Referencing upstream identifiers.
        upstreams: Vec<String>,
        /// Referencing route identifiers.
        routes: Vec<String>,
    },

    /// The caller is authenticated but lacks the required permission.
    #[error("permission denied: {message}")]
    PermissionDenied {
        /// Which permission was missing.
        message: String,
    },

    /// The caller is not authenticated.
    #[error("unauthenticated")]
    Unauthenticated,

    /// The target is administratively disabled.
    #[error("unavailable: {message}")]
    Unavailable {
        /// Why the target is unavailable.
        message: String,
    },

    /// The upstream connection could not be established or failed mid-exchange.
    #[error("upstream unreachable: {message}")]
    UpstreamUnreachable {
        /// Transport-level detail, safe for the wire.
        message: String,
    },

    /// Establishing or completing the upstream exchange exceeded the bound.
    #[error("upstream timed out")]
    UpstreamTimeout,

    /// The declared body exceeds the hard cap.
    #[error("payload too large")]
    PayloadTooLarge,

    /// A rate limit rejected the request.
    #[error("rate limit exceeded")]
    RateLimited {
        /// Seconds after which the caller may retry.
        retry_after_secs: u64,
    },

    /// The capability is deliberately not served in this configuration.
    #[error("not implemented: {message}")]
    NotImplemented {
        /// What is not served, and why.
        message: String,
    },

    /// An unexpected internal failure.
    #[error("internal error: {message}")]
    Internal {
        /// Internal detail.
        message: String,
    },
}

impl DomainError {
    /// Convenience constructor for a validation failure.
    pub fn validation(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Validation {
            field: field.into(),
            message: message.into(),
        }
    }

    /// Convenience constructor for a missing resource.
    pub fn not_found(resource: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            resource: resource.into(),
            id: id.into(),
        }
    }

    /// Every domain error the gear raises is gateway-produced by definition;
    /// upstream-sourced responses are relayed, never converted to a
    /// `DomainError`.
    #[must_use]
    pub const fn source(&self) -> ErrorSource {
        ErrorSource::Gateway
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_source_header_values() {
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    }

    #[test]
    fn domain_errors_are_gateway_sourced() {
        assert_eq!(DomainError::Unauthenticated.source(), ErrorSource::Gateway);
    }

    #[test]
    fn display_is_stable() {
        let e = DomainError::validation("server.endpoints", "must not be empty");
        assert!(e.to_string().contains("server.endpoints"));
    }
}
