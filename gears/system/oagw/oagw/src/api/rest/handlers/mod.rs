//! Handlers of the OAGW management REST surface.
//!
//! Every handler is deliberately thin: it extracts the caller's
//! [`SecurityContext`], the service and (for a mutation) the `x-request-id`
//! header, translates the wire DTO into the validator input and returns either
//! the canonical JSON response or an [`OagwError`], which the gear's error
//! layer renders as `application/problem+json` with the ADR-0007
//! `X-OAGW-Error-Source: gateway` header.
//!
//! Errors are returned as `Result<_, OagwError>` rather than the toolkit's
//! canonical `CanonicalError`: the OAGW problem taxonomy carries OAGW GTS
//! error types (`gts.cf.core.errors.err.v1~cf.oagw.*`) and OAGW-specific
//! extension fields (`alias`, `plugin_id`, `referenced_by`), which the
//! canonical catalogue cannot express.
//!
//! Path segment ids are parsed by this module rather than by an extractor
//! (`Path<String>` + [`path_uuid`]): axum's own `Path<Uuid>` rejection is a
//! plain-text `400`, which would bypass the problem-document contract, so a
//! malformed id is turned into a `404` problem document instead. A resource
//! that cannot be id'd is indistinguishable from one that does not exist
//! (DESIGN section 3.3), and the caller sees the same
//! `application/problem+json` / `X-OAGW-Error-Source: gateway` pair as for any
//! other failure.

pub(crate) mod plugins;
pub(crate) mod proxy;
pub(crate) mod routes;
pub(crate) mod upstreams;

use crate::domain::error::OagwError;

/// Service every handler receives through `Extension<Arc<ControlPlaneService>>`.
pub(crate) type Service = std::sync::Arc<crate::domain::services::ControlPlaneService>;

/// Parses a `{id}` path segment: the canonical bare UUID, then the GTS-form
/// instance id (`gts.cf.core.oagw.<resource>.v1~{uuid}`) via `parse`.
///
/// # Errors
///
/// Returns [`OagwError::NotFound`] with `invalid {resource} id` as the detail
/// when neither spelling matches, so the response is a problem document.
pub(crate) fn path_uuid(
    raw: &str,
    resource: &str,
    parse: impl Fn(&str) -> Option<uuid::Uuid>,
) -> Result<uuid::Uuid, OagwError> {
    let parsed = raw.parse::<uuid::Uuid>().ok().or_else(|| parse(raw));
    parsed.ok_or_else(|| {
        OagwError::not_found(format!("invalid {resource} id")).with_invalid_value(raw)
    })
}
