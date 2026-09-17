//! Control-plane implementation over the in-memory store (DESIGN §3.3).
//!
//! Every operation is tenant-scoped: a tenant only ever reads or mutates its
//! own records. Mutations bump the config generation so the data plane drops
//! stale L1 cache entries.


use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::OagwError;
use crate::domain::error::OagwResult;
use crate::domain::gts_helpers;
use crate::domain::model::{Plugin, PluginInput, PluginType, Route, RouteInput, Upstream, UpstreamInput};
use crate::domain::services::management::{ControlPlaneService, ListQuery, PluginSource};
use crate::domain::services::proxy::{ResolvedTarget, TargetHostChoice};
use crate::domain::validation;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::infra::plugin::builtin_plugin_ids;
use crate::infra::storage::memory::MemoryStore;

/// Control plane backed by [`MemoryStore`].
#[derive(Clone)]
pub struct ManagementService {
    store: std::sync::Arc<MemoryStore>,
    data_plane: Option<std::sync::Arc<crate::infra::proxy::service::DataPlane>>,
    allow_http: bool,
}

impl std::fmt::Debug for ManagementService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementService")
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl ManagementService {
    /// Builds the control plane.
    ///
    /// `data_plane` supplies the plugin catalogue and alias resolution; it is
    /// optional so the control plane can be exercised standalone in tests.
    #[must_use]
    pub fn new(
        store: std::sync::Arc<MemoryStore>,
        data_plane: Option<std::sync::Arc<crate::infra::proxy::service::DataPlane>>,
        allow_http: bool,
    ) -> Self {
        Self {
            store,
            data_plane,
            allow_http,
        }
    }

    fn data_plane(&self) -> OagwResult<&std::sync::Arc<crate::infra::proxy::service::DataPlane>> {
        self.data_plane
            .as_ref()
            .ok_or_else(|| OagwError::Internal("data plane is not attached".to_owned()))
    }
}

/// Epoch milliseconds now.
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Builds the custom-plugin GTS id from its tenant and uuid.
#[must_use]
pub fn custom_plugin_id(plugin_type: PluginType, id: uuid::Uuid) -> String {
    format!("{}{id}", plugin_type.prefix())
}

/// OData-ish ordering used by all three list endpoints.
///
/// Only the documented sortable fields are honoured; anything else leaves the
/// natural (insertion) order untouched.
fn sort_by_orderby<T: Filterable>(items: &mut [T], orderby: Option<&String>) {
    let Some(orderby) = orderby else {
        return;
    };
    let expr = orderby.trim().to_ascii_lowercase();
    let (field, descending) = match expr.split_once(' ') {
        Some(("created_at", "desc")) => ("created_at", true),
        Some(("created_at", "asc")) | Some(("created_at", _)) => ("created_at", false),
        Some(("updated_at", "desc")) => ("updated_at", true),
        Some(("updated_at", "asc")) | Some(("updated_at", _)) => ("updated_at", false),
        Some((field, "desc")) => (field, true),
        Some((field, "asc")) | Some((field, _)) => (field, false),
        None => ("", false),
    };
    if !matches!(field, "created_at" | "updated_at" | "name" | "alias") {
        return;
    }
    items.sort_by(|a, b| {
        let (ka, kb) = (a.sort_value(field), b.sort_value(field));
        if descending {
            kb.cmp(&ka)
        } else {
            ka.cmp(&kb)
        }
    });
}

