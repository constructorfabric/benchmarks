//! Tenant-scope resolution over the tenant resolver.
//!
//! A request acts in its own tenant first and may reach an ancestor's shared
//! upstreams, so the scope a [`TenantHierarchy`] returns is ordered
//! descendant → root. When the resolver answers nothing — the platform has no
//! hierarchy configured, or it is unreachable — the scope is the caller's own
//! tenant alone: never *more* than the caller asked for, so the failure mode
//! loses shared upstreams rather than gaining access to foreign ones.

use std::sync::Arc;

use async_trait::async_trait;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::services::control_plane::TenantHierarchy;

/// Resolves the scope a request acts in.
pub struct TenantScope {
    resolver: Option<Arc<dyn TenantResolverClient>>,
}

impl TenantScope {
    /// A scope resolver over the platform's tenant resolver.
    #[must_use]
    pub fn over(resolver: Arc<dyn TenantResolverClient>) -> Self {
        Self {
            resolver: Some(resolver),
        }
    }

    /// A single-tenant scope, for deployments without a hierarchy.
    #[must_use]
    pub const fn own_tenant_only() -> Self {
        Self { resolver: None }
    }
}

impl Default for TenantScope {
    fn default() -> Self {
        Self::own_tenant_only()
    }
}

#[async_trait]
impl TenantHierarchy for TenantScope {
    async fn scope(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, DomainError> {
        let tenant = ctx.subject_tenant_id();
        if tenant.is_nil() {
            return Err(DomainError::new(
                ErrorKind::AuthFailed,
                "the subject carries no tenant".to_owned(),
            ));
        }
        let Some(resolver) = &self.resolver else {
            return Ok(vec![tenant]);
        };
        // An unreachable hierarchy narrows the scope to the caller's own
        // tenant; it never widens it.
        let ancestors = match resolver
            .get_ancestors(ctx, TenantId(tenant), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => {
                let mut scope = vec![tenant];
                for ancestor in &response.ancestors {
                    scope.push(ancestor.id.0);
                }
                scope
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "tenant hierarchy unavailable; scoping to the caller's own tenant"
                );
                vec![tenant]
            }
        };
        Ok(ancestors)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tenant_scope_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn context(tenant: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("valid context")
    }

    #[tokio::test]
    async fn a_nil_tenant_is_an_authentication_failure() {
        let scope = TenantScope::own_tenant_only();
        let error = scope
            .scope(&context(Uuid::nil()))
            .await
            .expect_err("nil tenant is rejected");
        assert_eq!(error.kind(), ErrorKind::AuthFailed);
    }

    #[tokio::test]
    async fn without_a_resolver_the_scope_is_the_callers_own_tenant() {
        let tenant = Uuid::new_v4();
        let scope = TenantScope::own_tenant_only();
        assert_eq!(
            scope.scope(&context(tenant)).await.expect("scope"),
            vec![tenant]
        );
    }
}
