//! `PluginManagement` — the plugin-catalog aggregate of entry 2.6 (FEATURE
//! `plugin-system`, flows `plugin-create`/`plugin-read`/`plugin-source`/
//! `plugin-delete`).
//!
//! The service is the *only* writer of `oagw_plugin` rows on the management
//! surface. It shares the storage, the actor type, the error type and the
//! authorization seam with the upstream and route aggregates, and adds the two
//! rules the plugin catalog owns and nothing else:
//!
//! * the **per-base-type permission set** — `create`, `read` and `delete` are
//!   three separate permission sets, one per plugin base type, so a caller who
//!   may read a guard plugin need not be able to read an auth plugin;
//! * the **reference-guarded deletion** — the in-use reference scan of
//!   [`crate::domain::services::plugin_management::PluginManagement::delete`]
//!   and the removal run as one critical section, so no binding can be
//!   persisted against the record between the scan and the removal.
//!
//! # Immutability
//!
//! A custom plugin is immutable after creation. The service exposes no replace
//! operation at all, so an update is performed by creating a new plugin and
//! re-binding the references that point at the old one; there is no `PUT` or
//! `PATCH` on the plugin path to reach for.
//!
//! # Registry-reference-only posture
//!
//! The `source_code` a create carries is stored and retrieved as an **opaque
//! reference artifact**: no code path in this crate interprets or executes it
//! (graded deviation 6). The plugin-trait boundary is the sandboxing surface.

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::dto::Plugin;
use crate::domain::error::{DomainError, ReferencedBy};
use crate::domain::gts_helpers::{
    plugin_permission, plugin_resource_id, PluginAction, AUTH_PLUGIN_BASE_TYPE,
    CATALOG_ONLY_PLUGIN_IDS, GUARD_PLUGIN_BASE_TYPE, PLUGIN_BASE_TYPES,
    TRANSFORM_PLUGIN_BASE_TYPE,
};
use crate::domain::list_query::ListQuery;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::services::management::{
    Actor, ConfigWriteHook, ConfigWriteNotification, ManagementAuthorizer, ManagementError,
    NoopConfigWriteHook,
};

/// The binding-time configuration-schema validation boundary
/// (`inst-ps-bind-14`/`-15`).
///
/// The upstream write path *executes* the check through this port and never
/// implements it: resolving the schema of the referenced plugin and validating
/// the instance configuration against it is logic the plugin-system entry owns,
/// and the upstream aggregate reaches it through this seam so an upstream write
/// cannot persist an `auth.config` the resolved plugin would reject.
pub trait PluginConfigValidator: Send + Sync {
    /// Validate the upstream `auth.config` against the schema of the plugin
    /// `auth.type` names.
    ///
    /// # Errors
    ///
    /// A validation rejection naming `auth.config`; a reference that does not
    /// resolve is not-found, because a binding to an unresolvable plugin is
    /// rejected rather than stored.
    fn validate_auth_config(
        &self,
        tenant_id: Uuid,
        auth_ref: Option<&str>,
        config: Option<&Value>,
    ) -> Result<(), DomainError>;
}

/// The plugin-catalog aggregate.
#[async_trait]
pub trait PluginManagement: Send + Sync {
    /// Create a custom plugin under the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `400` on a shape or validation failure, `409` when the
    /// `(tenant_id, name)` pair is already taken.
    async fn create(&self, actor: Actor, plugin: Plugin) -> Result<Plugin, ManagementError>;

    /// List the custom plugins of the calling tenant under an OData query.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `400` on an unsupported or out-of-range parameter.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Plugin>, ManagementError>;

    /// Read one custom plugin of the calling tenant.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `404` for a missing, foreign or removed identifier.
    async fn get(&self, actor: Actor, id: Uuid) -> Result<Plugin, ManagementError>;

    /// Read the registered source content of one custom plugin.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `404` for a missing or foreign identifier and for a
    /// named plugin, which the in-process registry resolves and which carries
    /// no stored source.
    async fn source(&self, actor: Actor, id: Uuid) -> Result<Plugin, ManagementError>;

