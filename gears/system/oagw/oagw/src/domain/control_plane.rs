//! OAGW control plane: the tenant-scoped store behind the management CRUD.
//!
//! Slice S2 completes the management surface: upstream, route and plugin CRUD
//! live on an in-process store, plus the reads the data plane needs (alias
//! lookup, route candidates, plugin usage). The rules that the design assigns
//! to the control plane live here and not in the transport layer: schema
//! validation, alias derivation, alias uniqueness per tenant, alias
//! immutability, route match-rule uniqueness and plugin immutability.

use std::collections::HashMap;

use parking_lot::{Mutex, RwLock};
use uuid::Uuid;

use crate::domain::model::{
    PluginSpec, RouteSpec, UPSTREAM_ID_PREFIX, UpstreamSpec, enforce_alias_update, parse_plugin_id,
    parse_resource_id, plugin_gts_id, resolve_alias, route_gts_id, upstream_gts_id,
};
use crate::error::OagwError;

/// `reason` extension value of a 409 caused by a duplicate alias.
pub const REASON_ALIAS_CONFLICT: &str = "ALIAS_CONFLICT";
/// `reason` extension value of a 409 caused by a plugin still in use.
pub const REASON_PLUGIN_IN_USE: &str = "PLUGIN_IN_USE";
/// `reason` extension value of a 409 caused by a duplicate plugin name.
pub const REASON_PLUGIN_NAME_CONFLICT: &str = "PLUGIN_NAME_CONFLICT";
/// `reason` extension value of a 409 caused by a route whose match rule
/// overlaps an existing route of the same upstream.
pub const REASON_ROUTE_MATCH_CONFLICT: &str = "ROUTE_MATCH_CONFLICT";

/// A stored upstream: the schema-shaped document plus the system fields the
/// schema marks read-only.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRecord {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant (never serialized: the schema forbids extra members).
    pub tenant_id: Uuid,
    /// Normalized routing alias, unique per tenant.
    pub alias: String,
    /// Stored document (`upstream.v1` shape).
    pub spec: UpstreamSpec,
}

impl UpstreamRecord {
    /// The response representation: the stored document with `id` populated
    /// with the GTS identifier (`gts.cf.core.oagw.upstream.v1~{uuid}`) and the
    /// resolved alias always present, even when the request derived it.
    #[must_use]
    pub fn wire(&self) -> UpstreamSpec {
        let mut spec = self.spec.clone();
        spec.id = Some(upstream_gts_id(self.id));
        spec.alias = Some(self.alias.clone());
        spec
    }
}

/// A stored route: the schema-shaped document plus the system fields the schema
/// marks read-only.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRecord {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant (the tenant that owns the upstream, never serialized).
    pub tenant_id: Uuid,
    /// Upstream the route belongs to.
    pub upstream_id: Uuid,
    /// Whether the route takes part in proxy resolution. `route.v1` has no
    /// `enabled` member, so the flag is a stored system field rather than part
    /// of the document: every route created through the management API is
    /// enabled, and the data plane only ever sees enabled routes.
    pub enabled: bool,
    /// Stored document (`route.v1` shape).
    pub spec: RouteSpec,
}

impl RouteRecord {
    /// The response representation: the stored document with `id` populated
    /// with the GTS identifier (`gts.cf.core.oagw.route.v1~{uuid}`) and
    /// `upstream_id` normalized to the GTS form of the referenced upstream.
    #[must_use]
    pub fn wire(&self) -> RouteSpec {
        let mut spec = self.spec.clone();
        spec.id = Some(route_gts_id(self.id));
        spec.upstream_id = upstream_gts_id(self.upstream_id);
        spec
    }

    /// Deterministic list ordering key: `(upstream_id, match path)`.
    #[must_use]
    pub fn order_key(&self) -> (Uuid, String) {
        (
            self.upstream_id,
            self.spec.match_rules.order_path().to_owned(),
        )
    }
}