#[async_trait]
impl ControlPlaneService for ManagementService {
    async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> OagwResult<Upstream> {
        validation::validate_upstream(&input, self.allow_http)?;
        let tenant_id = ctx.subject_tenant_id();
        let resolved = alias::resolve_alias(&input)?;
        if self
            .store
            .find_upstream_by_alias(tenant_id, &resolved)?
            .is_some()
        {
            return Err(OagwError::AliasConflict(format!(
                "alias '{resolved}' is already used in this tenant"
            )));
        }
        let now = now_millis();
        let upstream = Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            enabled: input.enabled,
            alias: resolved,
            alias_explicit: input.alias.is_some(),
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol.clone(),
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            created_at: now,
            updated_at: now,
        };
        self.store.insert_upstream(upstream.clone())?;
        Ok(upstream)
    }

    async fn list_upstreams(&self, ctx: &SecurityContext, query: &ListQuery) -> OagwResult<Vec<Upstream>> {
        let mut items = self.store.upstreams_for_tenant(ctx.subject_tenant_id())?;
        sort_by_orderby(&mut items, query.orderby.as_ref());
        let selected = apply_filter(&items, query.filter.as_deref())?;
        Ok(selected
            .into_iter()
            .skip(query.offset())
            .take(query.limit())
            .collect())
    }

    async fn get_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Upstream> {
        let upstream = self
            .store
            .find_upstream(id)?
            .ok_or_else(|| OagwError::RouteNotFound("record not found for this tenant".to_owned()))?;
        require_owner(upstream.tenant_id, ctx)?;
        Ok(upstream)
    }

    async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: UpstreamInput,
    ) -> OagwResult<Upstream> {
        let existing = self.get_upstream(ctx, id).await?;
        validation::validate_upstream(&input, self.allow_http)?;
        let resolved = alias::resolve_alias(&input)?;
        if let Some(other) = self.store.find_upstream_by_alias(existing.tenant_id, &resolved)? {
            if other.id != id {
                return Err(OagwError::AliasConflict(format!(
                    "alias '{resolved}' is already used in this tenant"
                )));
            }
        }
        let upstream = Upstream {
            id,
            tenant_id: existing.tenant_id,
            enabled: input.enabled,
            alias: resolved,
            alias_explicit: input.alias.is_some(),
            tags: input.tags.clone(),
            server: input.server.clone(),
            protocol: input.protocol.clone(),
            auth: input.auth.clone(),
            headers: input.headers.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            cors: input.cors.clone(),
            created_at: existing.created_at,
            updated_at: now_millis(),
        };
        self.store.update_upstream(upstream.clone())?;
        Ok(upstream)
    }

    async fn delete_upstream(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        let existing = self.get_upstream(ctx, id).await?;
        if self.store.upstream_route_count(id)? > 0 {
            return Err(OagwError::RouteConflict(format!(
                "upstream {id} still has routes"
            )));
        }
        let _ = existing;
        self.store.delete_upstream(id)
    }

    async fn create_route(&self, ctx: &SecurityContext, input: RouteInput) -> OagwResult<Route> {
        validation::validate_route(&input)?;
        let tenant_id = ctx.subject_tenant_id();
        let upstream = self
            .store
            .find_upstream(input.upstream_id)?
            .filter(|upstream| upstream.tenant_id == tenant_id)
            .ok_or_else(|| OagwError::RouteNotFound("record not found for this tenant".to_owned()))?;
        if self.conflicting_route(tenant_id, &input).is_some() {
            return Err(OagwError::RouteConflict(
                "another route already matches this upstream".to_owned(),
            ));
        }
        let _ = upstream;
        let now = now_millis();
        let route = Route {
            id: uuid::Uuid::new_v4(),
            tenant_id,
            tags: input.tags.clone(),
            upstream_id: input.upstream_id,
            r#match: input.r#match.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            created_at: now,
            updated_at: now,
        };
        self.store.insert_route(route.clone())?;
        Ok(route)
    }

    async fn list_routes(&self, ctx: &SecurityContext, query: &ListQuery) -> OagwResult<Vec<Route>> {
        let mut items = self.store.routes_for_tenant(ctx.subject_tenant_id())?;
        sort_by_orderby(&mut items, query.orderby.as_ref());
        let selected = apply_filter(&items, query.filter.as_deref())?;
        Ok(selected
            .into_iter()
            .skip(query.offset())
            .take(query.limit())
            .collect())
    }

    async fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<Route> {
        let route = self
            .store
            .find_route(id)?
            .ok_or_else(|| OagwError::RouteNotFound("record not found for this tenant".to_owned()))?;
        require_owner(route.tenant_id, ctx)?;
        Ok(route)
    }

    async fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        input: RouteInput,
    ) -> OagwResult<Route> {
        let existing = self.get_route(ctx, id).await?;
        validation::validate_route(&input)?;
        if input.upstream_id != existing.upstream_id {
            return Err(OagwError::Validation(
                "upstream_id is immutable on routes".to_owned(),
            ));
        }
        if let Some(other) = self.conflicting_route(existing.tenant_id, &input) {
            if other != id {
                return Err(OagwError::RouteConflict(
                    "another route already matches this upstream".to_owned(),
                ));
            }
        }
        let route = Route {
            id,
            tenant_id: existing.tenant_id,
            tags: input.tags.clone(),
            upstream_id: existing.upstream_id,
            r#match: input.r#match.clone(),
            plugins: input.plugins.clone(),
            rate_limit: input.rate_limit.clone(),
            created_at: existing.created_at,
            updated_at: now_millis(),
        };
        self.store.update_route(route.clone())?;
        Ok(route)
    }

    async fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> OagwResult<()> {
        self.get_route(ctx, id).await?;
        self.store.delete_route(id)
    }

    async fn create_plugin(&self, ctx: &SecurityContext, input: PluginInput) -> OagwResult<Plugin> {
        validation::validate_plugin(&input)?;
        let tenant_id = ctx.subject_tenant_id();
        let now = now_millis();
        let id = uuid::Uuid::new_v4();
        let plugin = Plugin {
            id: custom_plugin_id(input.plugin_type, id),
            tenant_id,
            name: input.name.clone(),
            description: input.description.clone(),
            plugin_type: input.plugin_type,
            config_schema: input.config_schema.clone(),
            config: input.config.clone(),
            source_code: input.source_code.clone(),
            created_at: now,
            updated_at: now,
        };
        self.store.insert_plugin(plugin.clone())?;
        Ok(plugin)
    }

    async fn list_plugins(&self, ctx: &SecurityContext, query: &ListQuery) -> OagwResult<Vec<Plugin>> {
        let mut custom = self.store.plugins_for_tenant(ctx.subject_tenant_id())?;
        let mut items = builtin_plugins();
        items.append(&mut custom);
        sort_by_orderby(&mut items, query.orderby.as_ref());
        let selected = apply_filter(&items, query.filter.as_deref())?;
        Ok(selected
            .into_iter()
            .skip(query.offset())
            .take(query.limit())
            .collect())
    }

    async fn get_plugin(&self, ctx: &SecurityContext, id: &str) -> OagwResult<Plugin> {
        if let Some(plugin) = builtin_plugin(id) {
            return Ok(plugin);
        }
        let plugin = self
            .store
            .find_plugin(id)?
            .ok_or_else(|| OagwError::PluginNotFound(id.to_owned()))?;
        require_owner(plugin.tenant_id, ctx)?;
        Ok(plugin)
    }

    async fn delete_plugin(&self, ctx: &SecurityContext, id: &str) -> OagwResult<()> {
        let plugin = self.get_plugin(ctx, id).await?;
        if is_builtin(plugin.id.as_str()) {
            return Err(OagwError::Validation(
                "built-in plugins cannot be deleted".to_owned(),
            ));
        }
        let in_use = self.in_use(id)?;
        if in_use {
            return Err(OagwError::PluginInUse(id.to_owned()));
        }
        self.store.delete_plugin(id)
    }

    async fn get_plugin_source(&self, ctx: &SecurityContext, id: &str) -> OagwResult<PluginSource> {
        let plugin = self.get_plugin(ctx, id).await?;
        if is_builtin(plugin.id.as_str()) {
            return Ok(PluginSource {
                plugin_id: plugin.id.clone(),
                plugin_type: plugin.plugin_type.prefix().to_owned(),
                language: "rust",
                source: format!(
                    "// built-in {} plugin, compiled into cf-gears-oagw",
                    plugin.name
                ),
                config: plugin.config,
            });
        }
        Ok(PluginSource {
            plugin_id: plugin.id.clone(),
            plugin_type: plugin.plugin_type.prefix().to_owned(),
            language: "starlark",
            source: plugin.source_code.clone(),
            config: plugin.config,
        })
    }

    async fn resolve_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        choice: &TargetHostChoice,
    ) -> OagwResult<ResolvedTarget> {
        self.data_plane()?.resolve_target(ctx, alias, choice).await
    }
}

