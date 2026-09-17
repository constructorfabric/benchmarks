//! Control-plane services — tenant-scoped CRUD for upstreams, routes
//! and plugins.
//!
//! All operations are strictly scoped to the calling tenant: ancestor
//! resources are invisible (404) via the management API. Upstream
//! alias derivation / update rules and route match-uniqueness are
//! enforced here; the data plane reads the same store.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tracing::warn;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias;
use crate::domain::error::OagwError;
use crate::domain::model::{
    PluginCreateRequest, PluginKind, PluginRecord, RouteRecord, RouteRequest, RouteUpdateRequest,
    UpstreamAuth, UpstreamRecord, UpstreamRequest, created_now,
};
use crate::infra::storage::MemoryStore;

/// Permission identifiers (token scopes) guarding the management API.
pub const SCOPE_UPSTREAM_CREATE: &str = "gts.cf.core.oagw.upstream.v1~:create";
pub const SCOPE_UPSTREAM_OVERRIDE: &str = "gts.cf.core.oagw.upstream.v1~:override";
pub const SCOPE_UPSTREAM_READ: &str = "gts.cf.core.oagw.upstream.v1~:read";
pub const SCOPE_UPSTREAM_DELETE: &str = "gts.cf.core.oagw.upstream.v1~:delete";
pub const SCOPE_ROUTE_CREATE: &str = "gts.cf.core.oagw.route.v1~:create";
pub const SCOPE_ROUTE_OVERRIDE: &str = "gts.cf.core.oagw.route.v1~:override";
pub const SCOPE_ROUTE_READ: &str = "gts.cf.core.oagw.route.v1~:read";
pub const SCOPE_ROUTE_DELETE: &str = "gts.cf.core.oagw.route.v1~:delete";
pub const SCOPE_AUTH_PLUGIN_CREATE: &str = "gts.cf.core.oagw.auth_plugin.v1~:create";
pub const SCOPE_AUTH_PLUGIN_READ: &str = "gts.cf.core.oagw.auth_plugin.v1~:read";
pub const SCOPE_AUTH_PLUGIN_DELETE: &str = "gts.cf.core.oagw.auth_plugin.v1~:delete";
pub const SCOPE_GUARD_PLUGIN_CREATE: &str = "gts.cf.core.oagw.guard_plugin.v1~:create";
pub const SCOPE_GUARD_PLUGIN_READ: &str = "gts.cf.core.oagw.guard_plugin.v1~:read";
pub const SCOPE_GUARD_PLUGIN_DELETE: &str = "gts.cf.core.oagw.guard_plugin.v1~:delete";
pub const SCOPE_TRANSFORM_PLUGIN_CREATE: &str = "gts.cf.core.oagw.transform_plugin.v1~:create";
pub const SCOPE_TRANSFORM_PLUGIN_READ: &str = "gts.cf.core.oagw.transform_plugin.v1~:read";
pub const SCOPE_TRANSFORM_PLUGIN_DELETE: &str = "gts.cf.core.oagw.transform_plugin.v1~:delete";
/// Data-plane scope: proxy requests to upstreams.
pub const SCOPE_PROXY_INVOKE: &str = "gts.cf.core.oagw.proxy.v1~:invoke";

/// Whether the token's scopes admit `required` (wildcard `*` matches all).
pub fn scope_allowed(scopes: &[String], required: &str) -> bool {
    scopes.iter().any(|s| s == "*" || s == required)
}

/// Control-plane entry points, backed by the in-memory store.
pub struct ControlPlane {
    store: Arc<MemoryStore>,
    config: Arc<OagwConfig>,
    types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient>,
}

impl ControlPlane {
    pub fn new(
        store: Arc<MemoryStore>,
        config: Arc<OagwConfig>,
        types_registry: Arc<dyn types_registry_sdk::TypesRegistryClient>,
    ) -> Self {
        Self {
            store,
            config,
            types_registry,
        }
    }

    fn require_scope(ctx: &SecurityContext, required: &str) -> Result<(), OagwError> {
        if scope_allowed(ctx.token_scopes(), required) {
            Ok(())
        } else {
            Err(OagwError::permission_denied(format!(
                "required scope `{required}` is not present on the token"
            )))
        }
    }

