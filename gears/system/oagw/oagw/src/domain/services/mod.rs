//! `ControlPlaneService` — the aggregate the REST handlers and the data plane
//! call through (`cpt-cf-oagw-dod-gear-foundation-rest-registration`).
//!
//! The management flows describe a linear `Client -> API Handler ->
//! ControlPlaneService -> Response`; ADR 0006's request-flow text names the
//! one concrete signature: `resolve_proxy_target(alias, method, path)`
//! returning `(EffectiveUpstream, MatchedRoute)`. Everything else is the CRUD
//! surface the upstream/route/plugin flows describe, each operation tenant
//! scoped.
//!
//! The implementation is a thin facade over the three repositories, so entry
//! 2.2 can swap in the REST handlers and entry 2.5 the data plane without
//! touching storage. The effective-config merge (`domain/merge`) is *not*
//! cached here: it is recomputed per request.

use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, RouteMatchType, Upstream};
use crate::domain::merge::EffectiveConfig;
use crate::domain::repo::{
    PluginBinding, PluginRepository, RouteRecord, RouteRepository, UpstreamRecord,
    UpstreamRepository, WriteConflict,
};
use crate::domain::{error::DomainError, merge};

/// A resolved proxy target: the effective upstream configuration plus the
/// matched route (ADR 0006).
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProxyTarget {
    pub upstream: EffectiveConfig,
    /// The matched route's identifier, and its match type.
    pub route_id: Uuid,
    pub match_type: RouteMatchType,
}

