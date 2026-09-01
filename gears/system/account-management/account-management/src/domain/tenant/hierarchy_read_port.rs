//! Narrow read-only seam between `tr_plugin` and AM storage.
//!
//! The plugin OWNS closure-walk pre-order, audit emission, projection,
//! and `tenant_type` hydration. The port OWNS structural reads against
//! `tenants` / `tenant_closure` and the WHERE-clause construction for
//! caller-supplied status filtering. The trait MUST NOT emit audit
//! events (`target: "tr_plugin.audit"` stays in the plugin) and MUST
//! NOT perform `tenant_type` hydration.

use async_trait::async_trait;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::tenant::model::{TenantModel, TenantStatus};

/// Caller-supplied predicate on `tenants.status` that the port
/// translates into a structural WHERE clause.
///
/// `VisibleAll` excludes `Provisioning` (defense-in-depth: closure
/// already excludes it). `VisibleIn(set)` AND-s a positive
/// `status IN (...)` predicate onto `VisibleAll`. Callers translating
/// SDK `&[SdkTenantStatus]` MUST map empty input to `VisibleAll`; an
/// empty `VisibleIn` set is rejected by the impl as caller misuse.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusFilter {
    VisibleAll,
    VisibleIn(Vec<TenantStatus>),
}

/// Barrier-row handling on closure reads. Mirrors the SDK's
/// `BarrierMode` at the domain layer; the plugin maps from the SDK
/// enum at the seam.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarrierMode {
    Respect,
    Ignore,
}

/// Narrow read-only seam between `tr_plugin` and AM storage.
///
/// See gear docs for the responsibility split. Impls live in
/// `infra/storage/repo_impl/hierarchy_read.rs`.
#[async_trait]
pub trait TenantHierarchyReadPort: Send + Sync {
    /// Single visible-row probe. `Ok(None)` when row is absent OR is
    /// `Provisioning`.
    async fn get(&self, id: Uuid) -> Result<Option<TenantModel>, DomainError>;

    /// Locate non-provisioning roots. Returns AT MOST 2 rows so the
    /// plugin can distinguish 0 / 1 / many roots cheaply without
    /// scanning the full table. Ordered by `id` ascending for
    /// deterministic diagnostics.
    async fn get_root(&self) -> Result<Vec<TenantModel>, DomainError>;

    /// Bulk fetch by id with caller-supplied filter. The impl
    /// deduplicates `ids` internally. Returned order is unspecified.
    /// Filter applied at query time; provisioning always excluded.
    async fn get_bulk(
        &self,
        ids: &[Uuid],
        filter: &StatusFilter,
    ) -> Result<Vec<TenantModel>, DomainError>;

    /// Strict ancestors of `descendant_id` via the closure table.
    /// Self-row excluded. Under `BarrierMode::Respect`, only rows
    /// with `barrier = 0`. Returns ancestor ids only — plugin hydrates
    /// via `get_bulk` so hydration mismatches surface as `Internal`.
    async fn get_ancestors(
        &self,
        descendant_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<Vec<Uuid>, DomainError>;

    /// Strict descendants of `ancestor_id` via the closure table.
    /// Self-row excluded. Under `BarrierMode::Respect`, only rows
    /// with `barrier = 0`. `status_filter` is INTENTIONALLY NOT a
    /// parameter — it is an EMISSION predicate applied by the plugin
    /// during the pre-order walk so `Root → Suspended → Active`
    /// filtered by `[Active]` still emits the `Active` leaf.
    async fn get_descendants(
        &self,
        ancestor_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<Vec<Uuid>, DomainError>;

    /// `true` iff a closure row `(ancestor_id, descendant_id)` exists
    /// under the chosen barrier mode. Visibility / self-reference
    /// probes are the plugin's responsibility (via `get` / `get_bulk`).
    async fn is_ancestor(
        &self,
        ancestor_id: Uuid,
        descendant_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<bool, DomainError>;
}