    // ==================================================================
    // Upstreams
    // ==================================================================

    /// Create an upstream. Server-allocates `id` / timestamps,
    /// resolves the alias, and enforces `(tenant_id, alias)` uniqueness.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        req: UpstreamRequest,
    ) -> Result<UpstreamRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_UPSTREAM_CREATE)?;
        let tenant = ctx.subject_tenant_id();
        let record = self.build_upstream(tenant, req)?;
        match self
            .store
            .upstream_insert(tenant, record.clone())
        {
            Ok(()) => {
                self.register_upstream_instance(&record);
                Ok(record)
            }
            Err((_tenant, _record, key)) => Err(OagwError::already_exists(format!(
                "upstream with alias `{key}` already exists in this tenant"
            ))),
        }
    }

    pub fn list_upstreams(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<UpstreamRecord>, OagwError> {
        Self::require_scope(ctx, SCOPE_UPSTREAM_READ)?;
        let mut items = self.store.upstream_list(ctx.subject_tenant_id());
        items.sort_by_key(|u| u.created_at);
        Ok(items)
    }

    pub fn get_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<UpstreamRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_UPSTREAM_READ)?;
        self.store
            .upstream_get(ctx.subject_tenant_id(), id)
            .ok_or_else(|| not_found_404("upstream", id))
    }

    /// Full replacement PUT. `alias` is immutable once set — recomputed
    /// aliases must match the stored value.
    pub fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        req: UpstreamRequest,
    ) -> Result<UpstreamRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_UPSTREAM_OVERRIDE)?;
        let tenant = ctx.subject_tenant_id();
        let now = created_now();
        let mut record = self.build_upstream(tenant, req)?;
        record.id = id;
        record.created_at = self
            .store
            .upstream_get(tenant, id)
            .map(|existing| existing.created_at)
            .unwrap_or(now);
        record.updated_at = now;
        match self.store.upstream_replace(tenant, id, record) {
            Ok(Some(_)) => {
                if let Some(stored) = self.store.upstream_get(tenant, id) {
                    self.register_upstream_instance(&stored);
                }
                self.store
                    .upstream_get(tenant, id)
                    .ok_or_else(|| not_found_404("upstream", id))
            }
            Ok(None) => Err(not_found_404("upstream", id)),
            Err((_record, stored_alias)) => Err(OagwError::validation(format!(
                "alias is immutable once set (stored alias `{stored_alias}`); delete and re-create to change it"
            ))),
        }
    }

    pub fn delete_upstream(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<(), OagwError> {
        Self::require_scope(ctx, SCOPE_UPSTREAM_DELETE)?;
        let tenant = ctx.subject_tenant_id();
        if self.store.upstream_get(tenant, id).is_none() {
            return Err(not_found_404("upstream", id));
        }
        // Routes reference the upstream by immutable `upstream_id`; a
        // delete must clear them or refuse. Deleting the routes keeps
        // the store consistent with one well-defined behaviour.
        for route in self.store.route_list(tenant) {
            if route.upstream_id == id {
                self.store.route_delete(tenant, route.id);
            }
        }
        self.store.upstream_delete(tenant, id);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Upstream construction + validation
    // ------------------------------------------------------------------

    fn build_upstream(
        &self,
        tenant: Uuid,
        req: UpstreamRequest,
    ) -> Result<UpstreamRecord, OagwError> {
        validate_upstream_request(&req, self.config.allow_http_upstream)?;

        let derived = alias::derive(&req.server.endpoints);
        let alias_value = match derived {
            Some(derived_alias) => {
                if let Some(explicit) = &req.alias {
                    let normalized = alias::normalize(explicit);
                    if normalized != derived_alias {
                        return Err(OagwError::validation(format!(
                            "user-provided alias `{normalized}` differs from the auto-derived \
                             alias `{derived_alias}` for these endpoints"
                        )));
                    }
                }
                derived_alias
            }
            None => {
                let explicit = req.alias.as_ref().ok_or_else(|| {
                    OagwError::validation(
                        "explicit alias required for IP-based or non-derivable endpoints",
                    )
                })?;
                alias::normalize(explicit)
            }
        };

        if alias_value.is_empty() {
            return Err(OagwError::validation("alias must not be empty"));
        }

        validate_cors_config(req.cors.as_ref())?;

        let now = created_now();
        Ok(UpstreamRecord {
            id: Uuid::new_v4(),
            tenant_id: Some(tenant),
            created_at: now,
            updated_at: now,
            enabled: req.enabled,
            alias: alias_value,
            tags: req.tags,
            server: req.server,
            protocol: req.protocol,
            auth: req.auth,
            headers: req.headers,
            plugins: req.plugins,
            rate_limit: req.rate_limit,
            cors: req.cors,
        })
    }

    // ==================================================================
    // Routes
    // ==================================================================

    pub async fn create_route(
        &self,
        ctx: &SecurityContext,
        req: RouteRequest,
    ) -> Result<RouteRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_ROUTE_CREATE)?;
        let tenant = ctx.subject_tenant_id();
        validate_route_request(&req)?;
        // `upstream_id` must belong to the calling tenant — ancestor
        // upstreams are not directly addressable.
        if self.store.upstream_get(tenant, req.upstream_id).is_none() {
            return Err(not_found_404("upstream", req.upstream_id));
        }
        self.ensure_route_match_unique(tenant, req.upstream_id, None, &req.match_)?;
        let now = created_now();
        let record = RouteRecord {
            id: Uuid::new_v4(),
            tenant_id: Some(tenant),
            created_at: now,
            updated_at: now,
            enabled: req.enabled,
            tags: req.tags,
            upstream_id: req.upstream_id,
            match_: req.match_,
            plugins: req.plugins,
            rate_limit: req.rate_limit,
        };
        self.store.route_insert(tenant, record.clone());
        self.register_route_instance(&record);
        Ok(record)
    }

    pub fn list_routes(&self, ctx: &SecurityContext) -> Result<Vec<RouteRecord>, OagwError> {
        Self::require_scope(ctx, SCOPE_ROUTE_READ)?;
        let mut items = self.store.route_list(ctx.subject_tenant_id());
        items.sort_by_key(|r| r.created_at);
        Ok(items)
    }

    pub fn get_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<RouteRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_ROUTE_READ)?;
        self.store
            .route_get(ctx.subject_tenant_id(), id)
            .ok_or_else(|| not_found_404("route", id))
    }

    pub fn replace_route(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
        req: RouteUpdateRequest,
    ) -> Result<RouteRecord, OagwError> {
        Self::require_scope(ctx, SCOPE_ROUTE_OVERRIDE)?;
        let tenant = ctx.subject_tenant_id();
        let existing = self
            .store
            .route_get(tenant, id)
            .ok_or_else(|| not_found_404("route", id))?;
        let update = RouteRequest {
            enabled: req.enabled,
            tags: req.tags,
            upstream_id: existing.upstream_id,
            match_: req.match_,
            plugins: req.plugins,
            rate_limit: req.rate_limit,
        };
        validate_route_request(&update)?;
        // `upstream_id` is immutable — it is not part of the update DTO.
        self.ensure_route_match_unique(tenant, update.upstream_id, Some(id), &update.match_)?;
        let record = RouteRecord {
            id,
            tenant_id: Some(tenant),
            created_at: existing.created_at,
            updated_at: created_now(),
            enabled: update.enabled,
            tags: update.tags,
            upstream_id: update.upstream_id,
            match_: update.match_,
            plugins: update.plugins,
            rate_limit: update.rate_limit,
        };
        self.store.route_replace(tenant, id, record.clone());
        self.register_route_instance(&record);
        Ok(record)
    }

    pub fn delete_route(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), OagwError> {
        Self::require_scope(ctx, SCOPE_ROUTE_DELETE)?;
        let tenant = ctx.subject_tenant_id();
        if self.store.route_delete(tenant, id).is_none() {
            return Err(not_found_404("route", id));
        }
        Ok(())
    }

    /// Reject a new/updated route when it would duplicate an existing
    /// match rule of the same upstream (same path + overlapping method).
    fn ensure_route_match_unique(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
        exclude: Option<Uuid>,
        match_: &crate::domain::model::RouteMatch,
    ) -> Result<(), OagwError> {
        for existing in self.store.route_list(tenant) {
            if Some(existing.id) == exclude || existing.upstream_id != upstream_id {
                continue;
            }
            if let (Some(a), Some(b)) = (existing.match_.http.as_ref(), match_.http.as_ref()) {
                if a.path == b.path
                    && a.methods.iter().any(|m| b.methods.iter().any(|n| n == m))
                {
                    return Err(OagwError::validation(format!(
                        "route already exists for upstream `{upstream_id}` with path `{}` and an \
                         overlapping method",
                        a.path
                    ))
                    .as_conflict());
                }
            }
        }
        Ok(())
    }

    // ==================================================================
    // Plugins
    // ==================================================================

    /// Create a custom (Starlark) plugin. Plugins are immutable after
    /// creation.
    pub async fn create_plugin(
        &self,
        ctx: &SecurityContext,
        req: PluginCreateRequest,
    ) -> Result<PluginRecord, OagwError> {
        let scope = match req.kind {
            PluginKind::Auth => SCOPE_AUTH_PLUGIN_CREATE,
            PluginKind::Guard => SCOPE_GUARD_PLUGIN_CREATE,
            PluginKind::Transform => SCOPE_TRANSFORM_PLUGIN_CREATE,
        };
        Self::require_scope(ctx, scope)?;
        if req.name.trim().is_empty() {
            return Err(OagwError::validation("plugin name must not be empty"));
        }
        if req.source.trim().is_empty() {
            return Err(OagwError::validation("plugin source must not be empty"));
        }
        let record = PluginRecord {
            id: Uuid::new_v4(),
            tenant_id: Some(ctx.subject_tenant_id()),
            created_at: created_now(),
            kind: req.kind,
            name: req.name,
            source: req.source,
            config_schema: req.config_schema,
        };
        self.store
            .plugin_insert(ctx.subject_tenant_id(), record.clone());
        self.register_plugin_instance(&record);
        Ok(record)
    }

    pub fn list_plugins(&self, ctx: &SecurityContext) -> Result<Vec<PluginRecord>, OagwError> {
        let mut items = self.store.plugin_list(ctx.subject_tenant_id());
        items.sort_by_key(|p| p.created_at);
        for item in &items {
            match item.kind {
                PluginKind::Auth => Self::require_scope(ctx, SCOPE_AUTH_PLUGIN_READ)?,
                PluginKind::Guard => Self::require_scope(ctx, SCOPE_GUARD_PLUGIN_READ)?,
                PluginKind::Transform => Self::require_scope(ctx, SCOPE_TRANSFORM_PLUGIN_READ)?,
            }
        }
        Ok(items)
    }

    pub fn get_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<PluginRecord, OagwError> {
        let record = self
            .store
            .plugin_get(ctx.subject_tenant_id(), id)
            .ok_or_else(|| not_found_404("plugin", id))?;
        match record.kind {
            PluginKind::Auth => Self::require_scope(ctx, SCOPE_AUTH_PLUGIN_READ)?,
            PluginKind::Guard => Self::require_scope(ctx, SCOPE_GUARD_PLUGIN_READ)?,
            PluginKind::Transform => Self::require_scope(ctx, SCOPE_TRANSFORM_PLUGIN_READ)?,
        }
        Ok(record)
    }

    /// GET /plugins/{id}/source — return the Starlark source.
    pub fn get_plugin_source(
        &self,
        ctx: &SecurityContext,
        id: Uuid,
    ) -> Result<String, OagwError> {
        Ok(self.get_plugin(ctx, id)?.source)
    }

    pub fn delete_plugin(&self, ctx: &SecurityContext, id: Uuid) -> Result<(), OagwError> {
        let tenant = ctx.subject_tenant_id();
        let record = self
            .store
            .plugin_get(tenant, id)
            .ok_or_else(|| not_found_404("plugin", id))?;
        match record.kind {
            PluginKind::Auth => Self::require_scope(ctx, SCOPE_AUTH_PLUGIN_DELETE)?,
            PluginKind::Guard => Self::require_scope(ctx, SCOPE_GUARD_PLUGIN_DELETE)?,
            PluginKind::Transform => Self::require_scope(ctx, SCOPE_TRANSFORM_PLUGIN_DELETE)?,
        }
        // Only unlinked plugins can be deleted.
        let referenced = self.plugin_references(tenant, id);
        if !referenced.is_empty() {
            return Err(OagwError::plugin_in_use(format!(
                "plugin `{id}` is still referenced by {}",
                referenced.join(", ")
            ))
            .with_ctx("referenced_by", referenced.join(", ")));
        }
        self.store.plugin_delete(tenant, id);
        Ok(())
    }

    /// Collect human-readable references to a plugin across the
    /// tenant's upstreams and routes.
    fn plugin_references(&self, tenant: Uuid, plugin_id: Uuid) -> Vec<String> {
        let mut out = Vec::new();
        for upstream in self.store.upstream_list(tenant) {
            if upstream
                .plugin_refs()
                .iter()
                .any(|p| p.plugin_uuid() == Some(plugin_id))
            {
                out.push(format!("upstream/{}", upstream.id));
            }
        }
        for route in self.store.route_list(tenant) {
            if route
                .plugin_refs()
                .iter()
                .any(|p| p.plugin_uuid() == Some(plugin_id))
            {
                out.push(format!("route/{}", route.id));
            }
        }
        out
    }

    // ==================================================================
    // Best-effort GTS instance registration
    // ==================================================================

    /// Best-effort instance registration. The content must match the
    /// instance's type schema exactly (struct-derived schemas set
    /// `additionalProperties: false`); the instance's type is derived
    /// from its id prefix, not from a content field.
    fn register_upstream_instance(&self, record: &UpstreamRecord) {
        let id = format!("gts.cf.core.oagw.upstream.v1~{}", record.id);
        let entity = serde_json::json!({
            "id": id,
            "alias": record.alias,
            "enabled": record.enabled,
            "tags": record.tags,
            "protocol": record.protocol,
        });
        self.register_best_effort(entity, record.id);
    }

    fn register_route_instance(&self, record: &RouteRecord) {
        let id = format!("gts.cf.core.oagw.route.v1~{}", record.id);
        let protocol = record
            .tenant_id
            .and_then(|tenant| self.store.upstream_get(tenant, record.upstream_id))
            .map(|u| u.protocol)
            .unwrap_or_else(|| crate::gts::PROTOCOL_HTTP.to_owned());
        let entity = serde_json::json!({
            "id": id,
            "upstream_id": record.upstream_id,
            "tags": record.tags,
            "protocol": protocol,
        });
        self.register_best_effort(entity, record.id);
    }

    fn register_plugin_instance(&self, record: &PluginRecord) {
        let id = record.gts_ref();
        let entity = serde_json::json!({
            "id": id,
            "name": record.name,
            "description": null,
        });
        self.register_best_effort(entity, record.id);
    }

    fn register_best_effort(&self, entity: serde_json::Value, resource_id: Uuid) {
        let registry = self.types_registry.clone();
        let id_str = resource_id.to_string();
        tokio::spawn(async move {
            match registry.register(vec![entity]).await {
                Ok(results) => {
                    if results.iter().any(|r| {
                        !matches!(
                            r,
                            types_registry_sdk::RegisterResult::Ok { .. }
                        )
                    }) {
                        warn!(
                            resource = id_str,
                            "types-registry rejected best-effort OAGW instance registration"
                        );
                    }
                }
                Err(err) => {
                    warn!(
                        resource = id_str,
                        error = %err,
                        "types-registry unavailable during best-effort OAGW instance registration"
                    );
                }
            }
        });
    }
}

