//! Resolving the calling tenant and subject.
//!
//! Realizes `cpt-cf-oagw-algo-gf-tenant-context` / `cpt-cf-oagw-dod-gf-tenant-context`.
//!
//! Inbound authentication is performed by the host runtime before a request
//! reaches this gear; the gear reads the resulting security context off the
//! request and scopes every control-plane read and write by the tenant it
//! carries. A request arriving with no attached security context is treated as
//! belonging to the nil tenant rather than being rejected, so a router can be
//! exercised directly without the auth middleware in front of it.

use toolkit_security::SecurityContext;
use uuid::Uuid;

/// The tenant a request without an attached security context is scoped to.
pub const ANONYMOUS_TENANT: Uuid = Uuid::nil();

/// The calling identity, as far as this gear is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// The tenant every control-plane operation is scoped by.
    pub tenant_id: Uuid,
    /// The authenticated subject, when one is present.
    pub subject_id: Option<Uuid>,
}

impl Caller {
    /// Derive the caller from an optional security context.
    // @cpt-begin:cpt-cf-oagw-dod-gf-tenant-context:p1:inst-full
    #[must_use]
    pub fn from_context(ctx: Option<&SecurityContext>) -> Self {
        match ctx {
            Some(c) => Self {
                tenant_id: c.subject_tenant_id(),
                subject_id: Some(c.subject_id()),
            },
            None => Self {
                tenant_id: ANONYMOUS_TENANT,
                subject_id: None,
            },
        }
    }
    // @cpt-end:cpt-cf-oagw-dod-gf-tenant-context:p1:inst-full

    /// Whether an authenticated subject is present.
    #[must_use]
    pub const fn is_authenticated(&self) -> bool {
        self.subject_id.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_context_yields_the_anonymous_tenant() {
        let c = Caller::from_context(None);
        assert_eq!(c.tenant_id, ANONYMOUS_TENANT);
        assert!(!c.is_authenticated());
    }
}