impl ManagementService {
    /// `Some(id)` when another route on the same upstream matches the same
    /// method/path pair.
    fn conflicting_route(&self, tenant_id: uuid::Uuid, input: &RouteInput) -> Option<Uuid> {
        let Some(http) = &input.r#match.http else {
            return None;
        };
        self.store
            .routes_for_tenant(tenant_id)
            .ok()?
            .into_iter()
            .filter(|route| route.upstream_id == input.upstream_id)
            .find(|route| {
                route.r#match.http.as_ref().map_or(false, |existing| {
                    existing.path == http.path
                        && existing
                            .methods
                            .iter()
                            .any(|method| http.methods.contains(method))
                })
            })
            .map(|route| route.id)
    }

    /// `true` when any upstream or route references `plugin_id`.
    fn in_use(&self, plugin_id: &str) -> OagwResult<bool> {
        let upstreams = self.store.with_upstreams(|map| {
            map.values()
                .filter(|upstream| {
                    upstream
                        .plugins
                        .as_ref()
                        .map_or(false, |plugins| {
                            plugins.items.iter().any(|binding| binding.matches(plugin_id))
                        })
                })
                .count()
        });
        let routes = self.store.with_routes(|map| {
            map.values()
                .filter(|route| {
                    route.plugins.items.iter().any(|binding| binding.matches(plugin_id))
                })
                .count()
        });
        Ok(upstreams + routes > 0)
    }
}

