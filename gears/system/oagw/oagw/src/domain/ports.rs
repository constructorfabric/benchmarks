//! Non-storage ports the domain layer depends on.

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Tenant hierarchy lookup, used for alias shadowing and enforced-limit
/// inheritance.
#[async_trait]
pub trait TenantDirectory: Send + Sync {
    /// Tenant chain from `tenant_id` up to the root, `tenant_id` first.
    ///
    /// Never empty: an unresolvable tenant yields the single-element chain
    /// `[tenant_id]`, which degrades to "no inheritance" rather than to a
    /// failed request.
    async fn chain(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Which plugin identifiers the in-process registries can resolve.
///
/// Catalog-only identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`,
/// `metrics`) deliberately answer `false`: they exist for types-registry
/// cataloguing, and binding them must fail at write time.
pub trait PluginCatalog: Send + Sync {
    /// `true` when a named auth plugin implementation exists.
    fn has_auth(&self, plugin_ref: &str) -> bool;
    /// `true` when a named guard plugin implementation exists.
    fn has_guard(&self, plugin_ref: &str) -> bool;
    /// `true` when a named transform plugin implementation exists.
    fn has_transform(&self, plugin_ref: &str) -> bool;
}