/// 404 helper for management resources.
fn not_found_404(resource: &str, id: Uuid) -> OagwError {
    OagwError::not_found(format!("{resource} `{id}` not found in this tenant"))
}

// =====================================================================
//                           Validation
// =====================================================================

/// Validate an upstream create/update request.
fn validate_upstream_request(
    req: &UpstreamRequest,
    allow_http: bool,
) -> Result<(), OagwError> {
    if req.server.endpoints.is_empty() {
        return Err(OagwError::validation("server.endpoints must not be empty"));
    }
    for endpoint in &req.server.endpoints {
        validate_endpoint(endpoint, allow_http)?;
    }
    if !req.protocol.ends_with("cf.core.oagw.http.v1")
        && !req.protocol.ends_with("cf.core.oagw.grpc.v1")
    {
        return Err(OagwError::validation(format!(
            "unsupported protocol `{}` (expected a `gts.cf.core.oagw.protocol.v1~*` identifier)",
            req.protocol
        )));
    }
    if let Some(auth) = &req.auth {
        validate_auth(auth)?;
    }
    validate_tags(&req.tags)?;
    Ok(())
}

fn validate_endpoint(endpoint: &crate::domain::model::Endpoint, allow_http: bool) -> Result<(), OagwError> {
    let scheme = endpoint.scheme.trim().to_ascii_lowercase();
    let scheme_ok = matches!(
        scheme.as_str(),
        "https" | "wss" | "wt" | "grpc"
    ) || (allow_http && (scheme == "http" || scheme == "ws"));
    if !scheme_ok {
        return Err(OagwError::validation(format!(
            "unsupported scheme `{}` (allowed: https, wss, wt, grpc{}); plain HTTP requires \
             `allow_http_upstream: true`",
            endpoint.scheme,
            if allow_http { "" } else { "; http disabled by config" }
        )));
    }
    let host = alias::normalize(&endpoint.host);
    if !alias::is_valid_hostname(&host) {
        return Err(OagwError::validation(format!(
            "invalid endpoint host `{}` (must be a hostname or IP literal)",
            endpoint.host
        )));
    }
    if let Some(port) = endpoint.port {
        if !(1..=65535).contains(&port) {
            return Err(OagwError::validation(format!("invalid port `{port}`")));
        }
    }
    Ok(())
}