/// The control-plane aggregate.
pub trait ControlPlaneService: Send + Sync {
    // -- upstream CRUD ---------------------------------------------------
    fn create_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<Upstream, DomainError>;
    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;
    fn get_upstream_by_alias(&self, tenant_id: Uuid, alias: &str)
    -> Result<Upstream, DomainError>;
    fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;
    fn replace_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<Upstream, DomainError>;
    fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    // -- route CRUD ------------------------------------------------------
    fn create_route(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError>;
    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;
    fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;
    fn list_routes_for_upstream(&self, tenant_id: Uuid, upstream_id: Uuid)
    -> Result<Vec<Route>, DomainError>;
    fn replace_route(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError>;
    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    // -- plugin CRUD -----------------------------------------------------
    fn create_plugin(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, DomainError>;
    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;
    fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;
    fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError>;

    // -- request path ----------------------------------------------------
    /// `resolve_proxy_target(alias, method, path)` -> `(upstream, route)`.
    fn resolve_proxy_target(
        &self,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<ResolvedProxyTarget, DomainError>;
}

/// The management aggregate of entry 2.2 lives beside the control-plane
/// aggregate: the two are separate on purpose, because the management surface
/// is async and permission-gated while the shared control-plane service is the
/// synchronous store facade the proxy path calls through.
pub mod management;

/// The route-management aggregate of entry 2.3, over the same store and behind
/// the same authorization and configuration-write seams as the upstream
/// aggregate.
pub mod route_management;

/// The data-plane seam of entry 2.4: the proxy handler dispatches every
/// non-preflight request through it.
pub mod proxy;

/// The plugin-catalog aggregate of entry 2.6, over the same store and behind
/// the same authorization seam as the other two management aggregates, plus
/// the per-base-type plugin permission sets and the reference-guarded
/// deletion.
pub mod plugin_management;

/// The concrete implementation over the three repositories.
pub struct ControlPlaneServiceImpl {
    upstreams: std::sync::Arc<dyn UpstreamRepository>,
    routes: std::sync::Arc<dyn RouteRepository>,
    plugins: std::sync::Arc<dyn PluginRepository>,
    /// `allow_http_upstream`, needed to re-validate on write.
    allow_http_upstream: bool,
}

impl ControlPlaneServiceImpl {
    /// Build the service over the three repositories.
    pub fn new(
        upstreams: std::sync::Arc<dyn UpstreamRepository>,
        routes: std::sync::Arc<dyn RouteRepository>,
        plugins: std::sync::Arc<dyn PluginRepository>,
        allow_http_upstream: bool,
    ) -> Self {
        Self { upstreams, routes, plugins, allow_http_upstream }
    }

    fn map_conflict(&self, conflict: WriteConflict) -> DomainError {
        match conflict {
            WriteConflict::UniqueKey => DomainError::Conflict {
                detail: "a record with the same unique key already exists in this tenant"
                    .to_owned(),
                referenced_by: None,
            },
            WriteConflict::RouteMatch => DomainError::Conflict {
                detail: "another enabled route of this upstream already matches this method, \
                         path prefix and priority"
                    .to_owned(),
                referenced_by: None,
            },
            WriteConflict::BindingPositions => DomainError::ValidationError {
                detail: "plugin binding positions must be contiguous from zero".to_owned(),
                path: Some("plugins".to_owned()),
                trace_id: None,
            },
            WriteConflict::BindingReference => DomainError::ValidationError {
                detail: "plugin binding reference and UUID disagree".to_owned(),
                path: Some("plugins".to_owned()),
                trace_id: None,
            },
            WriteConflict::MissingParent => DomainError::NotFound { resource_type: "upstream" },
        }
    }

    /// The ordered plugin bindings of a `plugins` block: positions are the
    /// list indices, contiguous from zero, and `plugin_uuid` is set only for a
    /// UUID-backed reference.
    /// Resolve the ordered `plugins.items[]` of a write into the binding rows
    /// the write persists.
    ///
    /// A UUID-backed reference is resolved through the caller-scoped plugin
    /// catalog, exactly as the management aggregates resolve one: a plugin the
    /// calling tenant does not hold is a validation error, so a tenant can
    /// never persist a binding to a record it cannot read
    /// (`inst-ps-bind-2` .. `-8`). A built-in type identifier is not a catalog
    /// row and is carried through unchanged.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when a UUID-backed reference names no plugin
    /// of the calling tenant.
    fn bindings_of(
        &self,
        tenant_id: Uuid,
        items: &[String],
    ) -> Result<Vec<PluginBinding>, DomainError> {
        items
            .iter()
            .enumerate()
            .map(|(position, reference)| {
                let plugin_uuid = uuid_of(reference);
                if let Some(uuid) = plugin_uuid {
                    self.plugins.get(tenant_id, uuid)?;
                }
                Ok(PluginBinding {
                    position: position as u32,
                    plugin_ref: reference.clone(),
                    plugin_uuid,
                })
            })
            .collect()
    }
}

/// The UUID behind a plugin reference, when the reference is UUID-backed.
fn uuid_of(reference: &str) -> Option<Uuid> {
    Uuid::parse_str(reference).ok()
}

// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-1
// `inst-gf-repo-1`/`-2`: every operation of the aggregate carries the caller's
// `tenant_id`, which the repositories bind before the store is consulted.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-1
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-2
// `inst-gf-repo-2`: the aggregate forwards the caller's tenant to the
// repository boundary rather than deriving it from the record itself.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-2
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-3
// `inst-gf-repo-3`/`-4`: a record owned by a different tenant resolves as
// not-found and is never disclosed.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-3
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-4
// The not-found mapping happens in the repository, so the aggregate has no
// foreign record to leak.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-4
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-5
// `inst-gf-repo-5`/`-6`: a write that would violate a per-tenant uniqueness
// key is a conflict and leaves the store unchanged.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-5
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-6
// The conflict is raised before the write is applied, so the store keeps its
// previous content.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-6
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-7
// `inst-gf-repo-7`: the tenant-scoped result is returned to the caller.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-repo-access:p1:inst-gf-repo-7
impl ControlPlaneService for ControlPlaneServiceImpl {
    fn create_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<Upstream, DomainError> {
        let validated = crate::domain::validation::validate_upstream(
            &upstream,
            self.allow_http_upstream,
        )?;
        let record = UpstreamRecord {
            upstream: validated,
            plugin_bindings: self.bindings_of(
                tenant_id,
                &upstream.plugins.clone().unwrap_or_default().items,
            )?,
        };
        self.upstreams.create(tenant_id, record).map(|r| r.upstream).map_err(|error| {
            if error.is_conflict() {
                self.map_conflict(WriteConflict::UniqueKey)
            } else {
                error
            }
        })
    }

    fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams.get(tenant_id, id).map(|r| r.upstream)
    }

    fn get_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        self.upstreams.get_by_alias(tenant_id, alias).map(|r| r.upstream)
    }

    fn list_upstreams(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError> {
        self.upstreams.list(tenant_id).map(|records| records.into_iter().map(|r| r.upstream).collect())
    }

    fn replace_upstream(&self, tenant_id: Uuid, upstream: Upstream) -> Result<Upstream, DomainError> {
        let stored = self.upstreams.get(tenant_id, upstream.id)?;
        // Alias immutability: the replacement pool may not move the alias, and
        // the alias the verdict returns — the stored one, or the one the
        // replacement pool derives — is the alias that is persisted, so the
        // `(tenant_id, alias)` routing key the data plane resolves follows the
        // pool rather than the caller's supplied spelling.
        let alias = crate::domain::alias::validate_alias_replacement(&stored.upstream, &upstream)?;
        let mut replacement = upstream;
        replacement.alias = alias;
        let validated = crate::domain::validation::validate_upstream(
            &replacement,
            self.allow_http_upstream,
        )?;
        let record = UpstreamRecord {
            upstream: validated,
            plugin_bindings: self.bindings_of(
                tenant_id,
                &replacement.plugins.clone().unwrap_or_default().items,
            )?,
        };
        self.upstreams.replace(tenant_id, record).map(|r| r.upstream).map_err(|error| {
            if error.is_conflict() {
                self.map_conflict(WriteConflict::UniqueKey)
            } else {
                error
            }
        })
    }

    fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.upstreams.delete(tenant_id, id)
    }

    fn create_route(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError> {
        let validated = crate::domain::validation::validate_route(&route)?;
        // The parent upstream must exist inside the same tenant.
        self.upstreams.get(tenant_id, validated.upstream_id)?;
        let record = RouteRecord {
            route: validated,
            plugin_bindings: self.bindings_of(
                tenant_id,
                &route.plugins.clone().unwrap_or_default().items,
            )?,
        };
        self.routes.create(tenant_id, record).map(|r| r.route).map_err(|error| {
            if error.is_conflict() {
                self.map_conflict(WriteConflict::RouteMatch)
            } else {
                error
            }
        })
    }

    fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes.get(tenant_id, id).map(|r| r.route)
    }

    fn list_routes(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        self.routes.list(tenant_id).map(|records| records.into_iter().map(|r| r.route).collect())
    }

    fn list_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        self.routes.list_for_upstream(tenant_id, upstream_id).map(|records| {
            records.into_iter().map(|r| r.route).collect()
        })
    }

    fn replace_route(&self, tenant_id: Uuid, route: Route) -> Result<Route, DomainError> {
        let validated = crate::domain::validation::validate_route(&route)?;
        self.upstreams.get(tenant_id, validated.upstream_id)?;
        let record = RouteRecord {
            route: validated,
            plugin_bindings: self.bindings_of(
                tenant_id,
                &route.plugins.clone().unwrap_or_default().items,
            )?,
        };
        self.routes.replace(tenant_id, record).map(|r| r.route).map_err(|error| {
            if error.is_conflict() {
                self.map_conflict(WriteConflict::RouteMatch)
            } else {
                error
            }
        })
    }

    fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.routes.delete(tenant_id, id)
    }