/// A stored custom (Starlark) plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRecord {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Kind-qualified GTS identifier the plugin is addressed and bound by
    /// (`gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`).
    pub plugin_ref: String,
    /// Stored document (plugin shape).
    pub spec: PluginSpec,
}

impl PluginRecord {
    /// The response representation: the stored document with the read-only
    /// identifiers populated.
    #[must_use]
    pub fn wire(&self) -> PluginSpec {
        let mut spec = self.spec.clone();
        spec.id = Some(self.plugin_ref.clone());
        spec.plugin_ref = Some(self.plugin_ref.clone());
        spec
    }
}

/// The resources that still reference a plugin, used to refuse its deletion.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PluginUsage {
    /// Upstreams binding the plugin (directly or through their auth plugin).
    pub upstreams: Vec<Uuid>,
    /// Routes binding the plugin.
    pub routes: Vec<Uuid>,
}

impl PluginUsage {
    /// `true` when nothing references the plugin any more.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }

    /// GTS identifiers of the referencing resources, for the 409 extensions.
    #[must_use]
    pub fn identifiers(&self) -> Vec<String> {
        self.upstreams
            .iter()
            .map(|id| upstream_gts_id(*id))
            .chain(self.routes.iter().map(|id| route_gts_id(*id)))
            .collect()
    }
}

/// Tenant-scoped index of the stored upstreams.
#[derive(Default)]
struct UpstreamIndex {
    /// Records by identifier.
    by_id: HashMap<Uuid, UpstreamRecord>,
    /// Alias uniqueness per tenant: `(tenant_id, alias) → id`.
    by_alias: HashMap<(Uuid, String), Uuid>,
}

/// Tenant-scoped index of the stored routes.
#[derive(Default)]
struct RouteIndex {
    /// Records by identifier.
    by_id: HashMap<Uuid, RouteRecord>,
}

/// Tenant-scoped index of the stored custom plugins.
#[derive(Default)]
struct PluginIndex {
    /// Records by identifier.
    by_id: HashMap<Uuid, PluginRecord>,
}

/// The OAGW control plane.
///
/// All operations are tenant-scoped: an identifier that exists under another
/// tenant is indistinguishable from a missing one (404), never a 403.
pub struct ControlPlane {
    upstreams: RwLock<UpstreamIndex>,
    routes: RwLock<RouteIndex>,
    plugins: RwLock<PluginIndex>,
    /// Round-robin cursors of the endpoint pools, keyed by upstream. The data
    /// plane owns load balancing, but the cursor is per-upstream state that
    /// every worker must share, so it lives next to the endpoint pool.
    rotations: Mutex<HashMap<Uuid, usize>>,
}

