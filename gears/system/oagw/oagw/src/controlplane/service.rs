// Created: 2026-09-04 by Constructor Tech
//! Control-plane service of the OAGW gear.
//!
//! Implements the management semantics of `docs/DESIGN.md` §3.3 and the FRs of
//! `docs/PRD.md` §5.1 on top of [`ControlPlaneStore`]: server-generated
//! identifiers, alias derivation and per-tenant uniqueness, route → upstream
//! referential integrity, match-rule uniqueness, plugin immutability and
//! in-use protection, and enable/disable without deleting state.
//!
//! Every method is synchronous and returns the single [`OagwError`] model of
//! Phase 1; the HTTP layer (`crate::api`) only projects the outcome.

use std::sync::Arc;

use uuid::Uuid;

use crate::controlplane::store::{ControlPlaneStore, match_key_of};
use crate::domain::plugin::{
    AUTH_APIKEY, AUTH_BASIC, AUTH_BEARER, AUTH_NOOP, AUTH_OAUTH2_CLIENT_CRED,
    AUTH_OAUTH2_CLIENT_CRED_BASIC, GUARD_CORS, GUARD_REQUIRED_HEADERS, GUARD_TIMEOUT,
    TRANSFORM_LOGGING, TRANSFORM_METRICS, TRANSFORM_REQUEST_ID,
};
use crate::domain::{
    AliasRegistration, CorsConfig, Plugin, PluginChain, PluginKind, PluginRef, RateLimitConfig,
    Route, RouteMatch, RouteSpec, Upstream, UpstreamSpec,
};
use crate::error::OagwError;

/// Human-readable 404 for any management resource the calling tenant cannot
/// see (`docs/DESIGN.md` §3.3 "Tenant Scoping"; the PRD maps every
/// not-found case to `RouteNotFound`).
#[must_use]
fn not_found(kind: &str, id: Uuid, tenant_id: Uuid) -> OagwError {
    OagwError::RouteNotFound {
        detail: format!("{kind} {id} does not exist in tenant {tenant_id}"),
    }
}

/// GTS instance identifier of a custom plugin, as used by the plugin
/// reference wire format (`gts.<type>~<uuid>`).
#[must_use]
pub fn plugin_instance_id(plugin: &Plugin) -> String {
    format!("{}{}", plugin.gts_type(), plugin.id.as_simple())
}

/// The full replacement of a route (`docs/DESIGN.md` §3.3 "PUT (Replace)"):
/// every field except the immutable `upstream_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteUpdate {
    /// Match keys.
    pub r#match: RouteMatch,
    /// Route-level plugin chain.
    pub plugins: Option<PluginChain>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS configuration.
    pub cors: Option<CorsConfig>,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat discovery tags.
    pub tags: Vec<String>,
}

/// One entry of the built-in plugin catalog
/// (`docs/ADR/0002-plugin-system.md`): a native implementation exposed to the
/// management surface as a read-only descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltInPlugin {
    /// GTS instance identifier, as referenced from `plugins.items[]` and
    /// `auth.type`.
    pub gts_id: &'static str,
    /// Plugin kind (selects the GTS type).
    pub kind: PluginKind,
    /// Short human-readable name.
    pub name: &'static str,
    /// `true` when the identifier may appear in `plugins.items[]`
    /// (catalog-only identifiers are not bindable).
    pub bindable: bool,
    /// One-line contract summary.
    pub summary: &'static str,
}

impl BuiltInPlugin {
    /// Starlark-equivalent contract returned by
    /// `GET /oagw/v1/plugins/{id}/source` for a built-in plugin.
    ///
    /// Built-in plugins are native Rust implementations; the sketch documents
    /// the same contract in the interpreted form custom plugins use.
    #[must_use]
    pub fn source(&self) -> String {
        let phase = match self.kind {
            PluginKind::Auth => String::from("def on_request(ctx):\n    # Credential injection"),
            PluginKind::Guard => String::from("def on_request(ctx):\n    # Policy enforcement"),
            PluginKind::Transform => String::from("def on_response(ctx):\n    # Mutation"),
        };
        format!(
            "# Built-in plugin {} (native implementation, shown as its Starlark contract)\n\
             # {}\n\
             {phase}\n    ...\n    return ctx.next()\n",
            self.gts_id, self.summary
        )
    }
}

