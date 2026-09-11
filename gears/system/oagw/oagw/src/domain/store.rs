//! In-memory configuration store and the management service layer.
//!
//! The store is a process-local snapshot of the control plane state: upstreams,
//! routes and custom plugins, all tenant-scoped. Proxy-time alias resolution
//! walks the tenant chain so descendants can shadow ancestor upstreams.

use std::sync::Arc;

use dashmap::DashMap;
use tenant_resolver_sdk::{GetAncestorsOptions, TenantResolverClient};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::alias::{self, AliasDecision};
use super::error::OagwError;
use super::model::{Plugin, Route, Upstream};
use crate::gts;

/// Process-local configuration store.
#[derive(Default)]
pub struct Store {
    upstreams: DashMap<Uuid, Upstream>,
    routes: DashMap<Uuid, Route>,
    plugins: DashMap<Uuid, Plugin>,
    /// `(tenant_id, alias) → upstream uuid` unique index.
    aliases: DashMap<(Uuid, String), Uuid>,
    /// `(upstream_id, method, path) → route uuid` uniqueness index.
    route_matches: DashMap<(Uuid, String, String), Uuid>,
}

impl Store {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // ------------------------------------------------------------------
    // Upstreams
    // ------------------------------------------------------------------

    /// Lists every upstream visible to a tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.upstreams
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Returns an upstream owned by the tenant.
    ///
    /// # Errors
    ///
    /// 404 when the upstream does not exist in the caller's tenant.
    pub fn get_upstream(&self, tenant_id: Uuid, id: &str) -> Result<Upstream, OagwError> {
        let uuid = parse_uuid(id)?;
        self.upstreams
            .get(&uuid)
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .ok_or_else(|| OagwError::upstream_not_found(format!("upstream {id} not found")))
    }

