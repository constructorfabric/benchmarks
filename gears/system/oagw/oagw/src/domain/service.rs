// Created: 2026-08-31 by Constructor Tech
//! Control-plane service (DESIGN §3.3 "CRUD Semantics", §3.6).
//!
//! The service owns every semantic rule the wire contract cares about:
//! alias derivation and immutability, endpoint validation, uniqueness
//! conflicts, tenant scoping, ancestor invisibility, plugin-usage tracking and
//! the upstream → route cascade. Handlers only translate HTTP to and from
//! these calls.

use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::domain::alias::{enforce_update_alias, resolve_creation_alias};
use crate::domain::lifecycle::UpstreamRemoval;
use crate::domain::model::{
    AuthConfig, CorsConfig, Plugin, PluginBinding, PluginsConfig, Route, Timestamps, Upstream,
};
use crate::domain::spec::{PluginSpec, RouteSpec, RouteUpdateSpec, UpstreamSpec};
use crate::domain::store::Store;
use crate::domain::validation::{
    ValidationPolicy, validate_config_bytes, validate_cors, validate_endpoints,
    validate_explicit_alias, validate_headers, validate_rate_limit, validate_route_match,
    validate_tags,
};
use crate::error::{OagwError, OagwErrorKind, OagwResult, ReferencedBy, ResourceKind};

/// Validate the `cors` member of an upstream or a route, when present.
///
/// # Errors
/// 400 on an ADR-0004 violation, propagated from
/// [`crate::domain::validation::validate_cors`].
fn validate_cors_record(config: Option<&CorsConfig>) -> OagwResult<()> {
    config.map_or(Ok(()), validate_cors)
}

/// Control-plane operations for the management API.
pub struct OagwService {
    policy: ValidationPolicy,
    store: Arc<dyn Store>,
    /// Subscribers to the removal of an upstream record.
    ///
    /// A [`Mutex`], because registration happens once at gear initialisation
    /// while the removals fire on the request path.
    removals: Mutex<Vec<Arc<dyn UpstreamRemoval>>>,
}

impl OagwService {
    /// Build a service on top of `store`.
    #[must_use]
    pub fn new(policy: ValidationPolicy, store: Arc<dyn Store>) -> Arc<Self> {
        Arc::new(Self {
            policy,
            store,
            removals: Mutex::new(Vec::new()),
        })
    }

    /// Subscribe `observer` to the removal of an upstream record.
    ///
    /// The data plane uses this to drop the per-upstream state it holds.
    pub fn observe_removals(&self, observer: Arc<dyn UpstreamRemoval>) {
        if let Ok(mut observers) = self.removals.lock() {
            observers.push(observer);
        }
    }

    /// Publish the removal of an upstream to every subscriber.
    fn upstream_removed(&self, upstream_id: Uuid) {
        let Ok(observers) = self.removals.lock() else {
            return;
        };
        for observer in observers.iter() {
            observer.upstream_removed(upstream_id);
        }
    }

    /// Validation policy in force.
    #[must_use]
    pub const fn policy(&self) -> &ValidationPolicy {
        &self.policy
    }

    // -- upstreams ---------------------------------------------------------

