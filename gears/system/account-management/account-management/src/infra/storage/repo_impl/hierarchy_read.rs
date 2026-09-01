//! `TenantHierarchyReadAdapter` — infra impl of `TenantHierarchyReadPort`.
//!
//! Sibling of `TenantRepoImpl` rather than an additional method block on
//! it: the adapter's trust elevation (`AccessScope::allow_all()`) is
//! intentionally distinct from `TenantRepo`'s scope-parameterized
//! contract. The adapter speaks domain types only (`TenantModel`,
//! `TenantStatus`); SDK-side `BarrierMode` / `SdkTenantStatus`
//! translation happens at the plugin call site.
//!
//! Trust rationale: AM's `tenants` and `tenant_closure` entities are
//! declared `no_tenant, no_resource, no_owner, no_type`, so
//! `AccessScope::allow_all()` adds no implicit WHERE. The plugin's
//! caller-scope check happens at the gateway (DESIGN §4.2 — gateway is
//! trusted); the plugin reads unconditionally.

use std::sync::Arc;

use async_trait::async_trait;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::tenant::hierarchy_read_port::{
    BarrierMode, StatusFilter, TenantHierarchyReadPort,
};
use crate::domain::tenant::model::{TenantModel, TenantStatus};
use crate::infra::storage::entity::{tenant_closure, tenants};

use super::AmDbProvider;
use super::helpers::{entity_to_model, map_scope_err};

/// `SeaORM`-backed implementation of `TenantHierarchyReadPort` used by
/// the in-crate `tr_plugin`.
pub struct TenantHierarchyReadAdapter {
    db: Arc<AmDbProvider>,
}

impl TenantHierarchyReadAdapter {
    #[must_use]
    pub fn new(db: Arc<AmDbProvider>) -> Self {
        Self { db }
    }

    /// Single named call site for the plugin's trust elevation. Audit
    /// and grep can point at this one function rather than every
    /// `AccessScope::allow_all()` constructed inside the trait impl.
    fn plugin_scope() -> AccessScope {
        AccessScope::allow_all()
    }

    /// Unconditional provisioning-exclusion predicate on
    /// `tenants.status`. Applied on every `tenants` read on the
    /// SDK path regardless of caller-supplied status filter.
    fn visible_status() -> Condition {
        Condition::all().add(tenants::Column::Status.ne(TenantStatus::Provisioning.as_smallint()))
    }

    /// Translate a [`StatusFilter`] into a `SeaORM` `Condition`.
    /// `StatusFilter::VisibleIn(vec![])` is caller misuse and is
    /// caught by `debug_assert!`; in release builds the empty set
    /// collapses to `VisibleAll` semantics rather than `WHERE false`.
    fn status_filter_to_condition(filter: &StatusFilter) -> Condition {
        match filter {
            StatusFilter::VisibleAll => Self::visible_status(),
            StatusFilter::VisibleIn(statuses) => {
                debug_assert!(
                    !statuses.is_empty(),
                    "StatusFilter::VisibleIn(empty) is caller misuse \u{2014} pass VisibleAll instead"
                );
                if statuses.is_empty() {
                    return Self::visible_status();
                }
                let mut any = Condition::any();
                for s in statuses {
                    any = any.add(tenants::Column::Status.eq(s.as_smallint()));
                }
                Condition::all().add(any).add(Self::visible_status())
            }
        }
    }
}

#[async_trait]
impl TenantHierarchyReadPort for TenantHierarchyReadAdapter {
    async fn get(&self, id: Uuid) -> Result<Option<TenantModel>, DomainError> {
        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let row = tenants::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(tenants::Column::Id.eq(id)))
            .filter(Self::visible_status())
            .one(&conn)
            .await
            .map_err(map_scope_err)?;
        match row {
            Some(r) => Ok(Some(entity_to_model(r)?)),
            None => Ok(None),
        }
    }

    async fn get_root(&self) -> Result<Vec<TenantModel>, DomainError> {
        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let rows = tenants::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Self::visible_status())
            .filter(Condition::all().add(tenants::Column::ParentId.is_null()))
            .order_by(tenants::Column::Id, Order::Asc)
            .limit(2u64)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        rows.into_iter().map(entity_to_model).collect()
    }

    async fn get_bulk(
        &self,
        ids: &[Uuid],
        filter: &StatusFilter,
    ) -> Result<Vec<TenantModel>, DomainError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut deduped: Vec<Uuid> = ids.to_vec();
        deduped.sort_unstable();
        deduped.dedup();

        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let rows = tenants::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(Condition::all().add(tenants::Column::Id.is_in(deduped)))
            .filter(Self::status_filter_to_condition(filter))
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        rows.into_iter().map(entity_to_model).collect()
    }

    async fn get_ancestors(
        &self,
        descendant_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<Vec<Uuid>, DomainError> {
        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let mut filter_cond = Condition::all()
            .add(tenant_closure::Column::DescendantId.eq(descendant_id))
            .add(tenant_closure::Column::AncestorId.ne(descendant_id));
        if matches!(barrier_mode, BarrierMode::Respect) {
            filter_cond = filter_cond.add(tenant_closure::Column::Barrier.eq(0_i16));
        }
        let rows = tenant_closure::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(filter_cond)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(rows.into_iter().map(|r| r.ancestor_id).collect())
    }

    async fn get_descendants(
        &self,
        ancestor_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<Vec<Uuid>, DomainError> {
        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let mut filter_cond = Condition::all()
            .add(tenant_closure::Column::AncestorId.eq(ancestor_id))
            .add(tenant_closure::Column::DescendantId.ne(ancestor_id));
        if matches!(barrier_mode, BarrierMode::Respect) {
            filter_cond = filter_cond.add(tenant_closure::Column::Barrier.eq(0_i16));
        }
        let rows = tenant_closure::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(filter_cond)
            .all(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(rows.into_iter().map(|r| r.descendant_id).collect())
    }

    async fn is_ancestor(
        &self,
        ancestor_id: Uuid,
        descendant_id: Uuid,
        barrier_mode: BarrierMode,
    ) -> Result<bool, DomainError> {
        let scope = Self::plugin_scope();
        let conn = self.db.conn()?;
        let mut filter_cond = Condition::all()
            .add(tenant_closure::Column::AncestorId.eq(ancestor_id))
            .add(tenant_closure::Column::DescendantId.eq(descendant_id));
        if matches!(barrier_mode, BarrierMode::Respect) {
            filter_cond = filter_cond.add(tenant_closure::Column::Barrier.eq(0_i16));
        }
        let count = tenant_closure::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(filter_cond)
            .count(&conn)
            .await
            .map_err(map_scope_err)?;
        Ok(count > 0)
    }
}
