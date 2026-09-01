//! `SeaORM` entity for the AM-owned `tenant_idp_metadata` table.
//!
//! Plugin-private per-tenant state isolated from public `tenant_metadata`.
//!
//! AM persists the opaque blob returned from
//! [`account_management_sdk::IdpProvisionResult::metadata`] keyed by
//! `tenant_id` (one row per tenant) and replays it on every subsequent
//! `IdpPluginClient` call for that tenant via
//! [`account_management_sdk::IdpTenantContext::metadata`] and
//! [`account_management_sdk::IdpDeprovisionTenantRequest::tenant_context`].
//!
//! AM does NOT validate, namespace, or interpret the JSON — the plugin
//! owns the shape entirely. Size is capped at the AM service boundary
//! by `MAX_IDP_METADATA_BYTES`.
//!
//! Single tenant_id-keyed row — AM resolves at most one
//! `IdpPluginClient` via `ClientHub`.

use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db_macros::Scopable;
use uuid::Uuid;

// @cpt-begin:cpt-cf-account-management-dbtable-tenant-idp-metadata:p1:inst-dbtable-tenant-idp-metadata-entity
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "tenant_idp_metadata")]
#[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub tenant_id: Uuid,
    /// Opaque JSON blob owned and shaped by the plugin. `NULL` is the
    /// "plugin returned no per-tenant state" path.
    pub metadata: Option<Json>,
    pub updated_at: OffsetDateTime,
}
// @cpt-end:cpt-cf-account-management-dbtable-tenant-idp-metadata:p1:inst-dbtable-tenant-idp-metadata-entity

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
