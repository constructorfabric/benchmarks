//! The security context a plugin presents to the credential store.

use crate::domain::plugin::RequestContext;

/// Builds a service [`toolkit_security::SecurityContext`] for a credstore
/// lookup.
///
/// The gateway acts on behalf of the tenant that owns the upstream: the
/// subject is the request principal when one is known, the tenant is the
/// request's tenant.
pub(crate) fn service_context(ctx: &RequestContext) -> toolkit_security::SecurityContext {
    let subject = ctx
        .principal_id
        .as_deref()
        .and_then(|p| uuid::Uuid::parse_str(p).ok())
        .unwrap_or_default();
    let tenant = ctx
        .tenant_id
        .as_deref()
        .and_then(|t| uuid::Uuid::parse_str(t).ok())
        .unwrap_or_default();
    toolkit_security::SecurityContext::builder()
        .subject_id(subject)
        .subject_type("service")
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| toolkit_security::SecurityContext::anonymous())
}