impl Default for ControlPlane {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlPlane {
    /// Create an empty control plane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            upstreams: RwLock::new(UpstreamIndex::default()),
            routes: RwLock::new(RouteIndex::default()),
            plugins: RwLock::new(PluginIndex::default()),
            rotations: Mutex::new(HashMap::new()),
        }
    }

    /// Create an upstream for `tenant_id`.
    ///
    /// The alias is derived from (or validated against) the endpoints, then
    /// reserved for the tenant.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the document is invalid or the alias
    /// rules are violated, and a 409 when the alias is already taken by the
    /// tenant (`reason: ALIAS_CONFLICT`).
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        spec: UpstreamSpec,
    ) -> Result<UpstreamRecord, OagwError> {
        spec.validate()?;
        let alias = resolve_alias(spec.alias.as_deref(), &spec.server.endpoints)?;

        let mut index = self.upstreams.write();
        if index.by_alias.contains_key(&(tenant_id, alias.clone())) {
            return Err(OagwError::conflict(format!(
                "an upstream with alias `{alias}` already exists for this tenant"
            ))
            .with_alias(alias)
            .with_reason(REASON_ALIAS_CONFLICT));
        }

        let id = Uuid::new_v4();
        let mut spec = spec;
        // The stored document always carries the resolved alias, so the wire
        // representation of a derived-alias upstream echoes it.
        spec.alias = Some(alias.clone());
        let record = UpstreamRecord {
            id,
            tenant_id,
            alias,
            spec,
        };
        index.by_alias.insert((tenant_id, record.alias.clone()), id);
        index.by_id.insert(id, record.clone());
        Ok(record)
    }

    /// Fetch an upstream of `tenant_id` by identifier.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such upstream.
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, OagwError> {
        self.upstreams
            .read()
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| upstream_not_found(id))
    }

    /// List the upstreams of `tenant_id`, ordered by alias.
    ///
    /// `top` caps the page and `skip` offsets into the ordered result.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid, top: usize, skip: usize) -> Vec<UpstreamRecord> {
        let mut records: Vec<UpstreamRecord> = self
            .upstreams
            .read()
            .by_id
            .values()
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .collect();
        records.sort_by(|left, right| left.alias.cmp(&right.alias));
        records.into_iter().skip(skip).take(top).collect()
    }

    /// Replace an upstream of `tenant_id` (whole-representation `PUT`).
    ///
    /// The alias is immutable: it is re-derived from the endpoints and must
    /// stay the routing key it was created with.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such upstream, and
    /// a 400 when the document is invalid or the alias rules are violated.
    pub fn update_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: UpstreamSpec,
    ) -> Result<UpstreamRecord, OagwError> {
        spec.validate()?;
        let mut index = self.upstreams.write();
        let existing = index
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| upstream_not_found(id))?;
        enforce_alias_update(&existing.alias, &existing.spec.server.endpoints, &spec)?;

        let mut spec = spec;
        spec.alias = Some(existing.alias.clone());
        let record = UpstreamRecord {
            id,
            tenant_id,
            alias: existing.alias,
            spec,
        };
        index.by_id.insert(id, record.clone());
        Ok(record)
    }

    /// Delete an upstream of `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such upstream.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRecord, OagwError> {
        let mut index = self.upstreams.write();
        let removed = index
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| upstream_not_found(id))?;
        index.by_id.remove(&id);
        index.by_alias.remove(&(tenant_id, removed.alias.clone()));
        let mut routes = self.routes.write();
        routes
            .by_id
            .retain(|_, route| route.upstream_id != id || route.tenant_id != tenant_id);
        Ok(removed)
    }

    /// Look up an upstream of `tenant_id` by (normalized) alias.
    ///
    /// Alias resolution is case-insensitive: the store keeps every alias in its
    /// normalized form, and the caller normalizes the request alias before
    /// asking.
    #[must_use]
    pub fn find_upstream_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<UpstreamRecord> {
        let index = self.upstreams.read();
        index
            .by_alias
            .get(&(tenant_id, alias.to_owned()))
            .and_then(|id| index.by_id.get(id))
            .cloned()
    }

    /// The next round-robin cursor of `upstream_id`'s endpoint pool, for a pool
    /// of `pool_len` endpoints.
    ///
    /// Returns 0 for an empty pool. The cursor advances once per call, so
    /// consecutive resolutions of one multi-endpoint upstream rotate over the
    /// endpoint list.
    #[must_use]
    pub fn next_endpoint_index(&self, upstream_id: Uuid, pool_len: usize) -> usize {
        if pool_len == 0 {
            return 0;
        }
        let mut rotations = self.rotations.lock();
        let cursor = rotations.entry(upstream_id).or_default();
        let index = *cursor % pool_len;
        *cursor = cursor.wrapping_add(1);
        index
    }

    /// Create a route for `tenant_id` under one of its upstreams.
    ///
    /// `upstream_id` must reference an upstream of the calling tenant: the
    /// management API is tenant-scoped, so an upstream of another tenant (an
    /// ancestor resource) is not addressable here and the reference is invalid.
    /// The match rule must be unique within the upstream.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the document is invalid or the upstream
    /// reference does not resolve inside the tenant, and a 409 when an enabled
    /// route of the same upstream already matches the same path with an
    /// overlapping method set (`reason: ROUTE_MATCH_CONFLICT`).
    pub fn create_route(&self, tenant_id: Uuid, spec: RouteSpec) -> Result<RouteRecord, OagwError> {
        spec.validate()?;
        let upstream_id = parse_resource_id(UPSTREAM_ID_PREFIX, &spec.upstream_id)?;
        let upstream = self.own_upstream(tenant_id, upstream_id)?;
        let upstream_id = upstream.id;

        // Lock order `plugins` -> `routes`: the guard is held until the binding
        // is committed, so a concurrent `delete_plugin` cannot remove a plugin
        // this route is about to reference (see `delete_plugin`).
        let _plugins = self.plugins.read();
        let mut routes = self.routes.write();
        if let Some(occupied) =
            Self::find_conflicting_route_excluding(&routes, upstream_id, &spec, Uuid::nil())
        {
            return Err(route_match_conflict(&occupied, &spec));
        }

        let id = Uuid::new_v4();
        let mut spec = spec;
        spec.id = None;
        let record = RouteRecord {
            id,
            tenant_id,
            upstream_id,
            enabled: true,
            spec,
        };
        routes.by_id.insert(id, record.clone());
        Ok(record)
    }

    /// Fetch a route of `tenant_id` by identifier.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such route.
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, OagwError> {
        self.routes
            .read()
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| route_not_found(id))
    }

    /// List the routes of `tenant_id`, ordered by `(upstream_id, match path)`.
    ///
    /// `top` caps the page and `skip` offsets into the ordered result.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid, top: usize, skip: usize) -> Vec<RouteRecord> {
        let mut records: Vec<RouteRecord> = self
            .routes
            .read()
            .by_id
            .values()
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .collect();
        records.sort_by_key(RouteRecord::order_key);
        records.into_iter().skip(skip).take(top).collect()
    }

    /// The enabled routes of one of `tenant_id`'s upstreams, longest match path
    /// first (the data plane takes the first match).
    #[must_use]
    pub fn enabled_routes_for_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Vec<RouteRecord> {
        let mut records: Vec<RouteRecord> = self
            .routes
            .read()
            .by_id
            .values()
            .filter(|record| {
                record.tenant_id == tenant_id && record.upstream_id == upstream_id && record.enabled
            })
            .cloned()
            .collect();
        records.sort_by(|left, right| {
            let left_path = left.spec.match_rules.order_path();
            let right_path = right.spec.match_rules.order_path();
            right_path
                .len()
                .cmp(&left_path.len())
                .then_with(|| left_path.cmp(right_path))
                .then_with(|| left.id.cmp(&right.id))
        });
        records
    }

    /// Replace a route of `tenant_id` (whole-representation `PUT`).
    ///
    /// `upstream_id` is immutable: a body that names another upstream is
    /// rejected. A `PUT` never creates.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such route, a 400
    /// when the document is invalid or `upstream_id` is moved, and a 409 when
    /// the new match rule overlaps another route of the same upstream.
    pub fn update_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        spec: RouteSpec,
    ) -> Result<RouteRecord, OagwError> {
        spec.validate()?;
        let requested_upstream = parse_resource_id(UPSTREAM_ID_PREFIX, &spec.upstream_id)?;
        // Lock order `plugins` -> `routes`, as in `create_route`.
        let _plugins = self.plugins.read();
        let mut routes = self.routes.write();
        let existing = routes
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| route_not_found(id))?;

        if requested_upstream != existing.upstream_id {
            return Err(OagwError::validation(format!(
                "upstream_id `{}` is immutable: a route always belongs to the upstream it was \
                 created under",
                spec.upstream_id
            ))
            .with_invalid_value(spec.upstream_id)
            .with_upstream_id(upstream_gts_id(existing.upstream_id)));
        }
        if let Some(occupied) =
            Self::find_conflicting_route_excluding(&routes, existing.upstream_id, &spec, id)
        {
            return Err(route_match_conflict(&occupied, &spec));
        }

        let mut spec = spec;
        spec.id = None;
        let record = RouteRecord {
            id,
            tenant_id,
            upstream_id: existing.upstream_id,
            enabled: existing.enabled,
            spec,
        };
        routes.by_id.insert(id, record.clone());
        Ok(record)
    }

    /// Delete a route of `tenant_id`.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such route.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRecord, OagwError> {
        let mut routes = self.routes.write();
        let removed = routes
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| route_not_found(id))?;
        routes.by_id.remove(&id);
        Ok(removed)
    }

    /// Create a stored (custom) plugin for `tenant_id`.
    ///
    /// The plugin is immutable: there is no replace operation, a change is a new
    /// plugin plus a re-bind.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the document is invalid, and a 409 when
    /// the tenant already owns a plugin with this name.
    pub fn create_plugin(
        &self,
        tenant_id: Uuid,
        spec: PluginSpec,
    ) -> Result<PluginRecord, OagwError> {
        spec.validate()?;
        let mut plugins = self.plugins.write();
        if plugins
            .by_id
            .values()
            .any(|record| record.tenant_id == tenant_id && record.spec.name == spec.name)
        {
            return Err(OagwError::conflict(format!(
                "a plugin named `{}` already exists for this tenant",
                spec.name
            ))
            .with_invalid_value(&spec.name)
            .with_reason(REASON_PLUGIN_NAME_CONFLICT));
        }

        let id = Uuid::new_v4();
        let mut spec = spec;
        spec.id = None;
        spec.plugin_ref = None;
        let plugin_ref = plugin_gts_id(spec.kind, id);
        let record = PluginRecord {
            id,
            tenant_id,
            plugin_ref,
            spec,
        };
        plugins.by_id.insert(id, record.clone());
        Ok(record)
    }

    /// Fetch a stored plugin of `tenant_id` by identifier.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such plugin.
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, OagwError> {
        self.plugins
            .read()
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| plugin_not_found(id))
    }

    /// List the stored plugins of `tenant_id`, ordered by name.
    ///
    /// `top` caps the page and `skip` offsets into the ordered result.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid, top: usize, skip: usize) -> Vec<PluginRecord> {
        let mut records: Vec<PluginRecord> = self
            .plugins
            .read()
            .by_id
            .values()
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .collect();
        records.sort_by(|left, right| left.spec.name.cmp(&right.spec.name));
        records.into_iter().skip(skip).take(top).collect()
    }

    /// The upstreams and routes of `tenant_id` that still reference `plugin`.
    #[must_use]
    pub fn plugin_usage(&self, tenant_id: Uuid, plugin: &PluginRecord) -> PluginUsage {
        let references = |identifier: &str| -> bool {
            identifier == plugin.plugin_ref
                || parse_plugin_id(identifier).is_ok_and(|parsed| parsed == plugin.id)
        };
        let mut usage = PluginUsage::default();
        for upstream in self.list_upstreams(tenant_id, usize::MAX, 0) {
            let bound =
                upstream.spec.plugins.as_ref().is_some_and(|chain| {
                    chain.items.iter().any(|item| references(item.plugin_ref()))
                }) || upstream
                    .spec
                    .auth
                    .as_ref()
                    .and_then(|auth| auth.auth_type.as_deref())
                    .is_some_and(references);
            if bound {
                usage.upstreams.push(upstream.id);
            }
        }
        for route in self.list_routes(tenant_id, usize::MAX, 0) {
            if route
                .spec
                .plugins
                .as_ref()
                .is_some_and(|chain| chain.items.iter().any(|item| references(item.plugin_ref())))
            {
                usage.routes.push(route.id);
            }
        }
        usage
    }

    /// Delete a stored plugin of `tenant_id`.
    ///
    /// The plugin write guard is taken **first** and held across the usage scan,
    /// so a concurrent `create_upstream` / `create_route` cannot bind the plugin
    /// between the scan and the removal: those writers acquire
    /// [`Self::plugins`] read-only before they commit a `plugin_ref`, in the
    /// documented lock order `plugins` → `upstreams` → `routes`.
    ///
    /// # Errors
    ///
    /// Returns a 404 [`OagwError`] when the tenant owns no such plugin, and a
    /// 409 (`reason: PLUGIN_IN_USE`) when an upstream or a route still binds it.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRecord, OagwError> {
        let mut plugins = self.plugins.write();
        let removed = plugins
            .by_id
            .get(&id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| plugin_not_found(id))?;
        let usage = self.plugin_usage(tenant_id, &removed);
        if !usage.is_empty() {
            return Err(OagwError::conflict(format!(
                "plugin `{}` is referenced by {} upstream(s) and {} route(s)",
                removed.plugin_ref,
                usage.upstreams.len(),
                usage.routes.len()
            ))
            .with_plugin_id(&removed.plugin_ref)
            .with_referenced_by(usage.identifiers())
            .with_reason(REASON_PLUGIN_IN_USE));
        }

        plugins.by_id.remove(&id);
        Ok(removed)
    }

    /// The upstream of `tenant_id` that `upstream_id` addresses.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the tenant owns no such upstream: a
    /// create that references an upstream of another tenant (or an unknown one)
    /// carries an invalid reference, it is not a missing-tenant-resource 404.
    fn own_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<UpstreamRecord, OagwError> {
        self.upstreams
            .read()
            .by_id
            .get(&upstream_id)
            .filter(|record| record.tenant_id == tenant_id)
            .cloned()
            .ok_or_else(|| own_upstream_error(upstream_id))
    }

    /// The stored route of `upstream_id` whose match rule overlaps `spec`,
    /// ignoring `excluded` (the route being replaced by `spec`; pass
    /// [`Uuid::nil`] when no route is being replaced).
    fn find_conflicting_route_excluding(
        routes: &RouteIndex,
        upstream_id: Uuid,
        spec: &RouteSpec,
        excluded: Uuid,
    ) -> Option<RouteRecord> {
        routes
            .by_id
            .values()
            .filter(|route| {
                route.upstream_id == upstream_id && route.id != excluded && route.enabled
            })
            .find(|route| route.spec.match_rules.conflicts_with(&spec.match_rules))
            .cloned()
    }
}

