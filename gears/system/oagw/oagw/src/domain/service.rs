//! Business services for the OAGW gear.
//!
//! [`ControlPlaneService`] implements upstream/route/plugin CRUD with full
//! validation (alias derivation, scheme policy, tenant scoping, uniqueness).
//! The data-plane interface ([`DataPlaneService`]) is implemented in
//! `crate::infra::proxy`.

use std::sync::Arc;

use uuid::Uuid;

use super::alias::{self, AliasInfo};
use super::error::{DomainError, code};
use super::models::{
    Plugin, PluginItem, Route, RouteMatch, Upstream, is_supported_protocol,
};
use super::repo::{PluginRepo, RouteRepo, UpstreamRepo};
use crate::config::OagwConfig;
use crate::gts;

/// Methods allowed in an HTTP route match.
const VALID_HTTP_METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Control-plane service: tenant-scoped CRUD with validation.
pub struct ControlPlaneService {
    upstreams: Arc<dyn UpstreamRepo>,
    routes: Arc<dyn RouteRepo>,
    plugins: Arc<dyn PluginRepo>,
    config: OagwConfig,
}

impl std::fmt::Debug for ControlPlaneService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneService").finish_non_exhaustive()
    }
}

impl ControlPlaneService {
    /// Construct the service over the in-memory repositories.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepo>,
        routes: Arc<dyn RouteRepo>,
        plugins: Arc<dyn PluginRepo>,
        config: OagwConfig,
    ) -> Self {
        Self {
            upstreams,
            routes,
            plugins,
            config,
        }
    }

    /// The gear's resolved configuration (used by the data plane).
    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    // -----------------------------------------------------------------------
    // Upstreams
    // -----------------------------------------------------------------------

    /// Validate + persist a new upstream. Alias handling per ADR 0001:
    /// derives from endpoints, rejects mismatched explicit aliases on
    /// hostname endpoints, requires explicit aliases for IP/non-derivable
    /// sets, and enforces `(tenant_id, alias)` uniqueness.
    pub fn create_upstream(&self, tenant_id: Uuid, mut up: Upstream) -> Result<Upstream, DomainError> {
        up.tenant_id = Some(tenant_id);
        self.validate_upstream(&mut up, None)?;
        let id = up.id.unwrap_or_else(uuid::Uuid::new_v4);
        up.id = Some(id);
        self.upstreams.upsert(tenant_id, up.clone())?;
        Ok(up)
    }

    /// Full replacement (`PUT`). The alias is immutable; `id`/`tenant_id`
    /// are ignored and taken from the stored resource.
    pub fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        mut up: Upstream,
    ) -> Result<Upstream, DomainError> {
        let existing = self
            .upstreams
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(Upstream::KIND, id))?;
        if !up.alias.is_empty() && alias::normalize_alias(&up.alias) != alias::normalize_alias(&existing.alias) {
            return Err(DomainError::Immutable(format!(
                "alias is immutable: '{}' != '{}'",
                up.alias, existing.alias
            )));
        }
        up.alias.clone_from(&existing.alias);
        up.id = Some(id);
        up.tenant_id = Some(tenant_id);
        self.validate_upstream(&mut up, Some(id))?;
        self.upstreams.upsert(tenant_id, up.clone())?;
        Ok(up)
    }

    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if self.upstreams.get(tenant_id, id).is_none() {
            return Err(DomainError::not_found(Upstream::KIND, id));
        }
        if !self.routes.list_for_upstream(tenant_id, id).is_empty() {
            return Err(DomainError::conflict(format!(
                "upstream {id} still has routes; delete the routes first"
            )));
        }
        self.upstreams.delete(tenant_id, id)?;
        Ok(())
    }

    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Upstream>, DomainError> {
        self.upstreams
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(Upstream::KIND, id))
    }

    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Arc<Upstream>> {
        self.upstreams.list(tenant_id)
    }

    /// Exact-tenant read used by the data-plane tenant-chain walk.
    #[must_use]
    pub fn upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Arc<Upstream>> {
        self.upstreams
            .list(tenant_id)
            .into_iter()
            .find(|u| alias::normalize_alias(&u.alias) == alias::normalize_alias(alias))
    }

    fn validate_upstream(&self, up: &mut Upstream, except_id: Option<Uuid>) -> Result<(), DomainError> {
        if up.server.endpoints.is_empty() {
            return Err(DomainError::validation(
                code::SERVER_FIELD,
                "server.endpoints must contain at least one endpoint",
                code::MISSING,
            ));
        }
        for (i, e) in up.server.endpoints.iter().enumerate() {
            self.validate_endpoint(&up.protocol, e, &format!("server.endpoints[{i}]"))?;
        }
        if !is_supported_protocol(&up.protocol) {
            return Err(DomainError::validation(
                code::SERVER_FIELD,
                format!("unsupported protocol '{}'", up.protocol),
                code::UNSUPPORTED,
            ));
        }

        let info = alias::derive_alias(&up.server.endpoints);
        up.alias.clone_from(&self.normalize_alias_input(&up.alias, &info)?);

        if let Some(rl) = &up.rate_limit {
            self.validate_rate_limit(rl)?;
        }
        if let Some(cors) = &up.cors {
            if cors.has_invalid_wildcard_with_credentials() {
                return Err(DomainError::validation(
                    code::CORS_FIELD,
                    "allow_credentials cannot be combined with allowed_origins ['*']",
                    code::INVALID_VALUE,
                ));
            }
        }
        self.validate_plugin_items(&up.plugins.items, Some(up.tenant_id.unwrap_or_default()))?;

        // `(tenant_id, alias)` uniqueness → 409.
        let tenant = up.tenant_id.unwrap_or_default();
        if self
            .upstreams
            .alias_taken(tenant, &up.alias, except_id)
        {
            return Err(DomainError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                up.alias
            )));
        }
        Ok(())
    }

    fn validate_endpoint(
        &self,
        protocol: &str,
        e: &super::models::Endpoint,
        field: &str,
    ) -> Result<(), DomainError> {
        let allowed = self.config.allow_http_upstream;
        let ok_scheme = match e.scheme.as_str() {
            "https" | "wss" | "wt" | "grpc" => true,
            "http" => allowed,
            _ => false,
        };
        if !ok_scheme {
            let mut msg = format!(
                "scheme '{}' is not allowed for this gear; use https, wss, wt, or grpc",
                e.scheme
            );
            if e.scheme == "http" {
                msg = "scheme 'http' is not allowed (set gears.oagw.config.allow_http_upstream to enable for testing)".to_owned();
            }
            return Err(DomainError::validation(code::SERVER_FIELD, msg, code::UNSUPPORTED));
        }
        let host = e.normalized_host();
        let valid_host = host.parse::<std::net::IpAddr>().is_ok() || alias::valid_hostname(&host);
        if !valid_host {
            return Err(DomainError::validation(
                code::SERVER_FIELD,
                format!("{field}.host is not a valid hostname or IP: '{}'", e.host),
                code::INVALID_FORMAT,
            ));
        }
        if !(1..=65535).contains(&e.port) {
            return Err(DomainError::validation(
                code::SERVER_FIELD,
                format!("{field}.port out of range: {}", e.port),
                code::INVALID_VALUE,
            ));
        }
        let _ = protocol;
        Ok(())
    }

    /// Resolve the stored alias from user input + derivation, enforcing the
    /// ADR 0001 matrix.
    fn normalize_alias_input(&self, user: &str, info: &AliasInfo) -> Result<String, DomainError> {
        match &info.derived {
            Some(derived) => {
                if user.trim().is_empty() {
                    Ok(derived.clone())
                } else {
                    let normalized = alias::normalize_alias(user);
                    if normalized == *derived {
                        Ok(normalized)
                    } else {
                        Err(DomainError::AliasMismatch(format!(
                            "alias '{}' does not match derived alias '{}' for hostname endpoints",
                            user, derived
                        )))
                    }
                }
            }
            None => {
                if user.trim().is_empty() {
                    Err(DomainError::AliasMissing(
                        "explicit alias required: endpoints are IP-based or have no common registrable suffix"
                            .to_owned(),
                    ))
                } else {
                    let normalized = alias::normalize_alias(user);
                    if !alias::valid_alias(&normalized) {
                        return Err(DomainError::validation(
                            code::ALIAS_FIELD,
                            format!("invalid alias: '{}'", user),
                            code::INVALID_FORMAT,
                        ));
                    }
                    let _ = info;
                    Ok(normalized)
                }
            }
        }
    }

    fn validate_rate_limit(&self, rl: &super::models::RateLimitConfig) -> Result<(), DomainError> {
        if rl.sustained.rate < 1 {
            return Err(DomainError::validation(
                code::RATE_LIMIT_FIELD,
                "rateLimit.sustained.rate must be >= 1",
                code::INVALID_VALUE,
            ));
        }
        let _ = self;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Routes
    // -----------------------------------------------------------------------

    pub fn create_route(&self, tenant_id: Uuid, mut r: Route) -> Result<Route, DomainError> {
        self.validate_route(&tenant_id, &r, None)?;
        let id = r.id.unwrap_or_else(uuid::Uuid::new_v4);
        r.id = Some(id);
        r.tenant_id = Some(tenant_id);
        self.routes.upsert(tenant_id, r.clone())?;
        Ok(r)
    }

    pub fn update_route(&self, tenant_id: Uuid, id: Uuid, mut r: Route) -> Result<Route, DomainError> {
        // `upstream_id` is immutable; `id`/`tenant_id` are managed.
        let existing = self
            .routes
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(Route::KIND, id))?;
        if r.upstream_id != existing.upstream_id {
            return Err(DomainError::Immutable(
                "upstream_id is immutable on routes".to_owned(),
            ));
        }
        self.validate_route(&tenant_id, &r, Some(id))?;
        r.id = Some(id);
        r.tenant_id = Some(tenant_id);
        self.routes.upsert(tenant_id, r.clone())?;
        Ok(r)
    }

    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if self.routes.get(tenant_id, id).is_none() {
            return Err(DomainError::not_found(Route::KIND, id));
        }
        self.routes.delete(tenant_id, id)?;
        Ok(())
    }

    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Route>, DomainError> {
        self.routes
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(Route::KIND, id))
    }

    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Arc<Route>> {
        self.routes.list(tenant_id)
    }

    pub fn routes_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<Arc<Route>> {
        self.routes.list_for_upstream(tenant_id, upstream_id)
    }

    fn validate_route(
        &self,
        tenant_id: &Uuid,
        r: &Route,
        except_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        // upstream must exist and belong to the calling tenant.
        if self.upstreams.get(*tenant_id, r.upstream_id).is_none() {
            return Err(DomainError::validation(
                code::UPSTREAM_ID_FIELD,
                format!("upstream_id {} does not exist for this tenant", r.upstream_id),
                code::NOT_FOUND,
            ));
        }
        match &r.match_ {
            RouteMatch::Http(m) => {
                if m.path.trim().is_empty() {
                    return Err(DomainError::validation(
                        code::MATCH_FIELD,
                        "match.http.path must be a non-empty path",
                        code::MISSING,
                    ));
                }
                if !m.path.starts_with('/') {
                    return Err(DomainError::validation(
                        code::MATCH_FIELD,
                        "match.http.path must start with '/'",
                        code::INVALID_FORMAT,
                    ));
                }
                if m.methods.is_empty() {
                    return Err(DomainError::validation(
                        code::MATCH_FIELD,
                        "match.http.methods must contain at least one method",
                        code::MISSING,
                    ));
                }
                for method in &m.methods {
                    let ok = VALID_HTTP_METHODS
                        .iter()
                        .any(|v| v.eq_ignore_ascii_case(method));
                    if !ok {
                        return Err(DomainError::validation(
                            code::MATCH_FIELD,
                            format!("unsupported HTTP method '{method}'"),
                            code::INVALID_VALUE,
                        ));
                    }
                }
                // Match-rule uniqueness within the upstream (path + method).
                let existing = self.routes.list_for_upstream(*tenant_id, r.upstream_id);
                for other in existing {
                    if except_id == other.id {
                        continue;
                    }
                    if let RouteMatch::Http(om) = &other.match_ {
                        let same_path = om.path == m.path;
                        let methods_overlap = m
                            .methods
                            .iter()
                            .any(|a| om.methods.iter().any(|b| b.eq_ignore_ascii_case(a)));
                        if same_path && methods_overlap {
                            return Err(DomainError::conflict(format!(
                                "route conflict: an existing route on upstream {} matches path '{}' with an overlapping method",
                                r.upstream_id, m.path
                            )));
                        }
                    }
                }
            }
            RouteMatch::Grpc(g) => {
                if g.service.trim().is_empty() || g.method.trim().is_empty() {
                    return Err(DomainError::validation(
                        code::MATCH_FIELD,
                        "match.grpc.service and match.grpc.method must be non-empty",
                        code::MISSING,
                    ));
                }
            }
        }
        if let Some(rl) = &r.rate_limit {
            self.validate_rate_limit(rl)?;
        }
        if let Some(cors) = &r.cors {
            if cors.has_invalid_wildcard_with_credentials() {
                return Err(DomainError::validation(
                    code::CORS_FIELD,
                    "allow_credentials cannot be combined with allowed_origins ['*']",
                    code::INVALID_VALUE,
                ));
            }
        }
        self.validate_plugin_items(&r.plugins.items, Some(*tenant_id))?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Plugins
    // -----------------------------------------------------------------------

    pub fn create_plugin(&self, tenant_id: Uuid, mut p: Plugin) -> Result<Plugin, DomainError> {
        if p.plugin_type.trim().is_empty() {
            return Err(DomainError::validation(
                code::PLUGINS_FIELD,
                "pluginType must be a GTS identifier",
                code::MISSING,
            ));
        }
        if p.name.trim().is_empty() {
            return Err(DomainError::validation(
                code::PLUGINS_FIELD,
                "name must be non-empty",
                code::MISSING,
            ));
        }
        if p.source_code.trim().is_empty() {
            return Err(DomainError::validation(
                code::PLUGINS_FIELD,
                "sourceCode must be non-empty (Starlark)",
                code::MISSING,
            ));
        }
        // Unique name per tenant.
        let existing = self.plugins.list(tenant_id);
        if existing.iter().any(|o| o.name == p.name) {
            return Err(DomainError::conflict(format!(
                "a plugin named '{}' already exists for this tenant",
                p.name
            )));
        }
        let id = p.id.unwrap_or_else(uuid::Uuid::new_v4);
        p.id = Some(id);
        p.tenant_id = Some(tenant_id);
        self.plugins.put(tenant_id, p.clone())?;
        Ok(p)
    }

    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if self.plugins.get(tenant_id, id).is_none() {
            return Err(DomainError::not_found(Plugin::KIND, id));
        }
        let refs = self.plugin_references(tenant_id, id);
        if refs.upstreams > 0 || refs.routes > 0 {
            return Err(DomainError::conflict(format!(
                "Plugin is referenced by {} upstream(s) and {} route(s)",
                refs.upstreams, refs.routes
            )));
        }
        self.plugins.delete(tenant_id, id)?;
        Ok(())
    }

    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Arc<Plugin>, DomainError> {
        self.plugins
            .get(tenant_id, id)
            .ok_or_else(|| DomainError::not_found(Plugin::KIND, id))
    }

    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Arc<Plugin>> {
        self.plugins.list(tenant_id)
    }

    /// Count of reference sites (upstreams/routes) binding `plugin_id`.
    #[must_use]
    pub fn plugin_references(&self, tenant_id: Uuid, plugin_id: Uuid) -> PluginReferences {
        let id_str = plugin_id.to_string();
        let mut refs = PluginReferences::default();
        for u in self.upstreams.list(tenant_id) {
            let bound = u.auth.plugin_type.as_deref().map_or(false, |t| t == id_str)
                || u.plugins
                    .items
                    .iter()
                    .any(|i| i.plugin_ref().eq_ignore_ascii_case(&id_str));
            if bound {
                refs.upstreams += 1;
            }
        }
        for r in self.routes.list(tenant_id) {
            if r.plugins
                .items
                .iter()
                .any(|i| i.plugin_ref().eq_ignore_ascii_case(&id_str))
            {
                refs.routes += 1;
            }
        }
        refs
    }

    /// Validate every plugin reference in `items`: builtin GTS identifiers
    /// must be in the catalog and bindable in their referenced position;
    /// UUID references must name an existing custom plugin of this tenant.
    fn validate_plugin_items(
        &self,
        items: &[PluginItem],
        tenant: Option<Uuid>,
    ) -> Result<(), DomainError> {
        for item in items {
            let plugin_ref = item.plugin_ref();
            if plugin_ref.is_empty() {
                return Err(DomainError::validation(
                    code::PLUGINS_FIELD,
                    "plugin reference is empty",
                    code::MISSING,
                ));
            }
            if gts::BUILTIN_PLUGIN_IDS.contains(&plugin_ref) {
                // Bindable builtins: required_headers guard, request_id
                // transform, and the implemented auth plugins are bound here.
                // Catalog-only entries (basic/bearer/timeout/cors/logging/
                // metrics) must not be bound via plugins.items.
                let bindable = plugin_ref == gts::GUARD_REQUIRED_HEADERS
                    || plugin_ref == gts::TRANSFORM_REQUEST_ID;
                if !bindable {
                    return Err(DomainError::validation(
                        code::PLUGINS_FIELD,
                        format!("plugin '{plugin_ref}' is catalog-only and cannot be bound via plugins.items"),
                        code::UNSUPPORTED,
                    ));
                }
                continue;
            }
            // Custom plugin reference: a UUID of an existing plugin.
            if let Ok(id) = uuid::Uuid::parse_str(plugin_ref) {
                let tenant_ok = tenant
                    .map(|t| self.plugins.get(t, id).is_some())
                    .unwrap_or(false);
                if !tenant_ok {
                    return Err(DomainError::validation(
                        code::PLUGINS_FIELD,
                        format!("unknown custom plugin '{plugin_ref}'"),
                        code::NOT_FOUND,
                    ));
                }
            } else {
                return Err(DomainError::validation(
                    code::PLUGINS_FIELD,
                    format!("unknown plugin identifier '{plugin_ref}'"),
                    code::NOT_FOUND,
                ));
            }
        }
        Ok(())
    }

    /// Validate a custom plugin type used as `upstream.auth.type`.
    pub fn validate_auth_type(&self, auth_type: &str) -> Result<(), DomainError> {
        let known = gts::AUTH_NOOP == auth_type
            || gts::AUTH_APIKEY == auth_type
            || gts::AUTH_OAUTH2_CLIENT_CRED == auth_type
            || gts::AUTH_OAUTH2_CLIENT_CRED_BASIC == auth_type;
        if !known {
            return Err(DomainError::validation(
                code::AUTH_FIELD,
                format!("unknown auth plugin type '{auth_type}'"),
                code::UNSUPPORTED,
            ));
        }
        Ok(())
    }
}

/// Reference counts for plugin in-use checks.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PluginReferences {
    pub upstreams: usize,
    pub routes: usize,
}

/// Data-plane service interface. Implemented by
/// `crate::infra::proxy::DataPlaneServiceImpl`.
#[async_trait::async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Proxy a fully-resolved request to its upstream. `tenant_id` is the
    /// *calling* tenant; the implementation walks the tenant chain.
    async fn proxy(
        &self,
        tenant_id: Uuid,
        subject_id: Uuid,
        req: axum::http::Request<axum::body::Body>,
        target_host_header: Option<String>,
    ) -> axum::response::Response;
}
