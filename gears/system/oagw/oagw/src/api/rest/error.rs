//! Canonical error mapping for the OAGW gear.
//!
//! Realizes `cpt-cf-oagw-algo-gf-error-mapping` / `cpt-cf-oagw-dod-gf-error-mapping`:
//! every domain error becomes a canonical error, which the shared middleware
//! projects onto an RFC 9457 `application/problem+json` response.

use toolkit_canonical_errors::transport::Http;
use toolkit_canonical_errors::{CanonicalError, resource_error};

use crate::domain::error::DomainError;

/// Resource-scoped error factory for OAGW.
#[resource_error(gts_id!("cf.oagw.gateway.resource.v1~"))]
pub struct OagwError;

// @cpt-begin:cpt-cf-oagw-dod-gf-error-mapping:p1:inst-full
impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            // Invalid argument -> 400.
            DomainError::Validation { field, message } => OagwError::invalid_argument()
                .with_field_violation(field, message, "INVALID_FIELD")
                .create(),

            // Not found -> 404.
            DomainError::NotFound { resource, id } => {
                OagwError::not_found(format!("{resource} `{id}` not found"))
                    .with_resource(id)
                    .create()
            }

            // Already exists -> 409.
            DomainError::AlreadyExists { resource, key } => {
                OagwError::already_exists(format!("{resource} `{key}` already exists"))
                    .with_resource(key)
                    .create()
            }

            // Conflicting state -> 409.
            DomainError::Conflict { message } => OagwError::aborted(message)
                .with_reason("CONFLICT")
                .create(),

            // A referenced plugin cannot be deleted -> 409, naming the
            // referencing upstreams and routes.
            DomainError::PluginInUse {
                id,
                upstreams,
                routes,
            } => OagwError::aborted(format!(
                "plugin `{id}` is referenced by {} upstream(s) and {} route(s)",
                upstreams.len(),
                routes.len()
            ))
            .with_resource(id)
            .with_reason("PLUGIN_IN_USE")
            .create(),

            // Permission denied -> 403.
            DomainError::PermissionDenied { message } => OagwError::permission_denied()
                .with_reason(message.clone())
                .create(),

            // Unauthenticated -> 401.
            DomainError::Unauthenticated => CanonicalError::unauthenticated()
                .with_reason("NO_SECURITY_CONTEXT")
                .create(),

            // Administratively unavailable -> 503.
            DomainError::Unavailable { message } => CanonicalError::service_unavailable()
                .with_detail(message)
                .create(),

            // Upstream could not be reached -> 502 Bad Gateway.
            DomainError::UpstreamUnreachable { message } => {
                CanonicalError::internal(format!("upstream unreachable: {message}"))
                    .with_override(Http::status_code(502))
                    .create()
            }

            // Upstream exchange exceeded the bound -> 504 Gateway Timeout.
            DomainError::UpstreamTimeout => OagwError::deadline_exceeded(
                "the upstream did not respond within the configured proxy timeout",
            )
            .create(),

            // Declared body over the hard cap -> 413.
            DomainError::PayloadTooLarge => {
                OagwError::out_of_range("request body exceeds the 100 MB limit")
                    .with_field_violation("body", "body exceeds the 100 MB limit", "BODY_TOO_LARGE")
                    .with_override(Http::status_code(413))
                    .create()
            }

            // Rate limited -> 429 with Retry-After.
            DomainError::RateLimited { retry_after_secs } => {
                OagwError::resource_exhausted("rate limit exceeded")
                    .with_quota_violation("rate_limit", "the configured rate limit was exceeded")
                    .with_quota_violation_retry_after_seconds(retry_after_secs)
                    .create()
            }

            // Deliberately not served -> 501.
            DomainError::NotImplemented { message } => OagwError::unimplemented(message).create(),

            // Anything else -> 500.
            DomainError::Internal { message } => CanonicalError::internal(message).create(),
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-gf-error-mapping:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use toolkit_canonical_errors::Problem;

    fn status_of(e: DomainError) -> u16 {
        Problem::from(CanonicalError::from(e)).status
    }

    #[test]
    fn validation_maps_to_400() {
        assert_eq!(status_of(DomainError::validation("alias", "bad")), 400);
    }

    #[test]
    fn unauthenticated_maps_to_401() {
        assert_eq!(status_of(DomainError::Unauthenticated), 401);
    }

    #[test]
    fn permission_denied_maps_to_403() {
        assert_eq!(
            status_of(DomainError::PermissionDenied {
                message: "MISSING_BIND_PERMISSION".to_owned(),
            }),
            403
        );
    }

    #[test]
    fn not_found_maps_to_404() {
        assert_eq!(status_of(DomainError::not_found("upstream", "abc")), 404);
    }

    #[test]
    fn already_exists_maps_to_409() {
        assert_eq!(
            status_of(DomainError::AlreadyExists {
                resource: "upstream".to_owned(),
                key: "example.com".to_owned(),
            }),
            409
        );
    }

    #[test]
    fn conflict_maps_to_409() {
        assert_eq!(
            status_of(DomainError::Conflict {
                message: "duplicate route match".to_owned(),
            }),
            409
        );
    }

    #[test]
    fn plugin_in_use_maps_to_409() {
        assert_eq!(
            status_of(DomainError::PluginInUse {
                id: "p1".to_owned(),
                upstreams: vec!["u1".to_owned()],
                routes: vec![],
            }),
            409
        );
    }

    #[test]
    fn payload_too_large_maps_to_413() {
        assert_eq!(status_of(DomainError::PayloadTooLarge), 413);
    }

    #[test]
    fn rate_limited_maps_to_429() {
        assert_eq!(
            status_of(DomainError::RateLimited {
                retry_after_secs: 1
            }),
            429
        );
    }

    #[test]
    fn not_implemented_maps_to_501() {
        assert_eq!(
            status_of(DomainError::NotImplemented {
                message: "WebTransport is not served".to_owned(),
            }),
            501
        );
    }

    #[test]
    fn upstream_unreachable_maps_to_502() {
        assert_eq!(
            status_of(DomainError::UpstreamUnreachable {
                message: "connection refused".to_owned(),
            }),
            502
        );
    }

    #[test]
    fn unavailable_maps_to_503() {
        assert_eq!(
            status_of(DomainError::Unavailable {
                message: "upstream disabled".to_owned(),
            }),
            503
        );
    }

    #[test]
    fn upstream_timeout_maps_to_504() {
        assert_eq!(status_of(DomainError::UpstreamTimeout), 504);
    }

    #[test]
    fn internal_maps_to_500() {
        assert_eq!(
            status_of(DomainError::Internal {
                message: "boom".to_owned(),
            }),
            500
        );
    }
}