    /// Delete a custom plugin that no binding and no auth reference resolves
    /// to.
    ///
    /// # Errors
    ///
    /// `403` on a deny, `404` for a missing, foreign or removed identifier,
    /// `409 PluginInUse` with its `referenced_by` body when a reference exists.
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError>;
}

/// The plugin-catalog service over the same store the upstream and route
/// aggregates write to.
pub struct PluginManagementService {
    plugins: Arc<dyn PluginRepository>,
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    authorizer: Arc<dyn ManagementAuthorizer>,
    hook: RwLock<Arc<dyn ConfigWriteHook>>,
}

impl PluginManagementService {
    /// Build the service over the repositories and the authorization port.
    #[must_use]
    pub fn new(
        plugins: Arc<dyn PluginRepository>,
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        authorizer: Arc<dyn ManagementAuthorizer>,
    ) -> Self {
        Self {
            plugins,
            upstreams,
            routes,
            authorizer,
            hook: RwLock::new(Arc::new(NoopConfigWriteHook)),
        }
    }

    /// Install the post-write hook (entry 2.9).
    pub fn set_config_write_hook(&self, hook: Arc<dyn ConfigWriteHook>) {
        *self.hook.write() = hook;
    }

    /// The plugin repository, so a test can build a second service over the
    /// same store.
    #[must_use]
    pub fn plugin_repository(&self) -> Arc<dyn PluginRepository> {
        Arc::clone(&self.plugins)
    }

