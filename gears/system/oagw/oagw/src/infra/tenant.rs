//! Tenant hierarchy resolution for alias shadowing (`DESIGN.md` §3.2).

use std::sync::Arc;

use tenant_resolver_sdk::TenantResolverClient;
use uuid::Uuid;

/// Walk the tenant chain for `tenant_id`, ordered descendant → root
/// (the caller's tenant first, then each ancestor up to the root).
///
/// # Errors
///
/// Returns an error when the tenant resolver is unavailable or the tenant
/// does not exist.
pub async fn tenant_chain(
    resolver: &Arc<dyn TenantResolverClient>,
    ctx: &toolkit_security::SecurityContext,
    tenant_id: Uuid,
) -> anyhow::Result<Vec<Uuid>> {
    let response = resolver
        .get_ancestors(
            ctx,
            tenant_resolver_sdk::TenantId(tenant_id),
            &tenant_resolver_sdk::GetAncestorsOptions::default(),
        )
        .await
        .map_err(|err| anyhow::anyhow!("tenant hierarchy unavailable: {err}"))?;
    let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
    chain.push(response.tenant.id.0);
    for ancestor in &response.ancestors {
        chain.push(ancestor.id.0);
    }
    Ok(chain)
}
