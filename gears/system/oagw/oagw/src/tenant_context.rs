//! Authenticated caller context bridge.
//!
//! The REST transport extracts the `SecurityContext` inserted by the host
//! gateway's auth middleware; the domain layer must not depend on the transport
//! representation, so handlers convert it into a [`CallerContext`] value and
//! pass that down. Tenant scoping (DESIGN §3.3 "Tenant Scoping") is derived
//! from this value — never from client-supplied body fields.

use uuid::Uuid;

/// The authenticated caller of a management or proxy request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallerContext {
    subject_id: Uuid,
    tenant_id: Uuid,
}

impl CallerContext {
    /// Build a caller context from a tenant id (used by tests and by
    /// non-HTTP entry points).
    #[must_use]
    pub const fn new(tenant_id: Uuid) -> Self {
        Self {
            subject_id: Uuid::nil(),
            tenant_id,
        }
    }

    /// Build a caller context from the transport `SecurityContext`.
    #[must_use]
    pub const fn from_parts(subject_id: Uuid, tenant_id: Uuid) -> Self {
        Self {
            subject_id,
            tenant_id,
        }
    }

    /// Subject id of the caller, when known.
    #[must_use]
    pub const fn subject_id(&self) -> Uuid {
        self.subject_id
    }

    /// Home tenant of the caller.
    ///
    /// `None` when the caller has no tenant (anonymous context or no
    /// `SecurityContext`). Every consumer **fails closed** on `None` with a 401
    /// (DESIGN §3.3 "Tenant Scoping"): the gear never substitutes a default
    /// tenant.
    #[must_use]
    pub const fn tenant_id(&self) -> Option<Uuid> {
        if self.tenant_id.is_nil() {
            None
        } else {
            Some(self.tenant_id)
        }
    }
}

impl From<&toolkit_security::SecurityContext> for CallerContext {
    fn from(context: &toolkit_security::SecurityContext) -> Self {
        Self::from_parts(context.subject_id(), context.subject_tenant_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nil_tenant_maps_to_none() {
        let anonymous = CallerContext::new(Uuid::nil());

        assert_eq!(anonymous.tenant_id(), None);
        assert_eq!(anonymous.subject_id(), Uuid::nil());
    }

    #[test]
    fn from_security_context_carries_subject_and_tenant() {
        let tenant = Uuid::new_v4();
        let subject = Uuid::new_v4();
        let context = toolkit_security::SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(tenant)
            .build()
            .expect("valid security context");

        let caller = CallerContext::from(&context);

        assert_eq!(caller.tenant_id(), Some(tenant));
        assert_eq!(caller.subject_id(), subject);
    }

    #[test]
    fn anonymous_security_context_has_no_tenant() {
        let caller = CallerContext::from(&toolkit_security::SecurityContext::anonymous());

        assert_eq!(caller.tenant_id(), None);
    }
}