    /// Creates an upstream, deriving its alias when the endpoints imply one.
    ///
    /// # Errors
    ///
    /// 400 on invalid configuration, 409 on alias conflict.
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        mut upstream: Upstream,
        cfg: &crate::config::OagwConfig,
    ) -> Result<Upstream, OagwError> {
        upstream.id = None;
        upstream.tenant_id = None;
        upstream.created_at = None;
        upstream.updated_at = None;
        upstream.validate(cfg)?;

        let alias = match alias::enforce_alias_create(
            &upstream.normalized_endpoints(),
            upstream.alias.as_deref(),
        )? {
            AliasDecision::Derived(a)
            | AliasDecision::SuppliedMatchesDerived(a)
            | AliasDecision::Explicit(a) => a,
        };

        let uuid = Uuid::new_v4();
        let record = Upstream {
            id: Some(format!("{}{uuid}", gts::UPSTREAM_TYPE)),
            tenant_id: Some(tenant_id),
            enabled: upstream.enabled,
            alias: Some(alias.clone()),
            tags: upstream.tags,
            server: upstream.server,
            protocol: upstream.protocol,
            auth: upstream.auth,
            headers: upstream.headers,
            plugins: upstream.plugins,
            rate_limit: upstream.rate_limit,
            cors: upstream.cors,
            created_at: Some(now_rfc3339()),
            updated_at: Some(now_rfc3339()),
        };

        if self
            .aliases
            .insert((tenant_id, alias.clone()), uuid)
            .is_some()
        {
            return Err(OagwError::conflict(format!(
                "an upstream with alias {alias:?} already exists for this tenant"
            )));
        }
        self.upstreams.insert(uuid, record);

        self.get_upstream(tenant_id, &uuid.to_string())
    }

    /// Replaces an upstream.
    ///
    /// # Errors
    ///
    /// 404 when missing, 400 on invalid configuration or an illegal alias
    /// transition.
    pub fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: &str,
        replacement: Upstream,
        cfg: &crate::config::OagwConfig,
    ) -> Result<Upstream, OagwError> {
        let existing = self.get_upstream(tenant_id, id)?;
        let uuid = parse_uuid(id)?;
        replacement.validate(cfg)?;

        let was_derivable =
            alias::compute_derived_alias(&existing.normalized_endpoints()).is_some();
        alias::enforce_alias_update(
            existing.alias.as_deref().unwrap_or_default(),
            &replacement.normalized_endpoints(),
            was_derivable,
            replacement.alias.as_deref(),
        )?;

        let record = Upstream {
            id: existing.id.clone(),
            tenant_id: existing.tenant_id,
            enabled: replacement.enabled,
            alias: existing.alias.clone(),
            tags: replacement.tags,
            server: replacement.server,
            protocol: replacement.protocol,
            auth: replacement.auth,
            headers: replacement.headers,
            plugins: replacement.plugins,
            rate_limit: replacement.rate_limit,
            cors: replacement.cors,
            created_at: existing.created_at,
            updated_at: Some(now_rfc3339()),
        };
        self.upstreams.insert(uuid, record);
        self.get_upstream(tenant_id, id)
    }

    /// Deletes an upstream and its routes.
    ///
    /// # Errors
    ///
    /// 404 when missing.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let existing = self.get_upstream(tenant_id, id)?;
        let uuid = parse_uuid(id)?;

        self.routes
            .retain(|_, r| r.upstream_uuid() != uuid.to_string());
        self.route_matches.retain(|(u, _, _), _| *u != uuid);

        if let Some(alias_name) = &existing.alias {
            self.aliases.remove(&(tenant_id, alias_name.clone()));
        }
        self.upstreams.remove(&uuid);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Routes
    // ------------------------------------------------------------------

    /// Lists every route owned by a tenant.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.routes
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Lists routes belonging to one upstream.
    #[must_use]
    pub fn list_routes_for_upstream(&self, tenant_id: Uuid, upstream_uuid: Uuid) -> Vec<Route> {
        self.routes
            .iter()
            .filter(|e| {
                e.value().tenant_id == Some(tenant_id)
                    && e.value().upstream_uuid() == upstream_uuid.to_string()
            })
            .map(|e| e.value().clone())
            .collect()
    }

    /// Returns a route owned by the tenant.
    ///
    /// # Errors
    ///
    /// 404 when missing.
    pub fn get_route(&self, tenant_id: Uuid, id: &str) -> Result<Route, OagwError> {
        let uuid = parse_uuid(id)?;
        self.routes
            .get(&uuid)
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .ok_or_else(|| OagwError::not_found(format!("route {id} not found")))
    }

    /// Creates a route.
    ///
    /// # Errors
    ///
    /// 400 on invalid configuration, 409 on a duplicate match rule or an
    /// upstream that belongs to another tenant.
    pub fn create_route(&self, tenant_id: Uuid, mut route: Route) -> Result<Route, OagwError> {
        route.id = None;
        route.tenant_id = None;
        route.created_at = None;
        route.updated_at = None;
        route.validate()?;

        let upstream_uuid = self.assert_upstream_in_tenant(tenant_id, &route.upstream_id)?;

        let uuid = Uuid::new_v4();
        let record = Route {
            id: Some(format!("{}{uuid}", gts::ROUTE_TYPE)),
            tenant_id: Some(tenant_id),
            upstream_id: route.upstream_id,
            match_: route.match_,
            tags: route.tags,
            enabled: route.enabled,
            cors: route.cors,
            plugins: route.plugins,
            rate_limit: route.rate_limit,
            created_at: Some(now_rfc3339()),
            updated_at: Some(now_rfc3339()),
        };

        self.insert_route_index(&record, upstream_uuid, uuid)?;
        self.routes.insert(uuid, record);
        self.get_route(tenant_id, &uuid.to_string())
    }

    fn insert_route_index(
        &self,
        record: &Route,
        upstream_uuid: Uuid,
        uuid: Uuid,
    ) -> Result<(), OagwError> {
        if let Some(http) = &record.match_.http {
            for method in &http.methods {
                let key = (upstream_uuid, method.clone(), http.path.clone());
                if self.route_matches.insert(key, uuid).is_some() {
                    return Err(OagwError::conflict(format!(
                        "a route with method {method} and path {:?} already exists for this upstream",
                        http.path
                    )));
                }
            }
        } else if let Some(grpc) = &record.match_.grpc {
            let key = (
                upstream_uuid,
                format!("gRPC/{}", grpc.service),
                grpc.method.clone(),
            );
            if self.route_matches.insert(key, uuid).is_some() {
                return Err(OagwError::conflict(format!(
                    "a route for service {:?} / method {:?} already exists for this upstream",
                    grpc.service, grpc.method
                )));
            }
        }
        Ok(())
    }

    fn assert_upstream_in_tenant(
        &self,
        tenant_id: Uuid,
        upstream_id: &str,
    ) -> Result<Uuid, OagwError> {
        self.get_upstream(tenant_id, upstream_id)?;
        parse_uuid(upstream_id)
    }

    /// Replaces a route.
    ///
    /// # Errors
    ///
    /// 404 when missing, 400 on invalid configuration, 409 on conflicts.
    pub fn update_route(
        &self,
        tenant_id: Uuid,
        id: &str,
        replacement: Route,
    ) -> Result<Route, OagwError> {
        let existing = self.get_route(tenant_id, id)?;
        let uuid = parse_uuid(id)?;
        replacement.validate()?;

        let upstream_uuid = self.assert_upstream_in_tenant(tenant_id, &replacement.upstream_id)?;

        // release the old match keys before re-indexing
        self.release_route_index(&existing);

        let record = Route {
            id: existing.id.clone(),
            tenant_id: existing.tenant_id,
            upstream_id: replacement.upstream_id,
            match_: replacement.match_,
            tags: replacement.tags,
            enabled: replacement.enabled,
            cors: replacement.cors,
            plugins: replacement.plugins,
            rate_limit: replacement.rate_limit,
            created_at: existing.created_at,
            updated_at: Some(now_rfc3339()),
        };

        self.insert_route_index(&record, upstream_uuid, uuid)?;
        self.routes.insert(uuid, record);
        self.get_route(tenant_id, id)
    }

    fn release_route_index(&self, route: &Route) {
        let Ok(uuid) = Uuid::parse_str(route.upstream_uuid()) else {
            return;
        };
        if let Some(http) = &route.match_.http {
            for method in &http.methods {
                self.route_matches
                    .remove(&(uuid, method.clone(), http.path.clone()));
            }
        } else if let Some(grpc) = &route.match_.grpc {
            self.route_matches.remove(&(
                uuid,
                format!("gRPC/{}", grpc.service),
                grpc.method.clone(),
            ));
        }
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// 404 when missing.
    pub fn delete_route(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let existing = self.get_route(tenant_id, id)?;
        let uuid = parse_uuid(id)?;
        self.release_route_index(&existing);
        self.routes.remove(&uuid);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Plugins
    // ------------------------------------------------------------------

    /// Lists custom plugins owned by a tenant.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<Plugin> {
        self.plugins
            .iter()
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Returns a custom plugin owned by the tenant.
    ///
    /// # Errors
    ///
    /// 404 when missing.
    pub fn get_plugin(&self, tenant_id: Uuid, id: &str) -> Result<Plugin, OagwError> {
        let uuid = parse_uuid(id)?;
        self.plugins
            .get(&uuid)
            .filter(|e| e.value().tenant_id == Some(tenant_id))
            .map(|e| e.value().clone())
            .ok_or_else(|| OagwError::plugin_not_found_api(format!("plugin {id} not found")))
    }

    /// Creates a custom plugin.
    ///
    /// # Errors
    ///
    /// 400 on invalid definition, 409 on a duplicate name.
    pub fn create_plugin(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, OagwError> {
        if plugin.name.trim().is_empty() {
            return Err(OagwError::validation("plugin.name must not be empty"));
        }
        if !matches!(plugin.plugin_type.as_str(), "auth" | "guard" | "transform") {
            return Err(OagwError::validation(format!(
                "plugin.type must be one of auth, guard, transform (got {:?})",
                plugin.plugin_type
            )));
        }
        if self
            .plugins
            .iter()
            .any(|e| e.value().tenant_id == Some(tenant_id) && e.value().name == plugin.name)
        {
            return Err(OagwError::conflict(format!(
                "a plugin named {:?} already exists for this tenant",
                plugin.name
            )));
        }

        let uuid = Uuid::new_v4();
        let record = Plugin {
            id: Some(format!("{}{uuid}", plugin.type_prefix())),
            tenant_id: Some(tenant_id),
            ..plugin
        };
        self.plugins.insert(uuid, record.clone());
        Ok(record)
    }

    /// Deletes a custom plugin, refusing while it is still bound.
    ///
    /// # Errors
    ///
    /// 404 when missing, 409 with the referencing upstreams and routes.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let existing = self.get_plugin(tenant_id, id)?;
        let uuid = parse_uuid(id)?;
        let full_id = format!("{}{uuid}", existing.type_prefix());

        let mut upstream_refs: Vec<String> = Vec::new();
        for entry in self.upstreams.iter() {
            if entry.value().tenant_id == Some(tenant_id)
                && entry
                    .value()
                    .plugins
                    .items
                    .iter()
                    .any(|item| item == &full_id || item == &uuid.to_string())
            {
                upstream_refs.push(entry.value().id.clone().unwrap_or_default());
            }
        }
        let mut route_refs: Vec<String> = Vec::new();
        for entry in self.routes.iter() {
            if entry.value().tenant_id == Some(tenant_id)
                && entry
                    .value()
                    .plugins
                    .items
                    .iter()
                    .any(|item| item == &full_id || item == &uuid.to_string())
            {
                route_refs.push(entry.value().id.clone().unwrap_or_default());
            }
        }

        if !upstream_refs.is_empty() || !route_refs.is_empty() {
            let mut err = OagwError::plugin_in_use(format!(
                "plugin {} is referenced by {} upstream(s) and {} route(s)",
                full_id,
                upstream_refs.len(),
                route_refs.len()
            ));
            err = err.with_extension("plugin_id", full_id).with_extension(
                "referenced_by",
                serde_json::json!({ "upstreams": upstream_refs, "routes": route_refs }),
            );
            return Err(err);
        }

        self.plugins.remove(&uuid);
        Ok(())
    }

    // ------------------------------------------------------------------
    // Proxy-time resolution
    // ------------------------------------------------------------------

    /// Resolves an alias by walking the tenant chain, closest match first.
    #[must_use]
    pub fn find_upstream_by_alias(
        &self,
        tenant_ids: &[Uuid],
        alias_name: &str,
    ) -> Option<Upstream> {
        for tenant in tenant_ids {
            if let Some(uuid) = self.aliases.get(&(*tenant, alias_name.to_owned()))
                && let Some(u) = self.upstreams.get(&uuid)
            {
                return Some(u.value().clone());
            }
        }
        None
    }

    /// Routes belonging to an upstream (any tenant in the chain).
    #[must_use]
    pub fn find_routes_for_upstream(&self, upstream_uuid: Uuid) -> Vec<Route> {
        self.routes
            .iter()
            .filter(|e| e.value().upstream_uuid() == upstream_uuid.to_string())
            .map(|e| e.value().clone())
            .collect()
    }

    /// All upstreams in the chain (ancestor constraints, e.g. `enforce`).
    #[must_use]
    pub fn upstreams_in(&self, tenant_ids: &[Uuid]) -> Vec<Upstream> {
        self.upstreams
            .iter()
            .filter(|e| e.value().tenant_id.is_some_and(|t| tenant_ids.contains(&t)))
            .map(|e| e.value().clone())
            .collect()
    }
}

/// The current time as an RFC 3339 timestamp.
#[must_use]
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Parses a path parameter as a UUID, accepting a full GTS identifier too.
///
/// # Errors
///
/// 400 when the identifier is not a UUID.
pub fn parse_uuid(value: &str) -> Result<Uuid, OagwError> {
    let bare = gts::unqualify(gts::UPSTREAM_TYPE, value);
    let bare = bare.rsplit('~').next().unwrap_or(bare);
    Uuid::parse_str(bare)
        .map_err(|_| OagwError::validation(format!("invalid resource identifier {value:?}")))
}

/// Resolves the tenant chain (self first, then ancestors) for proxy-time
/// lookups.
///
/// # Errors
///
/// Returns a 404 problem when the caller's tenant cannot be resolved.
pub async fn tenant_chain(
    resolver: Option<&std::sync::Arc<dyn TenantResolverClient>>,
    ctx: &SecurityContext,
) -> Result<Vec<Uuid>, OagwError> {
    let tenant_id = ctx.subject_tenant_id();
    let mut chain = vec![tenant_id];

    if let Some(resolver) = resolver {
        let response = resolver
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::TenantId(tenant_id),
                &GetAncestorsOptions::default(),
            )
            .await
            .map_err(|e| OagwError::not_found(format!("tenant resolution failed: {e}")))?;
        for ancestor in response.ancestors {
            chain.push(ancestor.id.0);
        }
    }

    Ok(chain)
}

/// Snapshot type used by the data plane.
pub type SharedStore = Arc<Store>;