/// The built-in plugin catalog (`docs/ADR/0002-plugin-system.md`): the six
/// bindable identifiers plus the six catalog-only ones.
pub const BUILT_IN_PLUGIN_CATALOG: [BuiltInPlugin; 12] = [
    BuiltInPlugin {
        gts_id: AUTH_NOOP,
        kind: PluginKind::Auth,
        name: "noop",
        bindable: true,
        summary: "Injects no credential; keeps the upstream anonymous.",
    },
    BuiltInPlugin {
        gts_id: AUTH_APIKEY,
        kind: PluginKind::Auth,
        name: "apikey",
        bindable: true,
        summary: "Injects an API key from the credential store into a header or query parameter.",
    },
    BuiltInPlugin {
        gts_id: AUTH_OAUTH2_CLIENT_CRED,
        kind: PluginKind::Auth,
        name: "oauth2_client_cred",
        bindable: true,
        summary: "Performs the OAuth2 client-credentials flow and caches the token.",
    },
    BuiltInPlugin {
        gts_id: AUTH_OAUTH2_CLIENT_CRED_BASIC,
        kind: PluginKind::Auth,
        name: "oauth2_client_cred_basic",
        bindable: true,
        summary: "OAuth2 client-credentials flow authenticating the client with HTTP Basic.",
    },
    BuiltInPlugin {
        gts_id: GUARD_REQUIRED_HEADERS,
        kind: PluginKind::Guard,
        name: "required_headers",
        bindable: true,
        summary: "Rejects requests that miss one of the configured headers.",
    },
    BuiltInPlugin {
        gts_id: TRANSFORM_REQUEST_ID,
        kind: PluginKind::Transform,
        name: "request_id",
        bindable: true,
        summary: "Propagates or generates the X-Request-ID header.",
    },
    BuiltInPlugin {
        gts_id: AUTH_BASIC,
        kind: PluginKind::Auth,
        name: "basic",
        bindable: false,
        summary: "HTTP Basic authentication; data-plane configuration only.",
    },
    BuiltInPlugin {
        gts_id: AUTH_BEARER,
        kind: PluginKind::Auth,
        name: "bearer",
        bindable: false,
        summary: "Bearer token injection; data-plane configuration only.",
    },
    BuiltInPlugin {
        gts_id: GUARD_TIMEOUT,
        kind: PluginKind::Guard,
        name: "timeout",
        bindable: false,
        summary: "Enforces the request timeout; data-plane configuration only.",
    },
    BuiltInPlugin {
        gts_id: GUARD_CORS,
        kind: PluginKind::Guard,
        name: "cors",
        bindable: false,
        summary: "Validates CORS preflight requests; data-plane configuration only.",
    },
    BuiltInPlugin {
        gts_id: TRANSFORM_LOGGING,
        kind: PluginKind::Transform,
        name: "logging",
        bindable: false,
        summary: "Emits request and response logs; data-plane configuration only.",
    },
    BuiltInPlugin {
        gts_id: TRANSFORM_METRICS,
        kind: PluginKind::Transform,
        name: "metrics",
        bindable: false,
        summary: "Collects Prometheus metrics; data-plane configuration only.",
    },
];

/// A plugin as the list/read endpoints render it: either a catalog entry or a
/// registered custom (Starlark) plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDescriptor {
    /// Identifier on the wire: a GTS instance id for a built-in plugin, the
    /// UUID for a custom one.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin kind.
    pub kind: PluginKind,
    /// GTS type of the plugin.
    pub plugin_type: String,
    /// `true` when the plugin is a fixed built-in one.
    pub builtin: bool,
    /// `true` when the identifier may be bound from `plugins.items[]`.
    pub bindable: bool,
    /// `true` once no upstream or route references the plugin anymore.
    pub gc_eligible: bool,
}