fn validate_auth(auth: &UpstreamAuth) -> Result<(), OagwError> {
    if auth.plugin_type.is_empty() {
        return Err(OagwError::validation("auth.type must not be empty"));
    }
    Ok(())
}

fn validate_cors_config(cors: Option<&crate::domain::model::CorsConfig>) -> Result<(), OagwError> {
    if let Some(cors) = cors {
        if !cors.enabled {
            return Ok(());
        }
        if cors.allow_credentials
            && cors.allowed_origins.iter().any(|o| o == "*")
        {
            return Err(OagwError::validation(
                "cors: allow_credentials cannot be combined with a wildcard origin `*`",
            ));
        }
    }
    Ok(())
}

/// Validate a route create/update request.
fn validate_route_request(req: &RouteRequest) -> Result<(), OagwError> {
    req.match_.validate().map_err(OagwError::validation)?;
    if let Some(http) = &req.match_.http {
        const ALLOWED: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
        if http.methods.is_empty() {
            return Err(OagwError::validation("match.http.methods must not be empty"));
        }
        for method in &http.methods {
            if !ALLOWED.contains(&method.to_ascii_uppercase().as_str()) {
                return Err(OagwError::validation(format!(
                    "unsupported method `{method}` (allowed: GET, POST, PUT, DELETE, PATCH)"
                )));
            }
        }
        if http.path.is_empty() || !http.path.starts_with('/') {
            return Err(OagwError::validation(
                "match.http.path must be a non-empty path starting with '/'",
            ));
        }
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    const TAG_RE: &str = "^[a-z0-9_-]+$";
    for tag in tags {
        // Minimal check: lowercase alnum / underscore / hyphen. Avoid a
        // regex dependency by hand-rolling the charset check.
        let valid = !tag.is_empty()
            && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && !tag.chars().any(char::is_uppercase);
        if !valid {
            return Err(OagwError::validation(format!(
                "invalid tag `{tag}` (must match {TAG_RE})"
            )));
        }
    }
    Ok(())
}
