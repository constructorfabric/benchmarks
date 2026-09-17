//! `DomainError` → [`CanonicalError`] boundary mapping (feature
//! `cpt-cf-oagw-feature-error-semantics`, flow
//! `cpt-cf-oagw-flow-error-semantics-gateway-error`).
//!
//! Bridges the OAGW domain error model into the platform's canonical error
//! framework (toolkit canonical errors / `ApiResult` chains).  The canonical
//! model has no OAGW resource type to hang the `#[resource_error]` marker on
//! (the OAGW catalog is a list of *instances* under
//! `gts.cf.core.errors.err.v1~`, not resource *types*), so this mapping uses
//! the publicly constructible canonical categories and pins the exact wire
//! HTTP status from [`DomainError::status`] via
//! [`Http::status_code`] transport overrides.
//!
//! Toolkit canonical errors only accept transport overrides *within the same
//! status class* (a category default flipped between 4xx and 5xx would change
//! client-fault vs server-fault semantics), so the mapping is class-driven:
//!
//! - **4xx** (`< 500`) — built on [`CanonicalError::unauthenticated`] (401
//!   category) with an in-class override pinning the exact documented status
//!   (400/401/404/409/413/429);
//! - **5xx** (500–504) — built on [`CanonicalError::internal`] (500 category)
//!   with an in-class override for 502/503/504.
//!
//! The canonical bridge is status/category-faithful, but the DESIGN wire
//! contract — `type` = the OAGW GTS *instance*, the `X-OAGW-Error-Source`
//! header, and the OAGW extension fields — is carried by the dedicated
//! [`crate::infra::error_envelope::GatewayError`] envelope (RFC 9457), which
//! precedes this mapping on the wire (DoD
//! `cpt-cf-oagw-dod-error-semantics-envelope`, algorithm
//! `cpt-cf-oagw-algo-error-semantics-build-instance`).

use toolkit_canonical_errors::{CanonicalError, Http};

use crate::domain::error::DomainError;

impl From<DomainError> for CanonicalError {
    fn from(err: DomainError) -> Self {
        let detail = err.to_string();
        // 4xx client-fault instances: constructible as `unauthenticated`
        // (401 class) with the exact documented status pinned in-class.
        let status = err.status();
        if status < 500 {
            CanonicalError::unauthenticated()
                .with_reason(detail)
                .with_override(Http::status_code(status))
                .create()
        } else {
            // 5xx server-fault instances: constructible as `internal`
            // (500 class) with the exact documented status pinned in-class.
            CanonicalError::internal(detail)
                .with_override(Http::status_code(status))
                .create()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use toolkit_canonical_errors::CanonicalError;

    use crate::domain::error::{DomainError, Retriability};

    fn status_of(err: DomainError) -> u16 {
        CanonicalError::from(err).status_code()
    }

    fn sample(variant: DomainError) -> DomainError {
        variant
    }

    #[test]
    fn every_catalog_instance_maps_to_its_documented_status() {
        let samples = [
            sample(DomainError::validation(None, "bad request")),
            sample(DomainError::MissingTargetHost {
                detail: "x".to_owned(),
            }),
            sample(DomainError::InvalidTargetHost {
                detail: "x".to_owned(),
                host: None,
            }),
            sample(DomainError::UnknownTargetHost {
                host: "h".to_owned(),
            }),
            sample(DomainError::AuthenticationFailed {
                detail: "x".to_owned(),
            }),
            sample(DomainError::RouteNotFound {
                detail: "x".to_owned(),
            }),
            sample(DomainError::PluginInUse {
                name: "p".to_owned(),
                referenced_by: Vec::new(),
            }),
            sample(DomainError::AliasConflict {
                alias: "a".to_owned(),
            }),
            sample(DomainError::RouteConflict {
                detail: "x".to_owned(),
            }),
            sample(DomainError::PluginConflict {
                name: "p".to_owned(),
            }),
            sample(DomainError::AccessDenied {
                detail: "x".to_owned(),
                cause: None,
            }),
            sample(DomainError::ServiceUnavailable {
                detail: "x".to_owned(),
                retry_after: None,
                cause: None,
            }),
            sample(DomainError::PayloadTooLarge { max_bytes: None }),
            sample(DomainError::rate_limit_exceeded(
                "x".to_owned(),
                Some(Duration::from_secs(5)),
            )),
            sample(DomainError::SecretNotFound {
                detail: "x".to_owned(),
            }),
            sample(DomainError::ProtocolError {
                detail: "x".to_owned(),
                cause: None,
            }),
            sample(DomainError::DownstreamError {
                detail: "x".to_owned(),
                cause: None,
            }),
            sample(DomainError::StreamAborted {
                detail: "x".to_owned(),
            }),
            sample(DomainError::LinkUnavailable {
                detail: "x".to_owned(),
            }),
            sample(DomainError::CircuitBreakerOpen {
                detail: "x".to_owned(),
            }),
            sample(DomainError::PluginNotFound {
                detail: "x".to_owned(),
            }),
            sample(DomainError::ConnectionTimeout {
                detail: "x".to_owned(),
            }),
            sample(DomainError::RequestTimeout {
                detail: "x".to_owned(),
            }),
            sample(DomainError::IdleTimeout {
                detail: "x".to_owned(),
            }),
        ];

        for err in samples {
            let instance = err.instance().to_owned();
            let expected = err.status();
            assert_eq!(status_of(err), expected, "status mismatch for {instance}");
        }
    }

    #[test]
    fn retriable_429_is_constructible_with_status_fidelity() {
        // Retriability + retry_after are carried by the authoritative
        // GatewayError envelope (see error_envelope tests); the canonical
        // bridge is status-faithful only.
        let err =
            DomainError::rate_limit_exceeded("limited".to_owned(), Some(Duration::from_secs(30)));
        let canonical = CanonicalError::from(err);
        assert_eq!(canonical.status_code(), 429);
    }

    #[test]
    fn retriability_classification_drives_nothing_on_the_canonical_bridge() {
        // Sanity: classification is domain-owned; the bridge preserves status.
        assert_eq!(
            DomainError::RateLimitExceeded {
                detail: "x".to_owned(),
                retry_after: None
            }
            .retriability(),
            Retriability::Retriable
        );
    }
}
