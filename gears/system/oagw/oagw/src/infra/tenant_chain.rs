//! Tenant-hierarchy walks used at proxy time.

use std::sync::Arc;

use tenant_resolver_sdk::TenantResolverClient;
use tenant_resolver_sdk::models::{BarrierMode, GetAncestorsOptions};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Ancestors of `tenant`, ordered from the direct parent to the root.
///
/// Resolution failures degrade to an empty chain: a tenant that cannot be
/// resolved simply has no inherited configuration.
#[must_use]
pub async fn ancestors_of(
    tenants: Option<&Arc<dyn TenantResolverClient>>,
    security: &SecurityContext,
    tenant: Uuid,
) -> Vec<Uuid> {
    let Some(client) = tenants else {
        return Vec::new();
    };
    let options = GetAncestorsOptions {
        barrier_mode: BarrierMode::Respect,
    };
    let Ok(response) = client
        .get_ancestors(
            security,
            tenant_resolver_sdk::models::TenantId(tenant),
            &options,
        )
        .await
    else {
        return Vec::new();
    };
    response.ancestors.into_iter().map(|t| t.id.0).collect()
}