/// `Err` when the record belongs to a different tenant.
fn require_owner(owner: Uuid, ctx: &SecurityContext) -> OagwResult<()> {
    if owner != ctx.subject_tenant_id() {
        return Err(OagwError::RouteNotFound(
            "record not found for this tenant".to_owned(),
        ));
    }
    Ok(())
}

/// `true` for built-in plugin ids (catalogued or implemented).
///
/// A custom plugin shares the `..._plugin.v1~` namespace with the built-ins
/// but is identified by a bare UUID instance (DESIGN "Custom Plugins"), so it
/// must not be classified as built-in — otherwise it could never be deleted.
#[must_use]
pub fn is_builtin(id: &str) -> bool {
    if builtin_plugin_ids().contains(&id.to_owned()) {
        return true;
    }
    // UUID-backed instance: a custom plugin row, not a named built-in.
    gts_helpers::resource_uuid(id).is_none() && gts_helpers::plugin_kind(id).is_some()
}

/// Catalogue entries for the built-in plugins.
#[must_use]
pub fn builtin_plugins() -> Vec<Plugin> {
    let descriptions: &[(&str, &str, PluginType)] = &[
        (
            gts_helpers::NOOP_AUTH_PLUGIN_ID,
            "No credential injection; passes the caller's headers upstream",
            PluginType::Auth,
        ),
        (
            gts_helpers::APIKEY_AUTH_PLUGIN_ID,
            "API-key credential injection from the credential store",
            PluginType::Auth,
        ),
        (
            gts_helpers::OAUTH2_CLIENT_CRED_PLUGIN_ID,
            "OAuth2 client-credentials grant (form-encoded POST)",
            PluginType::Auth,
        ),
        (
            gts_helpers::OAUTH2_CLIENT_CRED_BASIC_PLUGIN_ID,
            "OAuth2 client-credentials grant (HTTP Basic auth)",
            PluginType::Auth,
        ),
        (
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            "Rejects requests missing required headers",
            PluginType::Guard,
        ),
        (
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID,
            "Adds an x-request-id header to proxied requests",
            PluginType::Transform,
        ),
    ];
    descriptions
        .iter()
        .map(|(id, description, plugin_type)| Plugin {
            id: (*id).to_owned(),
            tenant_id: Uuid::nil(),
            name: builtin_name(id),
            description: (*description).to_owned(),
            plugin_type: *plugin_type,
            config_schema: serde_json::Value::Object(serde_json::Map::new()),
            config: serde_json::Value::Null,
            source_code: String::new(),
            created_at: 0,
            updated_at: 0,
        })
        .collect()
}

fn builtin_name(id: &str) -> String {
    id.rsplit('~').next().unwrap_or(id).to_owned()
}