    fn create_plugin(&self, tenant_id: Uuid, plugin: Plugin) -> Result<Plugin, DomainError> {
        crate::domain::validation::validate_plugin(&plugin)?;
        self.plugins.create(tenant_id, plugin).map_err(|error| {
            if error.is_conflict() {
                self.map_conflict(WriteConflict::UniqueKey)
            } else {
                error
            }
        })
    }

    fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins.get(tenant_id, id)
    }

    fn list_plugins(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError> {
        self.plugins.list(tenant_id)
    }

    fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.plugins.delete(tenant_id, id)
    }

    fn resolve_proxy_target(
        &self,
        tenant_id: Uuid,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<ResolvedProxyTarget, DomainError> {
        let record = self.upstreams.get_by_alias(tenant_id, alias)?;
        if !record.upstream.enabled {
            return Err(DomainError::LinkUnavailable {
                upstream_id: Some(record.upstream.id.to_string()),
                host: None,
                path: Some(path.to_owned()),
                trace_id: None,
            });
        }
        let routes = self.routes.list_for_upstream(tenant_id, record.upstream.id)?;
        let matched = routes
            .into_iter()
            .filter(|r| r.route.enabled)
            .find(|r| route_matches(&r.route, method, path));
        let Some(matched) = matched else {
            return Err(DomainError::RouteNotFound { path: Some(path.to_owned()), trace_id: None });
        };
        let effective = merge_effective(&record, &matched);
        Ok(ResolvedProxyTarget {
            upstream: effective,
            route_id: matched.route.id,
            match_type: matched.route.match_type,
        })
    }
}

/// Whether a route's match block admits `(method, path)`.
fn route_matches(route: &Route, method: &str, path: &str) -> bool {
    match &route.match_ {
        crate::domain::dto::MatchConfig { http: Some(http), grpc: None } => {
            http.methods.iter().any(|m| m.as_str() == method)
                && (path == http.path || path.starts_with(&format!("{}/", http.path.trim_end_matches('/'))))
        }
        // gRPC is configuration surface only (graded deviation 7); the match
        // block is stored but never resolves a proxy target.
        crate::domain::dto::MatchConfig { http: None, grpc: Some(_) } => false,
        _ => false,
    }
}

/// The two-layer merge (upstream base, then route) of a resolved target.
/// Tenant-chain layers are appended by the request path once the tenant chain
/// is resolved; the merge engine takes the whole ordered list.
fn merge_effective(upstream: &UpstreamRecord, route: &RouteRecord) -> EffectiveConfig {
    let layers = [
        merge::upstream_base_layer(&upstream.upstream),
        merge::route_layer(&route.route),
    ];
    merge::merge(
        &layers,
        upstream.upstream.tenant_id,
        &merge::OverridePermissions::NONE,
    )
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod tests;