    /// Create an upstream (POST /upstreams).
    ///
    /// # Errors
    /// 400 on invalid endpoints, a rejected alias, inline credential material
    /// or an unresolvable plugin binding, 409 on alias conflict.
    pub fn create_upstream(&self, tenant_id: Uuid, spec: &UpstreamSpec) -> OagwResult<Upstream> {
        let endpoints = spec.server.endpoints();
        validate_endpoints(&self.policy, &endpoints)?;
        validate_tags(spec.tags.as_deref().unwrap_or_default())?;
        validate_headers(spec.headers.as_ref())?;
        validate_rate_limit(spec.rate_limit.as_ref())?;
        validate_cors_record(spec.cors.as_ref())?;
        let alias = resolve_creation_alias(&endpoints, spec.alias.as_deref())?;
        self.validate_bindings(tenant_id, spec.auth.as_ref(), spec.plugins.as_ref())?;
        let now = crate::domain::time::now_millis();
        let record = Upstream {
            id: Uuid::new_v4(),
            tenant_id,
            alias,
            enabled: spec.enabled.unwrap_or(true),
            protocol: spec.protocol,
            endpoints,
            tags: spec.tags.clone().unwrap_or_default(),
            auth: spec.auth.clone(),
            headers: spec.headers.clone(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            timestamps: Timestamps {
                created_at: now,
                updated_at: now,
            },
        };
        self.store.insert_upstream(record)
    }

    /// Validate the credential shape, the auth binding and the plugin chain
    /// (DESIGN §3.2 "Resolution Algorithm", §2.2 credential isolation).
    ///
    /// Named plugins must be bindable (`basic`/`bearer`/`timeout`/`cors`/
    /// `logging`/`metrics` are catalogued but not resolvable); custom plugin
    /// references must resolve in the calling tenant and match the family the
    /// reference declares. A chain never binds an auth plugin: credential
    /// injection has the `upstream.auth` member of its own, and a chain entry
    /// naming one would fail every request of the data plane.
    fn validate_bindings(
        &self,
        tenant_id: Uuid,
        auth: Option<&AuthConfig>,
        plugins: Option<&PluginsConfig>,
    ) -> OagwResult<()> {
        if let Some(auth) = auth {
            crate::domain::credentials::validate_auth_config(auth)?;
            if let Some(plugin_type) = auth.plugin_type.as_deref().filter(|p| !p.trim().is_empty())
            {
                self.validate_auth_plugin_type(tenant_id, plugin_type)?;
            }
        }
        for reference in chain_plugins(plugins) {
            let parsed = crate::domain::plugin::PluginRef::parse(reference);
            if let Some(detail) = chain_auth_rejection(&parsed) {
                return Err(OagwError::validation(detail));
            }
            if parsed.is_bindable_built_in() {
                continue;
            }
            match parsed {
                crate::domain::plugin::PluginRef::BuiltIn { .. } => {
                    return Err(OagwError::validation(format!(
                        "plugin '{reference}' is catalogued but cannot be bound"
                    )));
                }
                crate::domain::plugin::PluginRef::Custom { .. } => {
                    let record = self.resolve_plugin_reference(tenant_id, reference)?;
                    if record.kind == crate::domain::model::PluginKind::Auth {
                        return Err(OagwError::validation(format!(
                            "auth plugin '{}' belongs in the upstream 'auth' binding, not in the \
                             plugin chain",
                            record.name
                        )));
                    }
                }
                crate::domain::plugin::PluginRef::Unrecognised(_) => {
                    return Err(OagwError::validation(format!(
                        "plugin reference '{reference}' is not a plugin GTS identifier"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_auth_plugin_type(&self, tenant_id: Uuid, plugin_type: &str) -> OagwResult<()> {
        let parsed = crate::domain::plugin::PluginRef::parse(plugin_type);
        if parsed.is_bindable_built_in() {
            return Ok(());
        }
        match parsed {
            crate::domain::plugin::PluginRef::BuiltIn { .. } => Err(OagwError::validation(
                format!("auth plugin '{plugin_type}' is catalogued but cannot be bound"),
            )),
            crate::domain::plugin::PluginRef::Custom { .. } => {
                let record = self.resolve_plugin_reference(tenant_id, plugin_type)?;
                if record.kind != crate::domain::model::PluginKind::Auth {
                    return Err(OagwError::validation(format!(
                        "plugin '{}' is a {} plugin and cannot be used as an auth binding",
                        record.name,
                        record.kind.as_str()
                    )));
                }
                Ok(())
            }
            crate::domain::plugin::PluginRef::Unrecognised(_) => Err(OagwError::validation(
                format!("unknown auth plugin '{plugin_type}'"),
            )),
        }
    }

    /// Read one upstream (GET /upstreams/{id}).
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Upstream> {
        self.store
            .get_upstream(tenant_id, id)?
            .ok_or_else(|| OagwError::not_found(ResourceKind::Upstream, id))
    }

    /// List upstreams (GET /upstreams).
    ///
    /// # Errors
    /// Propagated from the store.
    pub fn list_upstreams(&self, tenant_id: Uuid) -> OagwResult<Vec<Upstream>> {
        self.store.list_upstreams(tenant_id)
    }

    /// Replace an upstream in full (PUT /upstreams/{id}).
    ///
    /// # Errors
    /// 404 on a foreign record, 400 on invalid endpoints, an alias change,
    /// inline credential material or an unresolvable plugin binding, 409 on
    /// alias conflict.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: &UpstreamSpec,
    ) -> OagwResult<Upstream> {
        let existing = self.get_upstream(tenant_id, id)?;
        let endpoints = spec.server.endpoints();
        validate_endpoints(&self.policy, &endpoints)?;
        validate_tags(spec.tags.as_deref().unwrap_or_default())?;
        validate_headers(spec.headers.as_ref())?;
        validate_rate_limit(spec.rate_limit.as_ref())?;
        validate_cors_record(spec.cors.as_ref())?;
        enforce_update_alias(
            &existing.alias,
            &existing.endpoints,
            &endpoints,
            spec.alias.as_deref(),
        )?;
        if let Some(alias) = spec.alias.as_deref() {
            validate_explicit_alias(&crate::domain::alias::normalize_alias(alias))?;
        }
        self.validate_bindings(tenant_id, spec.auth.as_ref(), spec.plugins.as_ref())?;
        let record = Upstream {
            id: existing.id,
            tenant_id: existing.tenant_id,
            alias: existing.alias,
            enabled: spec.enabled.unwrap_or(true),
            protocol: spec.protocol,
            endpoints,
            tags: spec.tags.clone().unwrap_or_default(),
            auth: spec.auth.clone(),
            headers: spec.headers.clone(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            timestamps: Timestamps::touched(existing.timestamps.created_at),
        };
        self.store.replace_upstream(record)
    }

    /// Delete an upstream (DELETE /upstreams/{id}).
    ///
    /// Routes bound to the upstream are deleted with it, because a route
    /// without its upstream is unreachable (DESIGN §3.6 cascade). The removal
    /// is a single store operation: a reader never observes an upstream
    /// without its routes or vice versa.
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<()> {
        self.store
            .delete_upstream_cascade(tenant_id, id)?
            .ok_or_else(|| OagwError::not_found(ResourceKind::Upstream, id))?;
        self.upstream_removed(id);
        Ok(())
    }

    // -- routes ------------------------------------------------------------

    /// Create a route (POST /routes).
    ///
    /// # Errors
    /// 404 when the upstream is not addressable, 400 on an invalid match rule,
    /// an unresolvable plugin binding or inline credential material, 409 on a
    /// duplicate match rule.
    pub fn create_route(&self, tenant_id: Uuid, spec: &RouteSpec) -> OagwResult<Route> {
        validate_route_match(&spec.match_rule)?;
        validate_tags(spec.tags.as_deref().unwrap_or_default())?;
        validate_rate_limit(spec.rate_limit.as_ref())?;
        validate_cors_record(spec.cors.as_ref())?;
        self.validate_bindings(tenant_id, None, spec.plugins.as_ref())?;
        let now = crate::domain::time::now_millis();
        let record = Route {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id: spec.upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            match_rule: spec.match_rule.clone(),
            tags: spec.tags.clone().unwrap_or_default(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            timestamps: Timestamps {
                created_at: now,
                updated_at: now,
            },
        };
        self.store.insert_route_checked(record)
    }

    /// Read one route (GET /routes/{id}).
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Route> {
        self.store
            .get_route(tenant_id, id)?
            .ok_or_else(|| OagwError::not_found(ResourceKind::Route, id))
    }

    /// List routes (GET /routes).
    ///
    /// # Errors
    /// Propagated from the store.
    pub fn list_routes(&self, tenant_id: Uuid) -> OagwResult<Vec<Route>> {
        self.store.list_routes(tenant_id)
    }

    /// Replace a route in full (PUT /routes/{id}).
    ///
    /// `upstream_id` is immutable: the value sent on PUT is ignored.
    ///
    /// # Errors
    /// 404 on a foreign record, 400 on an invalid match rule, 409 on a
    /// duplicate match rule.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: &RouteUpdateSpec,
    ) -> OagwResult<Route> {
        let existing = self.get_route(tenant_id, id)?;
        validate_route_match(&spec.match_rule)?;
        validate_tags(spec.tags.as_deref().unwrap_or_default())?;
        validate_rate_limit(spec.rate_limit.as_ref())?;
        validate_cors_record(spec.cors.as_ref())?;
        self.validate_bindings(tenant_id, None, spec.plugins.as_ref())?;
        let record = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            enabled: spec.enabled.unwrap_or(true),
            match_rule: spec.match_rule.clone(),
            tags: spec.tags.clone().unwrap_or_default(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            timestamps: Timestamps::touched(existing.timestamps.created_at),
        };
        self.store.replace_route(record)
    }

    /// Delete a route (DELETE /routes/{id}).
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<()> {
        self.store
            .delete_route(tenant_id, id)?
            .ok_or_else(|| OagwError::not_found(ResourceKind::Route, id))?;
        Ok(())
    }

    // -- plugins -----------------------------------------------------------

    /// Create a plugin (POST /plugins).
    ///
    /// # Errors
    /// 400 when required members are missing or `config` carries inline
    /// credential material, 409 on a name conflict.
    pub fn create_plugin(&self, tenant_id: Uuid, spec: &PluginSpec) -> OagwResult<Plugin> {
        let name = spec.name()?.to_owned();
        let kind = spec.plugin_type()?;
        let source = spec.source_code()?.to_owned();
        if let Some(config) = spec.config.as_ref() {
            validate_config_bytes(config)?;
            crate::domain::credentials::validate_plugin_config(config)?;
        }
        let now = crate::domain::time::now_millis();
        let record = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            kind,
            name,
            enabled: spec.enabled.unwrap_or(true),
            config: spec.config.clone().unwrap_or(serde_json::Value::Null),
            config_schema: spec.config_schema.clone(),
            description: spec.description.clone(),
            source,
            timestamps: Timestamps {
                created_at: now,
                updated_at: now,
            },
        };
        self.store.insert_plugin(record)
    }

    /// Read one plugin (GET /plugins/{id}).
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<Plugin> {
        self.store
            .get_plugin(tenant_id, id)?
            .ok_or_else(|| OagwError::not_found(ResourceKind::Plugin, id))
    }

    /// List plugins (GET /plugins).
    ///
    /// # Errors
    /// Propagated from the store.
    pub fn list_plugins(&self, tenant_id: Uuid) -> OagwResult<Vec<Plugin>> {
        self.store.list_plugins(tenant_id)
    }

    /// Starlark source of a plugin (GET /plugins/{id}/source).
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant.
    pub fn plugin_source(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<String> {
        Ok(self.get_plugin(tenant_id, id)?.source)
    }

    /// Delete a plugin (DELETE /plugins/{id}), ADR-0001 "Plugin Deletion
    /// Behavior".
    ///
    /// The usage scan and the removal are one store operation, so a binding
    /// created concurrently cannot survive with a dangling reference.
    ///
    /// # Errors
    /// 404 when the record belongs to another tenant, 409 `plugin.in_use` when
    /// an upstream or route still binds it.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> OagwResult<()> {
        let removal = self.store.delete_plugin_if_unreferenced(tenant_id, id)?;
        let Some(plugin) = removal.plugin else {
            return Err(OagwError::not_found(ResourceKind::Plugin, id));
        };
        let referenced_by = ReferencedBy {
            upstreams: removal.upstreams.iter().map(Uuid::to_string).collect(),
            routes: removal.routes.iter().map(Uuid::to_string).collect(),
        };
        if referenced_by.total() > 0 {
            return Err(OagwError::plugin_in_use(plugin.id, referenced_by));
        }
        Ok(())
    }

    /// Resolve a custom plugin reference to its record.
    ///
    /// Used when an upstream or route binds a plugin by UUID or by a
    /// UUID-backed GTS id. A reference that carries a GTS stem also declares
    /// the family it expects, and a stored record of another family is a
    /// 400 (the caller bound the wrong plugin), not a resolution failure.
    ///
    /// # Errors
    /// 503 `plugin.not_found` when the reference cannot be resolved in the
    /// calling tenant (DESIGN §3.3 `PluginNotFound`), 400 when the resolved
    /// record does not match the family the reference declares.
    pub fn resolve_plugin_reference(&self, tenant_id: Uuid, reference: &str) -> OagwResult<Plugin> {
        let parsed = crate::domain::plugin::PluginRef::parse(reference);
        let Some(id) = parsed.custom_id() else {
            return Err(unresolved_plugin(reference));
        };
        let Some(record) = self.store.get_plugin(tenant_id, id)? else {
            return Err(unresolved_plugin(reference));
        };
        if let Some(expected) = parsed.kind()
            && record.kind != expected
        {
            return Err(OagwError::validation(format!(
                "plugin '{}' is a {} plugin, but the reference declares a {} plugin",
                record.name,
                record.kind.as_str(),
                expected.as_str()
            )));
        }
        Ok(record)
    }
}

/// 503 `plugin.not_found` for a reference the tenant cannot resolve
/// (DESIGN §3.3 `PluginNotFound`).
fn unresolved_plugin(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::PluginNotFound,
        format!("plugin '{reference}' was not found for the calling tenant"),
    )
}

/// Why a chain reference may not name an auth plugin, when it does.
///
/// `upstream.auth` is the only way to bind credential injection (ADR-0002: one
/// auth plugin per upstream, a member of the upstream schema of its own), so a
/// chain entry that names one is a binding mistake. Left uncaught it would be
/// stored and then fail every request of the data plane with a 503.
fn chain_auth_rejection(parsed: &crate::domain::plugin::PluginRef) -> Option<String> {
    let reference = parsed.raw();
    let auth = crate::domain::model::PluginKind::Auth;
    if parsed.kind() == Some(auth) {
        return Some(format!(
            "auth plugin '{reference}' belongs in the upstream 'auth' binding, not in the plugin \
             chain"
        ));
    }
    match parsed {
        crate::domain::plugin::PluginRef::Unrecognised(name)
            if crate::domain::plugin::lookup_built_in(auth, name).is_some() =>
        {
            Some(format!(
                "auth plugin '{name}' belongs in the upstream 'auth' binding, not in the plugin \
                 chain"
            ))
        }
        _ => None,
    }
}

/// Chain references of a plugin chain, empty when the chain is absent.
fn chain_plugins(plugins: Option<&PluginsConfig>) -> Vec<&str> {
    plugins.map_or_else(Vec::new, |chain| {
        chain.items.iter().map(PluginBinding::reference).collect()
    })
}
