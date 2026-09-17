//! Domain-error → canonical-error mapping for the REST transport.
//!
//! Handlers return `ApiResult<T>` (= `Result<T, CanonicalError>`); the
//! canonical error middleware renders the wire `Problem` and fills `instance`
//! / `trace_id`.
//!
//! The **management** API uses the canonical error catalogue: the `type`
//! member carries the resource type of the affected OAGW entity. The **proxy**
//! (data plane) renders its own problem bodies with the exact error
//! `type` identifiers from `docs/DESIGN.md` §3.3 — see
//! [`crate::api::rest::handlers::proxy::render_problem`].

use toolkit_canonical_errors::{CanonicalError, Http, resource_error};

use crate::domain::error::DomainError;

/// Errors attributable to an OAGW resource (upstream or route).
///
/// The literal mirrors [`crate::domain::models::UPSTREAM_GTS_TYPE`]; proc-macros
/// cannot resolve a const, and both upstream and route errors share one
/// resource type because the canonical error catalogue is keyed per gear.
#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub struct OagwError;

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        let detail = e.detail();
        match &e {
            // 400 — validation and routing
            DomainError::Route(_)
            | DomainError::Validation(_)
            | DomainError::MissingTargetHost(_)
            | DomainError::InvalidTargetHost(_)
            | DomainError::UnknownTargetHost(_, _) => OagwError::invalid_argument()
                .with_format(detail)
                .create(),
            DomainError::ImmutableField { resource, field } => OagwError::failed_precondition()
                .with_precondition_violation(
                    *field,
                    detail,
                    format!("gts.cf.core.oagw.{resource}.v1~"),
                )
                .with_override(Http::status_code(409))
                .create(),
            DomainError::Semantic(_) => OagwError::invalid_argument()
                .with_format(detail)
                .with_override(Http::status_code(422))
                .create(),

            // 401
            DomainError::AuthenticationFailed(_) => {
                CanonicalError::unauthenticated().with_reason(detail).create()
            }

            // 404
            DomainError::NotFound(_) | DomainError::RouteNotFound { .. } => {
                OagwError::not_found(detail.clone()).with_resource(detail).create()
            }

            // 409 — conflicts
            DomainError::AliasConflict { alias, existing_id } => OagwError::already_exists(format!(
                "an upstream with alias '{alias}' already exists (id {existing_id})"
            ))
            .with_resource(existing_id.to_string())
            .create(),
            DomainError::DuplicateRouteMatch(message) | DomainError::PluginInUse(message) => {
                OagwError::already_exists(message.clone())
                    .with_resource(message)
                    .create()
            }

            // 413
            DomainError::PayloadTooLarge(limit) => OagwError::resource_exhausted(format!(
                "request payload exceeds the {limit} byte limit"
            ))
            .with_quota_violation("body", format!("payload exceeds the {limit} byte limit"))
            .with_override(Http::status_code(413))
            .create(),

            // 429
            DomainError::RateLimitExceeded(_) => OagwError::resource_exhausted(detail.clone())
                .with_quota_violation(detail.clone(), detail)
                .create(),

            // 500
            DomainError::Internal(_) | DomainError::SecretNotFound(_) => {
                CanonicalError::internal(detail).create()
            }

            // 502
            DomainError::ProtocolError(_, _)
            | DomainError::DownstreamError(_)
            | DomainError::StreamAborted(_) => OagwError::unknown(detail)
                .with_override(Http::status_code(502))
                .create(),

            // 503
            DomainError::LinkUnavailable(_, _)
            | DomainError::CircuitBreakerOpen(_)
            | DomainError::PluginNotFound(_) => CanonicalError::service_unavailable()
                .with_detail(detail)
                .create(),

            // 504
            DomainError::ConnectionTimeout(_)
            | DomainError::RequestTimeout(_)
            | DomainError::IdleTimeout(_) => OagwError::deadline_exceeded(detail).create(),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn management_errors_map_onto_the_canonical_status_band() {
        assert_eq!(
            CanonicalError::from(DomainError::Validation("bad".to_owned())).status_code(),
            400
        );
        assert_eq!(
            CanonicalError::from(DomainError::Semantic("bad".to_owned())).status_code(),
            422
        );
        assert_eq!(
            CanonicalError::from(DomainError::NotFound("missing".to_owned())).status_code(),
            404
        );
        assert_eq!(
            CanonicalError::from(DomainError::AliasConflict {
                alias: "a".to_owned(),
                existing_id: uuid::Uuid::nil()
            })
            .status_code(),
            409
        );
        assert_eq!(
            CanonicalError::from(DomainError::DuplicateRouteMatch("dup".to_owned())).status_code(),
            409
        );
        assert_eq!(
            CanonicalError::from(DomainError::ImmutableField {
                resource: "upstream",
                field: "id"
            })
            .status_code(),
            409
        );
        assert_eq!(
            CanonicalError::from(DomainError::PayloadTooLarge(1)).status_code(),
            413
        );
        assert_eq!(
            CanonicalError::from(DomainError::RateLimitExceeded("a".to_owned())).status_code(),
            429
        );
        assert_eq!(
            CanonicalError::from(DomainError::LinkUnavailable("a".to_owned(), "b".to_owned()))
                .status_code(),
            503
        );
        assert_eq!(
            CanonicalError::from(DomainError::RequestTimeout("a".to_owned())).status_code(),
            504
        );
        assert_eq!(
            CanonicalError::from(DomainError::Internal("x".to_owned())).status_code(),
            500
        );
        assert_eq!(
            CanonicalError::from(DomainError::ProtocolError("a".to_owned(), "b".to_owned()))
                .status_code(),
            502
        );
        assert_eq!(
            CanonicalError::from(DomainError::AuthenticationFailed("a".to_owned())).status_code(),
            401
        );
    }

    #[test]
    fn details_survive_the_mapping() {
        let err = CanonicalError::from(DomainError::NotFound("no upstream with id 1".to_owned()));
        assert_eq!(err.detail(), "no upstream with id 1");
        assert_eq!(
            err.resource_type(),
            Some("gts.cf.core.oagw.upstream.v1~")
        );
    }
}