impl From<&Plugin> for PluginDescriptor {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin_instance_id(plugin),
            name: plugin.name.clone(),
            kind: plugin.kind,
            plugin_type: plugin.gts_type().to_owned(),
            builtin: false,
            bindable: true,
            gc_eligible: plugin.gc_eligible,
        }
    }
}

/// Starlark source (or the built-in contract) of a plugin, as returned by
/// `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSource {
    /// Descriptor of the plugin the source belongs to.
    pub descriptor: PluginDescriptor,
    /// Source text.
    pub source_code: String,
}

/// Control-plane service: the synchronous authority over upstreams, routes
/// and plugins of one process (`docs/ADR/0006-state-management.md`).
#[derive(Debug, Clone)]
pub struct ControlPlaneService {
    store: Arc<ControlPlaneStore>,
}

impl ControlPlaneService {
    /// Builds a service over `store`.
    #[must_use]
    pub fn new(store: Arc<ControlPlaneStore>) -> Self {
        Self { store }
    }

    /// The store backing this service (shared with the data plane).
    #[must_use]
    pub const fn store(&self) -> &Arc<ControlPlaneStore> {
        &self.store
    }

    /// Built-in plugin with `id`, or `None` when `id` is not a catalog
    /// identifier.
    #[must_use]
    pub fn built_in_plugin(id: &str) -> Option<&'static BuiltInPlugin> {
        BUILT_IN_PLUGIN_CATALOG
            .iter()
            .find(|entry| entry.gts_id == id)
    }

    // ---------------------------------------------------------------- upstreams

    /// Creates an upstream (`docs/DESIGN.md` §3.3 "POST (Create)").
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`Upstream::new`] and
    /// [`OagwError::AliasConflict`] when another upstream of the same tenant
    /// already holds the derived or requested alias.
    pub fn create_upstream(&self, spec: &UpstreamSpec) -> Result<Upstream, OagwError> {
        self.store.write(|state| {
            let upstream = Upstream::new(Uuid::new_v4(), spec)?;
            if let Some(holder) = state.alias_holder(spec.tenant_id, &upstream.alias) {
                return Err(OagwError::AliasConflict {
                    alias: upstream.alias.as_str().to_owned(),
                    existing_upstream_id: holder,
                });
            }
            state.upstreams.push(upstream.clone());
            Ok(upstream)
        })
    }

    /// Replaces an upstream: a full replacement whose alias is recomputed from
    /// the (possibly new) endpoint pool (`docs/DESIGN.md` §3.3 "PUT").
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the upstream is not visible
    /// to the tenant, the validation errors of [`Upstream::new`], and
    /// [`OagwError::AliasConflict`] when the new alias is taken.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: &UpstreamSpec,
    ) -> Result<Upstream, OagwError> {
        self.store.write(|state| {
            let Some(index) = state
                .upstreams
                .iter()
                .position(|upstream| upstream.id == id && upstream.tenant_id == tenant_id)
            else {
                return Err(not_found("upstream", id, tenant_id));
            };
            let replacement = Upstream::new(id, &Self::re_spec(tenant_id, spec))?;
            if let Some(holder) = state.alias_holder_excluding(tenant_id, &replacement.alias, id) {
                return Err(OagwError::AliasConflict {
                    alias: replacement.alias.as_str().to_owned(),
                    existing_upstream_id: holder,
                });
            }
            state.upstreams[index] = replacement.clone();
            Ok(replacement)
        })
    }

    /// Rewrites the identity fields of a replacement spec: `id` and
    /// `tenant_id` are immutable, so they are taken from the resource.
    fn re_spec(tenant_id: Uuid, spec: &UpstreamSpec) -> UpstreamSpec {
        let mut spec = spec.clone();
        spec.tenant_id = tenant_id;
        spec
    }

    /// The tenant's upstream with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the upstream does not belong
    /// to the tenant (ancestor resources are invisible).
    pub fn upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, OagwError> {
        self.store
            .read(|state| state.upstream(tenant_id, id).cloned())
            .ok_or_else(|| not_found("upstream", id, tenant_id))
    }

    /// All upstreams of the tenant, in registration order.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<Upstream> {
        self.store.read(|state| {
            state
                .upstreams
                .iter()
                .filter(|upstream| upstream.tenant_id == tenant_id)
                .cloned()
                .collect()
        })
    }

    /// Deletes an unreferenced upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the upstream is invisible to
    /// the tenant and [`OagwError::Conflict`] when a route still points at it.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        self.store.write(|state| {
            let index = state
                .upstreams
                .iter()
                .position(|upstream| upstream.id == id && upstream.tenant_id == tenant_id)
                .ok_or_else(|| not_found("upstream", id, tenant_id))?;
            if let Some(user) = state.upstream_used_by(tenant_id, id) {
                return Err(OagwError::Conflict {
                    detail: format!("upstream {id} is still referenced by {user}"),
                });
            }
            state.upstreams.remove(index);
            Ok(())
        })
    }

    // ---------------------------------------------------------------- routes

    /// Creates a route (`docs/DESIGN.md` §3.3 "POST (Create)").
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when `spec.upstream_id` does not
    /// belong to the tenant, the validation errors of [`Route::new`], and
    /// [`OagwError::Conflict`] when the match rule is already claimed.
    pub fn create_route(&self, spec: &RouteSpec) -> Result<Route, OagwError> {
        self.store.write(|state| {
            if state.upstream(spec.tenant_id, spec.upstream_id).is_none() {
                return Err(not_found("upstream", spec.upstream_id, spec.tenant_id));
            }
            let route = Route::new(Uuid::new_v4(), spec)?;
            Self::ensure_match_key_unique(state, &route)?;
            state.routes.push(route.clone());
            Ok(route)
        })
    }

    /// Replaces a route. `upstream_id` is immutable
    /// (`docs/DESIGN.md` §3.3 "PUT (Replace)").
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the route is invisible to the
    /// tenant, the validation errors of [`Route::new`], and
    /// [`OagwError::Conflict`] when the match rule is already claimed.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: &RouteUpdate,
    ) -> Result<Route, OagwError> {
        self.store.write(|state| {
            let Some(index) = state
                .routes
                .iter()
                .position(|route| route.id == id && route.tenant_id == tenant_id)
            else {
                return Err(not_found("route", id, tenant_id));
            };
            let upstream_id = state.routes[index].upstream_id;
            let spec = RouteSpec {
                tenant_id,
                upstream_id,
                r#match: update.r#match.clone(),
                plugins: update.plugins.clone(),
                rate_limit: update.rate_limit.clone(),
                cors: update.cors.clone(),
                enabled: update.enabled,
                tags: update.tags.clone(),
            };
            let replacement = Route::new(id, &spec)?;
            Self::ensure_match_key_unique(state, &replacement)?;
            state.routes[index] = replacement.clone();
            Ok(replacement)
        })
    }

    /// Rejects a route whose match keys are already claimed by a sibling
    /// route of the same upstream.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Conflict`] naming the colliding route.
    fn ensure_match_key_unique(
        state: &mut crate::controlplane::store::StoreState,
        route: &Route,
    ) -> Result<(), OagwError> {
        let key = match_key_of(route);
        if let Some(holder) = state.route_with_match_key(route.tenant_id, route.upstream_id, &key)
            && holder != route.id
        {
            return Err(OagwError::Conflict {
                detail: format!("match rule '{key}' is already claimed by route {holder}"),
            });
        }
        Ok(())
    }

    /// The tenant's route with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the route is invisible to the
    /// tenant.
    pub fn route(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, OagwError> {
        self.store
            .read(|state| state.route(tenant_id, id).cloned())
            .ok_or_else(|| not_found("route", id, tenant_id))
    }

    /// All routes of the tenant, in registration order.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<Route> {
        self.store.read(|state| {
            state
                .routes
                .iter()
                .filter(|route| route.tenant_id == tenant_id)
                .cloned()
                .collect()
        })
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the route is invisible to the
    /// tenant.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        self.store.write(|state| {
            let index = state
                .routes
                .iter()
                .position(|route| route.id == id && route.tenant_id == tenant_id)
                .ok_or_else(|| not_found("route", id, tenant_id))?;
            state.routes.remove(index);
            Ok(())
        })
    }

    // ---------------------------------------------------------------- plugins

    /// Registers an immutable custom (Starlark) plugin
    /// (`docs/DESIGN.md` §3.3: plugins are immutable, no `PUT`).
    ///
    /// # Errors
    ///
    /// Returns the validation errors of [`Plugin::new`] and
    /// [`OagwError::Conflict`] when the tenant already registered a plugin
    /// with the same name.
    pub fn register_plugin(
        &self,
        tenant_id: Uuid,
        kind: PluginKind,
        name: String,
        source: String,
    ) -> Result<Plugin, OagwError> {
        self.store.write(|state| {
            if let Some(existing) = state.plugin_by_name(tenant_id, &name) {
                return Err(OagwError::Conflict {
                    detail: format!(
                        "a {} plugin named '{name}' is already registered as {}",
                        existing.kind,
                        plugin_instance_id(existing)
                    ),
                });
            }
            let plugin = Plugin::new(Uuid::new_v4(), tenant_id, kind, name, source)?;
            state.plugins.push(plugin.clone());
            Ok(plugin)
        })
    }

    /// The tenant's custom plugin with `id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the plugin is not a
    /// registered custom plugin of the tenant.
    pub fn plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, OagwError> {
        self.store
            .read(|state| state.plugin(tenant_id, id).cloned())
            .ok_or_else(|| not_found("plugin", id, tenant_id))
    }

    /// Descriptor of `id`, which may be a built-in catalog identifier or a
    /// registered custom plugin UUID.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the identifier resolves to
    /// neither.
    pub fn plugin_descriptor(
        &self,
        tenant_id: Uuid,
        id: &str,
    ) -> Result<PluginDescriptor, OagwError> {
        if let Some(builtin) = Self::built_in_plugin(id) {
            return Ok(Self::catalog_descriptor(builtin));
        }
        let parsed = Self::parse_plugin_id(id, tenant_id)?;
        self.store
            .read(|state| state.plugin(tenant_id, parsed).map(PluginDescriptor::from))
            .ok_or_else(|| not_found("plugin", parsed, tenant_id))
    }

    /// All plugins visible to the tenant: the built-in catalog first, then the
    /// registered custom plugins in registration order.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<PluginDescriptor> {
        let mut descriptors: Vec<PluginDescriptor> = BUILT_IN_PLUGIN_CATALOG
            .iter()
            .map(Self::catalog_descriptor)
            .collect();
        let custom = self.store.read(|state| {
            state
                .plugins
                .iter()
                .filter(|plugin| plugin.tenant_id == tenant_id)
                .map(PluginDescriptor::from)
                .collect::<Vec<_>>()
        });
        descriptors.extend(custom);
        descriptors
    }

    /// Read-only descriptor of a catalog entry.
    #[must_use]
    fn catalog_descriptor(entry: &BuiltInPlugin) -> PluginDescriptor {
        PluginDescriptor {
            id: entry.gts_id.to_owned(),
            name: entry.name.to_owned(),
            kind: entry.kind,
            plugin_type: entry.kind.gts_type().to_owned(),
            builtin: true,
            bindable: entry.bindable,
            gc_eligible: false,
        }
    }

    /// Starlark source of `id`: the stored source of a custom plugin, or the
    /// documented contract of a built-in one.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when `id` resolves to neither a
    /// built-in plugin nor a custom plugin of the tenant.
    pub fn plugin_source(&self, tenant_id: Uuid, id: &str) -> Result<PluginSource, OagwError> {
        if let Some(builtin) = Self::built_in_plugin(id) {
            return Ok(PluginSource {
                descriptor: Self::catalog_descriptor(builtin),
                source_code: builtin.source(),
            });
        }
        let parsed = Self::parse_plugin_id(id, tenant_id)?;
        self.store
            .read(|state| {
                state.plugin(tenant_id, parsed).map(|plugin| PluginSource {
                    descriptor: PluginDescriptor::from(plugin),
                    source_code: plugin.source.clone(),
                })
            })
            .ok_or_else(|| not_found("plugin", parsed, tenant_id))
    }

    /// Parses a plugin path segment: a bare UUID for a custom plugin, or the
    /// GTS instance id `docs/DESIGN.md` §3.1 uses for a custom plugin
    /// (`{plugin_type}{uuid}`, whose instance part is the plugin UUID).
    fn parse_plugin_id(id: &str, tenant_id: Uuid) -> Result<Uuid, OagwError> {
        let trimmed = id.trim();
        if let Ok(parsed) = Uuid::parse_str(trimmed) {
            return Ok(parsed);
        }
        if let Some(instance) = PluginKind::ALL
            .iter()
            .find_map(|kind| trimmed.strip_prefix(kind.gts_type()))
        {
            if let Ok(parsed) = Uuid::parse_str(instance) {
                return Ok(parsed);
            }
            return Err(OagwError::RouteNotFound {
                detail: format!(
                    "plugin '{id}' is neither a built-in plugin nor a custom plugin of tenant {tenant_id}"
                ),
            });
        }
        Err(OagwError::Validation {
            detail: format!("'{id}' must be a plugin UUID or a built-in plugin GTS identifier"),
        })
    }

    /// Deletes a custom plugin that no upstream or route references anymore.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the plugin is invisible to
    /// the tenant and [`OagwError::PluginInUse`] when it is still bound.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: &str) -> Result<(), OagwError> {
        let id = Self::parse_plugin_id(id, tenant_id)?;
        self.delete_plugin_by_id(tenant_id, id)
    }

    /// Deletes the custom plugin with the resolved `id`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::RouteNotFound`] when the plugin is invisible to
    /// the tenant and [`OagwError::PluginInUse`] when it is still bound.
    fn delete_plugin_by_id(&self, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
        self.store.write(|state| {
            let index = state
                .plugins
                .iter()
                .position(|plugin| plugin.id == id && plugin.tenant_id == tenant_id)
                .ok_or_else(|| not_found("plugin", id, tenant_id))?;
            if state.plugin_used_by(tenant_id, id).is_some() {
                return Err(OagwError::PluginInUse {
                    plugin_ref: plugin_instance_id(&state.plugins[index]),
                });
            }
            state.plugins.remove(index);
            Ok(())
        })
    }

    /// Alias registrations of the tenant, used by the alias-uniqueness
    /// invariant of [`crate::domain::ensure_alias_unique`].
    #[must_use]
    pub fn alias_registrations(&self, tenant_id: Uuid) -> Vec<AliasRegistration> {
        self.store.read(|state| {
            state
                .upstreams
                .iter()
                .filter(|upstream| upstream.tenant_id == tenant_id)
                .map(|upstream| {
                    AliasRegistration::new(tenant_id, upstream.id, upstream.alias.clone())
                })
                .collect()
        })
    }

    /// Resolves a `plugins.items[]` identifier from the wire
    /// (`docs/PRD.md` `cpt-cf-oagw-fr-builtin-plugins`): a built-in identifier
    /// must be in the catalog *and* bindable, a UUID must name a custom plugin
    /// of the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown, non-bindable or
    /// wrong-kind identifier.
    pub fn resolve_plugin_ref(&self, tenant_id: Uuid, raw: &str) -> Result<PluginRef, OagwError> {
        match Self::identify_plugin(raw)? {
            PluginIdentity::BuiltIn(entry) if !entry.bindable => Err(OagwError::Validation {
                detail: format!(
                    "'{raw}' is a catalog-only identifier and cannot be bound from plugins.items[]"
                ),
            }),
            PluginIdentity::BuiltIn(entry) => PluginRef::parse(entry.kind, entry.gts_id),
            PluginIdentity::Custom(id) => {
                let plugin = self.plugin(tenant_id, id)?;
                PluginRef::parse(plugin.kind, id.as_simple().to_string().as_str())
            }
        }
    }

    /// Resolves an `auth.type` identifier from the wire: unlike
    /// [`ControlPlaneService::resolve_plugin_ref`] this slot also accepts the
    /// catalog-only auth plugins (`basic`, `bearer`) but must resolve to an
    /// *auth* plugin.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown or non-auth plugin.
    pub fn resolve_auth_plugin_ref(
        &self,
        tenant_id: Uuid,
        raw: &str,
    ) -> Result<PluginRef, OagwError> {
        match Self::identify_plugin(raw)? {
            PluginIdentity::BuiltIn(entry) if entry.kind != PluginKind::Auth => {
                Err(OagwError::Validation {
                    detail: format!("auth.type must be an auth plugin, not '{raw}'"),
                })
            }
            PluginIdentity::BuiltIn(entry) => PluginRef::parse(PluginKind::Auth, entry.gts_id),
            PluginIdentity::Custom(id) => {
                let plugin = self.plugin(tenant_id, id)?;
                if plugin.kind != PluginKind::Auth {
                    return Err(OagwError::Validation {
                        detail: format!(
                            "custom plugin '{}' is a {} plugin and cannot be used as auth.type",
                            plugin_instance_id(&plugin),
                            plugin.kind
                        ),
                    });
                }
                PluginRef::parse(PluginKind::Auth, id.as_simple().to_string().as_str())
            }
        }
    }

    /// Classifies a plugin identifier from the wire
    /// (`docs/DESIGN.md` §3.1 "Resolution Algorithm"): the instance part after
    /// `~` decides between the in-process registry (a built-in catalog id) and
    /// a custom plugin of the tenant (a UUID, bare or behind the type prefix).
    fn identify_plugin(raw: &str) -> Result<PluginIdentity, OagwError> {
        let trimmed = raw.trim();
        if let Ok(id) = Uuid::parse_str(trimmed) {
            return Ok(PluginIdentity::Custom(id));
        }
        for kind in PluginKind::ALL {
            let Some(instance) = trimmed.strip_prefix(kind.gts_type()) else {
                continue;
            };
            if let Some(entry) = Self::built_in_plugin(trimmed) {
                if gts::GtsInstanceId::try_new(trimmed).is_err() {
                    return Err(OagwError::Validation {
                        detail: format!("'{raw}' is not a valid GTS instance identifier"),
                    });
                }
                return Ok(PluginIdentity::BuiltIn(entry));
            }
            return match Uuid::parse_str(instance) {
                Ok(id) => Ok(PluginIdentity::Custom(id)),
                Err(_) => Err(OagwError::Validation {
                    detail: format!("'{raw}' is not a known built-in {kind} plugin"),
                }),
            };
        }
        Err(OagwError::Validation {
            detail: format!("'{raw}' must be a built-in plugin GTS id or a custom plugin UUID"),
        })
    }
}

/// Classification of a plugin identifier found on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginIdentity {
    /// A fixed built-in plugin.
    BuiltIn(&'static BuiltInPlugin),
    /// A custom plugin registered by the tenant.
    Custom(Uuid),
}