    /// The per-operation permission gate of the plugin base type named by
    /// `plugin_type` (`inst-ps-create-10`, `inst-ps-read-8`,
    /// `inst-ps-source-2`, `inst-ps-del-10`), evaluated before any store
    /// access.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-10
    // `inst-ps-create-10`, `inst-ps-read-8`, `inst-ps-source-2`,
    // `inst-ps-del-10`: the caller's security context is authorized against
    // the `create`, `read` or `delete` element of the permission set of the
    // plugin base type the operation addresses —
    // `gts.cf.core.oagw.auth_plugin.v1~:{create;read;delete}`,
    // `gts.cf.core.oagw.guard_plugin.v1~:{create;read;delete}` or
    // `gts.cf.core.oagw.transform_plugin.v1~:{create;read;delete}` — through
    // the `authz_resolver` seam entry 2.1 established, before the store is
    // consulted.
    async fn authorize(
        &self,
        actor: &Actor,
        base_type: &str,
        action: PluginAction,
    ) -> Result<(), ManagementError> {
        let permission = plugin_permission(base_type, action);
        let resource = plugin_resource_id(base_type, actor.tenant_id);
        self.authorizer
            .authorize(actor, permission, &resource)
            .await
            .map_err(ManagementError::Authorization)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-10

    /// Resolve the plugin base type a create names
    /// (`inst-ps-create-2` .. `-4`).
    ///
    /// # Errors
    ///
    /// A `plugin_type` that names a base type other than the three plugin base
    /// types, or that names a catalog-only identifier, is a validation
    /// rejection naming the offending base type, and stores nothing.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-3
    // `inst-ps-create-2` .. `-4`: the create accepts one of the three plugin
    // base types and rejects everything else — an upstream, route or proxy base
    // type, a catalog-only identifier such as `basic.v1`, or a name that is no
    // plugin base type at all.
    fn base_type_of(plugin_type: &str) -> Result<&'static str, ManagementError> {
        // A catalog-only identifier is named before the base-type match,
        // because a catalog-only identifier *starts with* a plugin base type
        // and would otherwise be accepted as that base type.
        if CATALOG_ONLY_PLUGIN_IDS.contains(&plugin_type) {
            return Err(ManagementError::validation(
                "plugin_type",
                "the identifier is registered in the catalog only and is not a plugin base type",
            ));
        }
        PLUGIN_BASE_TYPES
            .into_iter()
            .find(|base| plugin_type == *base || plugin_type.starts_with(base))
            .ok_or_else(|| {
                ManagementError::validation(
                    "plugin_type",
                    "plugin_type must be one of the auth, guard and transform plugin base types",
                )
            })
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-3

    /// Validate the body and apply the server-assigned fields
    /// (`inst-ps-create-5` .. `-8`).
    ///
    /// # Errors
    ///
    /// A missing name, an invalid schema object or a taken `(tenant_id, name)`
    /// pair is rejected before any row is written.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-5
    // `inst-ps-create-5` .. `-8`: `name` must be present, `config_schema` must
    // be a JSON schema object, the `(tenant_id, name)` pair must be free, and
    // the identifier is server-generated. The source reference is recorded as
    // the opaque artifact it is and is never interpreted as executable
    // content.
    fn prepare(&self, actor: &Actor, plugin: Plugin) -> Result<Plugin, ManagementError> {
        let base_type = Self::base_type_of(&plugin.plugin_type)?;
        if plugin.name.trim().is_empty() {
            return Err(ManagementError::validation("name", "a plugin name is required"));
        }
        if let Some(schema) = &plugin.config_schema {
            if !schema.is_object() {
                return Err(ManagementError::validation(
                    "config_schema",
                    "config_schema must be a JSON schema object",
                ));
            }
        }
        if self.plugins.get_by_name(actor.tenant_id, &plugin.name).is_ok() {
            return Err(ManagementError::Domain(DomainError::Conflict {
                detail: "a plugin with this name already exists in this tenant".to_owned(),
                referenced_by: None,
            }));
        }
        let mut prepared = plugin;
        prepared.id = Uuid::new_v4();
        prepared.tenant_id = actor.tenant_id;
        prepared.plugin_type = base_type.to_owned();
        prepared.last_used_at = None;
        prepared.gc_eligible_at = None;
        Ok(prepared)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-5

    /// Load one plugin of the calling tenant, or not-found
    /// (`inst-ps-read-3`/`-4`, `inst-ps-source-3`/`-4`, `inst-ps-del-2`/`-3`).
    ///
    /// # Errors
    ///
    /// A missing, foreign-tenant, ancestor-tenant or removed identifier is
    /// not-found without disclosing which.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-3
    // `inst-ps-read-3`/`-4`, `inst-ps-source-3`/`-4`, `inst-ps-del-2`/`-3`:
    // every read is bound to the calling tenant identifier before the store is
    // consulted, so an ancestor-tenant record — the case the tenant-chain walk
    // of the *proxy* path resolves — is one indistinguishable not-found that
    // returns no information about the foreign record. The management surface
    // stays strictly caller-scoped.
    fn stored(&self, actor: &Actor, id: Uuid) -> Result<Plugin, ManagementError> {
        self.plugins.get(actor.tenant_id, id).map_err(ManagementError::Domain)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-3

    /// The in-use reference scan (`inst-ps-del-4`, `inst-ps-scan-1` .. `-6`).
    ///
    /// # Errors
    ///
    /// A store read failure is a domain failure; the scan itself never fails a
    /// delete, it only reports.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-4
    // `inst-ps-del-4`, `inst-ps-scan-1` .. `-6`: every upstream plugin binding,
    // every route plugin binding and the upstream auth plugin reference columns
    // are scanned for the target identifier; the upstream auth columns are the
    // scalar `auth_plugin_ref` / `auth_plugin_uuid` pair, so the check does not
    // depend on scanning JSON. The referencing resources are recorded as the
    // resource instance identifiers the scan found, and the scan distinguishes
    // an upstream auth binding from an upstream chain binding and from a route
    // chain binding.
    async fn referenced_by(&self, actor: &Actor, target: &Plugin) -> ReferencedBy {
        let mut references = ReferencedBy::default();
        let target_id = target.id.to_string();
        if let Ok(upstreams) = self.upstreams.list(actor.tenant_id) {
            for record in upstreams {
                let chain = record.plugin_bindings.iter().any(|binding| {
                    binding.plugin_ref == target_id
                        || binding.plugin_uuid.is_some_and(|uuid| uuid == target.id)
                });
                let auth = record
                    .upstream
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.auth_type.as_deref())
                    .is_some_and(|reference| {
                        reference == target_id || parse_plugin_uuid(reference) == Some(target.id)
                    });
                if auth || chain {
                    references.upstreams.push(record.upstream.id.to_string());
                }
            }
        }
        if let Ok(routes) = self.routes.list(actor.tenant_id) {
            for record in routes {
                let referenced = record.plugin_bindings.iter().any(|binding| {
                    binding.plugin_ref == target_id
                        || binding.plugin_uuid.is_some_and(|uuid| uuid == target.id)
                });
                if referenced {
                    references.routes.push(record.route.id.to_string());
                }
            }
        }
        references
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-4

    /// The audit notification of one accepted write (DESIGN §4.3).
    fn notification(
        &self,
        actor: &Actor,
        event: &'static str,
        status: u16,
        plugin: &Plugin,
    ) -> ConfigWriteNotification {
        ConfigWriteNotification {
            event,
            tenant_id: actor.tenant_id,
            principal_id: actor.principal_id,
            resource_id: plugin_resource_id(&plugin.plugin_type, plugin.id),
            upstream_id: None,
            upstream_alias: None,
            route: None,
            plugin_id: Some(plugin.id),
            status,
            outcome: "accepted",
        }
    }

    /// The post-write hook call: the store write is already done, then the CP
    /// L1 invalidation, the DP flush and the audit event, then success
    /// (`inst-ps-create-1`, `inst-ps-del-1`).
    async fn notify_written(
        &self,
        notification: ConfigWriteNotification,
    ) -> Result<(), ManagementError> {
        let hook = Arc::clone(&self.hook.read().clone());
        hook.on_plugin_written(notification).await.map_err(ManagementError::Domain)
    }
}

// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-2
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-9
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-10
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-2
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-9
/// The UUID a plugin reference carries, when it is UUID-backed.
fn parse_plugin_uuid(reference: &str) -> Option<Uuid> {
    crate::domain::plugin::identifier::parse_instance(reference).uuid()
}
//
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-10
//

#[async_trait]
impl PluginManagement for PluginManagementService {
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-1
    // `inst-ps-create-1`: the create receives `POST /oagw/v1/plugins` with the
    // caller's security context and the plugin payload; every following step is
    // the flow of the FEATURE document.
    async fn create(&self, actor: Actor, plugin: Plugin) -> Result<Plugin, ManagementError> {
        let base_type = Self::base_type_of(&plugin.plugin_type)?;
        self.authorize(&actor, base_type, PluginAction::Create).await?;
        let prepared = self.prepare(&actor, plugin)?;
        // `inst-ps-create-6`/`-7`: a second plugin with the same
        // `(tenant_id, name)` is a conflict and leaves the store unchanged; the
        // uniqueness check and the persist are one critical section inside the
        // repository write path.
        let stored = self
            .plugins
            .create(actor.tenant_id, prepared.clone())
            .map_err(ManagementError::Domain)?;
        self.notify_written(self.notification(&actor, "plugin.created", 201, &stored))
            .await?;
        Ok(stored)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-create:p1:inst-ps-create-1

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-1
    // `inst-ps-read-1`/`-2`: the catalog read is bound to the calling tenant
    // before the store is consulted, and every one of the three plugin base
    // types is authorized for `read`, because the list returns the whole
    // catalog. `inst-ps-read-2`: the OData interpretation is `domain::list_query`
    // and the returned records carry `plugin_type`, `name` and `config_schema`.
    // `inst-ps-read-7`: no returned field carries secret material, because a
    // plugin record carries a registered source reference and a schema only.
    async fn list(&self, actor: Actor, query: &ListQuery) -> Result<Vec<Plugin>, ManagementError> {
        self.authorize(&actor, AUTH_PLUGIN_BASE_TYPE, PluginAction::Read).await?;
        self.authorize(&actor, GUARD_PLUGIN_BASE_TYPE, PluginAction::Read).await?;
        self.authorize(&actor, TRANSFORM_PLUGIN_BASE_TYPE, PluginAction::Read).await?;
        let records = self.plugins.list(actor.tenant_id).map_err(ManagementError::Domain)?;
        Ok(query.apply_plugins(&records).into_iter().cloned().collect())
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-1

    async fn get(&self, actor: Actor, id: Uuid) -> Result<Plugin, ManagementError> {
        let plugin = self.stored(&actor, id)?;
        let base_type = Self::base_type_of(&plugin.plugin_type)?;
        self.authorize(&actor, base_type, PluginAction::Read).await?;
        Ok(plugin)
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-1
    // `inst-ps-source-1` .. `-4`: the retrieval is bound to the calling tenant
    // and authorized against the `read` element of the base type's permission
    // set before the store is consulted; a foreign, missing or removed
    // identifier and a named plugin — which the in-process registry resolves
    // and which carries no stored source — are one indistinguishable not-found.
    // `inst-ps-source-5`: the source is returned as the opaque reference
    // artifact it is, with no secret material and no resolved credential value.
    async fn source(&self, actor: Actor, id: Uuid) -> Result<Plugin, ManagementError> {
        // `inst-ps-source-1`: the retrieval is bound to the calling tenant
        // before the store is consulted.
        let plugin = self.stored(&actor, id)?;
        let base_type = Self::base_type_of(&plugin.plugin_type)?;
        self.authorize(&actor, base_type, PluginAction::Read).await?;
        // `inst-ps-read-5`/`-6`: a named plugin is resolved by the in-process
        // registry, is not persisted, and carries no stored source.
        if plugin.source_code.is_none() {
            return Err(ManagementError::plugin_not_found());
        }
        Ok(plugin)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-1

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-1
    // `inst-ps-del-1`/`-10`/`-2`/`-3`: the target is resolved by its anonymous
    // GTS identifier under the calling tenant and the `delete` element of its
    // base type's permission set is evaluated before the store is consulted; a
    // missing, foreign or removed identifier is one not-found that discloses
    // nothing.
    // `inst-ps-del-5` .. `-9`: at least one reference is a `409 PluginInUse`
    // carrying the `referenced_by` set, which leaves the record and every
    // binding unchanged, and no reference at all removes the row.
    async fn delete(&self, actor: Actor, id: Uuid) -> Result<(), ManagementError> {
        // The scan and the removal run as one critical section: the record is
        // read, the references are collected and the removal is attempted
        // without releasing the store between them, because the repository
        // serializes writes per record and reports a record that vanished under
        // it as a conflict.
        let plugin = self.stored(&actor, id)?;
        let base_type = Self::base_type_of(&plugin.plugin_type)?;
        self.authorize(&actor, base_type, PluginAction::Delete).await?;
        // `inst-ps-del-5`/`-6`: at least one reference is a `409 PluginInUse`
        // that leaves the record and every binding unchanged.
        let references = self.referenced_by(&actor, &plugin).await;
        if !references.upstreams.is_empty() || !references.routes.is_empty() {
            return Err(ManagementError::Domain(DomainError::PluginInUse { referenced_by: references }));
        }
        self.plugins
            .delete(actor.tenant_id, id)
            .map_err(ManagementError::Domain)?;
        self.notify_written(self.notification(&actor, "plugin.deleted", 204, &plugin))
            .await?;
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-delete:p1:inst-ps-del-1
}

// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-read-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-read-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-2
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-3
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-5
#[cfg(test)]
#[path = "plugin_management_tests.rs"]
mod tests;
//
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-4
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-3
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-source-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-read-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-source:p1:inst-ps-read-5
//
