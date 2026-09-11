// Updated: 2026-09-01 by Constructor Tech
//! Error mapping for the management API.
//!
//! Two vocabularies, each with its own job:
//!
//! * [`DomainError`] is what the domain service returns. It is projected onto
//!   the platform's `CanonicalError` here, which is what `ApiResult` renders.
//! * [`GatewayError`] is what the Data Plane returns. It carries the OAGW GTS
//!   error identifiers and the `X-OAGW-Error-Source` header (ADR-0007) and
//!   renders itself, because the proxy handler does not sit behind the
//!   canonical-error middleware.

use toolkit_canonical_errors::{CanonicalError, Http, resource_error};

use crate::domain::error::DomainError;

#[resource_error(gts_id!("cf.oagw.management.error.v1~"))]
pub struct OagwError;

impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match e {
            DomainError::Validation(issues) => {
                // The builder's state machine takes one violation to move from
                // "needs" to "has", so the chain is seeded with the first and
                // the rest are appended onto the "has" state.
                let mut it = issues.into_iter();
                match it.next() {
                    None => OagwError::invalid_argument()
                        .with_field_violation("body", "the request is invalid", "INVALID_VALUE")
                        .create(),
                    Some(first) => {
                        let mut b = OagwError::invalid_argument().with_field_violation(
                            first.field.clone(),
                            first.message.clone(),
                            "INVALID_VALUE",
                        );
                        for issue in it {
                            b = b.with_field_violation(issue.field, issue.message, "INVALID_VALUE");
                        }
                        b.create()
                    }
                }
            }
            DomainError::InvalidField { field, message } => OagwError::invalid_argument()
                .with_field_violation(field, message, "INVALID_VALUE")
                .create(),
            DomainError::NotFound { kind, id } => {
                OagwError::not_found(format!("No {kind} with identifier {id}"))
                    .with_resource(id)
                    .create()
            }
            DomainError::Conflict { kind: _, message } => OagwError::already_exists(message)
                .with_resource("conflict")
                .create(),
            DomainError::PluginInUse {
                plugin_id,
                referenced_by,
            } => OagwError::failed_precondition()
                .with_resource(plugin_id.clone())
                .with_precondition_violation(
                    plugin_id,
                    format!(
                        "still referenced by {} upstream(s) and {} route(s)",
                        referenced_by.upstreams.len(),
                        referenced_by.routes.len()
                    ),
                    "PLUGIN_IN_USE",
                )
                // DESIGN tables `PluginInUse` at 409, which is not the status
                // the `failed_precondition` category maps to on its own. The
                // transport override carries the documented status while the
                // category — and therefore the rest of the problem document —
                // stays canonical.
                .with_override(Http::status_code(409))
                .create(),
            DomainError::PluginAlreadyExists(id) => {
                OagwError::already_exists(format!("Plugin {id} already exists"))
                    .with_resource(id)
                    .create()
            }
            DomainError::NoTenant => CanonicalError::unauthenticated()
                .with_reason("the request carries no tenant identity")
                .create(),
            DomainError::Forbidden => OagwError::permission_denied()
                .with_reason("the caller lacks the scope this operation requires")
                .create(),
            DomainError::BadIdentifier(raw) => OagwError::invalid_argument()
                .with_field_violation(
                    "id",
                    format!("'{raw}' is not a valid identifier"),
                    "INVALID_VALUE",
                )
                .create(),
            DomainError::Internal(m) => CanonicalError::internal(m).create(),
        }
    }
}

/// The handler result type for the management API.
pub type RestResult<T> = Result<T, CanonicalError>;

#[cfg(test)]
mod tests {
    use super::*;
    use toolkit_canonical_errors::Problem;

    fn problem(e: DomainError) -> Problem {
        Problem::from(CanonicalError::from(e))
    }

    #[test]
    fn validation_becomes_a_400_with_field_violations() {
        let p = problem(DomainError::invalid("alias", "bad shape"));
        assert_eq!(p.status, 400);
    }

    #[test]
    fn not_found_becomes_a_404() {
        let p = problem(DomainError::upstream_not_found("abc"));
        assert_eq!(p.status, 404);
    }

    #[test]
    fn no_tenant_becomes_a_401() {
        let p = problem(DomainError::NoTenant);
        assert_eq!(p.status, 401);
    }

    #[test]
    fn forbidden_becomes_a_403() {
        let p = problem(DomainError::Forbidden);
        assert_eq!(p.status, 403);
    }

    #[test]
    fn plugin_in_use_becomes_a_409() {
        let p = problem(DomainError::PluginInUse {
            plugin_id: "p".into(),
            referenced_by: crate::domain::error::ReferencedBy {
                upstreams: vec!["u".into()],
                routes: vec![],
            },
        });
        assert_eq!(p.status, 409);
    }

    #[test]
    fn internal_becomes_a_500() {
        let p = problem(DomainError::Internal("boom".into()));
        assert_eq!(p.status, 500);
    }
}