/// One built-in plugin as a catalogue record.
#[must_use]
pub fn builtin_plugin(id: &str) -> Option<Plugin> {
    builtin_plugins().into_iter().find(|plugin| plugin.id == id)
}

/// Minimal `$filter` support: `name eq 'value'` on the record's string key.
fn apply_filter<T: Filterable + Clone>(items: &[T], filter: Option<&str>) -> OagwResult<Vec<T>> {
    let Some(filter) = filter else {
        return Ok(items.to_vec());
    };
    let expr = filter.trim();
    let (field, value) = match expr.split_once(" eq ") {
        Some((field, value)) => (field.trim(), value.trim().trim_matches('\'')),
        None => {
            return Err(OagwError::Validation(format!(
                "unsupported $filter expression '{expr}'"
            )))
        }
    };
    Ok(items
        .iter()
        .filter(|item| item.matches(field, value))
        .cloned()
        .collect())
}

/// Records that support the `$filter` / `$orderby` subset OAGW implements.
trait Filterable {
    /// Field comparison used by `$filter`.
    fn matches(&self, field: &str, value: &str) -> bool;
    /// Comparable value used by `$orderby`.
    fn sort_value(&self, field: &str) -> String;
}

impl Filterable for Upstream {
    fn matches(&self, field: &str, value: &str) -> bool {
        match field {
            "alias" => self.alias == value,
            "id" => self.id.to_string() == value,
            "protocol" => self.protocol == value,
            "enabled" => value == "true" && self.enabled,
            _ => false,
        }
    }
    fn sort_value(&self, field: &str) -> String {
        match field {
            "created_at" => self.created_at.to_string(),
            "updated_at" => self.updated_at.to_string(),
            "alias" => self.alias.clone(),
            _ => String::new(),
        }
    }
}

impl Filterable for Route {
    fn matches(&self, field: &str, value: &str) -> bool {
        match field {
            "id" => self.id.to_string() == value,
            "upstream_id" => self.upstream_id.to_string() == value,
            _ => false,
        }
    }
    fn sort_value(&self, field: &str) -> String {
        match field {
            "created_at" => self.created_at.to_string(),
            "updated_at" => self.updated_at.to_string(),
            _ => String::new(),
        }
    }
}

impl Filterable for Plugin {
    fn matches(&self, field: &str, value: &str) -> bool {
        match field {
            "id" => self.id == value,
            "name" => self.name == value,
            "type" => self.plugin_type.prefix() == value,
            _ => false,
        }
    }
    fn sort_value(&self, field: &str) -> String {
        match field {
            "created_at" => self.created_at.to_string(),
            "updated_at" => self.updated_at.to_string(),
            "name" => self.name.clone(),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_plugin_ids_use_the_type_prefix() {
        // DESIGN "Custom Plugins": `...{type}_plugin.v1~{uuid}`, one `~`.
        let id = uuid::Uuid::new_v4();
        let built = custom_plugin_id(PluginType::Transform, id);
        assert_eq!(built, format!("{}{id}", gts_helpers::TRANSFORM_PLUGIN_PREFIX));
        assert_eq!(gts_helpers::plugin_kind(&built), Some("transform"));
    }

    #[test]
    fn custom_plugins_are_not_builtins() {
        let id = uuid::Uuid::new_v4();
        let custom = custom_plugin_id(PluginType::Guard, id);
        assert!(!is_builtin(&custom), "a UUID-backed plugin must be deletable");
        // Catalogued-but-unimplemented ids are still built-ins.
        assert!(is_builtin(gts_helpers::CATALOG_BASIC_AUTH_PLUGIN_ID));
        assert!(is_builtin(gts_helpers::CATALOG_TIMEOUT_GUARD_PLUGIN_ID));
    }

    #[test]
    fn builtins_are_recognised() {
        assert!(is_builtin(gts_helpers::APIKEY_AUTH_PLUGIN_ID));
        assert!(is_builtin(
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID
        ));
        assert!(!is_builtin("not-a-plugin"));
    }

    #[test]
    fn builtin_catalogue_is_complete() {
        let plugins = builtin_plugins();
        assert!(plugins.len() >= 6);
        assert!(plugins.iter().all(|plugin| !plugin.name.is_empty()));
    }
}