/// Resolve the upstream a write operation addresses, requiring it to belong to
/// `tenant_id`.
///
/// A create that references an upstream of another tenant is a 400 (the
/// management API cannot see ancestor resources), not a 404: the reference
/// itself is invalid for this tenant.
fn own_upstream_error(upstream_id: Uuid) -> OagwError {
    OagwError::validation(format!(
        "upstream_id `{}` does not reference an upstream of this tenant",
        upstream_gts_id(upstream_id)
    ))
    .with_upstream_id(upstream_gts_id(upstream_id))
}

/// 409 problem for a route whose match rule overlaps a stored route.
fn route_match_conflict(existing: &RouteRecord, requested: &RouteSpec) -> OagwError {
    let path = requested.match_rules.http().map_or_else(
        || requested.match_rules.order_path().to_owned(),
        |http| http.path.clone(),
    );
    OagwError::conflict(format!(
        "a route of this upstream already matches path `{path}` for an overlapping method set"
    ))
    .with_upstream_id(upstream_gts_id(existing.upstream_id))
    .with_reason(REASON_ROUTE_MATCH_CONFLICT)
}

/// 404 problem for a route the tenant cannot see.
fn route_not_found(id: Uuid) -> OagwError {
    OagwError::route_not_found(format!(
        "no route `{}` exists for this tenant",
        route_gts_id(id)
    ))
}

/// 404 problem for a plugin the tenant cannot see.
fn plugin_not_found(id: Uuid) -> OagwError {
    OagwError::route_not_found(format!("no plugin exists for this tenant under `{id}`"))
}

/// 404 problem for an upstream the tenant cannot see.
fn upstream_not_found(id: Uuid) -> OagwError {
    OagwError::route_not_found(format!(
        "no upstream `{}` exists for this tenant",
        upstream_gts_id(id)
    ))
    .with_upstream_id(upstream_gts_id(id))
}

#[cfg(test)]
#[path = "control_plane_tests.rs"]
mod tests;
