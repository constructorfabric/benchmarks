//! In-process transactional store of the management half.
//!
//! The persisted model is the ten-table set the two management features own —
//! `oagw_upstream`, `oagw_route`, `oagw_route_http_match`,
//! `oagw_route_grpc_match`, `oagw_route_method`, `oagw_upstream_tag`,
//! `oagw_route_tag`, `oagw_plugin`, `oagw_upstream_plugin`, and
//! `oagw_route_plugin` — modelled as one in-process table set guarded by a
//! single [`parking_lot::RwLock`]. Every multi-row write is one batch: the
//! batch is applied against a cloned table set and the clone is swapped in
//! only when every uniqueness check and the whole batch have succeeded, so a
//! failed write leaves no partial rows. The table shapes are plain and no
//! backend-specific feature is used, so portability across the PostgreSQL,
//! MySQL, and SQLite backends holds by construction; a real database handle
//! for those backends is a platform provisioning concern, not something this
//! store provisions.
//!
//! ## Access paths
//!
//! - The tenant-scoped scan is the read path over each table: every read and
//!   write method takes the calling tenant and applies it in the same
//!   predicate as the other keys, so a scan never yields a row whose
//!   `tenant_id` differs from the caller's.
//! - [`MatchKey`] is the derived `(upstream_id, path, priority, method)` →
//!   `route_id` index the match-uniqueness check looks up; every write and
//!   every delete maintains it.
//! - Neither plugin-binding table carries a foreign key to `oagw_plugin`,
//!   because named plugins have no row there; a plugin deletion therefore
//!   removes no binding row. A binding row does name its parent, so the
//!   parent's deletion cascades into its binding rows.
//! - A custom plugin's `gc_eligible_at` column is set once, by the eligibility
//!   recompute a binding write runs or by the periodic job, at the moment the
//!   row's reference set becomes empty, and is cleared the moment a reference
//!   returns.

/// Garbage-collection TTL of an unlinked custom plugin: 30 days in seconds,
/// the default DESIGN §3.2 Plugin Lifecycle Management gives the configurable
/// TTL. It is a constant here and not an `OagwConfig` key, because the
/// configuration surface DECOMPOSITION §2.1 declares closes at five keys.
pub const PLUGIN_GC_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// The unix-second clock the write paths stamp their eligibility marking with.
///
/// A stored instant is only ever compared with another stored instant, so a
/// single monotonic-enough wall clock read once per write is enough, and no
/// persistence type enters the signature.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

// @cpt-dod:cpt-cf-oagw-dod-plugin-persistence:p1

use std::collections::{BTreeMap, BTreeSet};

use parking_lot::RwLock;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::alias::Alias;
use crate::domain::plugin::Plugin;
use crate::domain::plugin_contract::PluginFamily;
use crate::domain::route::{GrpcMatch, HttpMatch, Route};
use crate::domain::upstream::Upstream;

/// Key of the `oagw_route_method` table: one row per declared method of a
/// route.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteMethodKey {
    /// Owning route.
    pub route_id: Uuid,
    /// Declared HTTP method.
    pub method: String,
}

/// Key of the derived enabled-match index: the `(upstream_id, path, priority,
/// method)` tuple two enabled routes of one upstream may not share.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchKey {
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Match path.
    pub path: String,
    /// Match-uniqueness ordering.
    pub priority: i64,
    /// Declared HTTP method.
    pub method: String,
}

/// The `oagw_upstream` row plus its dependent `oagw_upstream_tag` rows.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRow {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The tag rows of this parent, in sorted order.
    pub tags: Vec<String>,
    /// The upstream row content; its `tags` projection is materialized from
    /// the tag table, so `upstream.tags` and `tags` always agree.
    pub upstream: Upstream,
    /// The `auth_plugin_ref` column: the identifier of the one auth plugin the
    /// upstream's `auth` sub-configuration binds, when it binds one. The
    /// plugin system writes both columns inside the parent's transaction; no
    /// other column of the row is its to write.
    pub auth_plugin_ref: Option<String>,
    /// The `auth_plugin_uuid` column: the extracted UUID of that plugin, when
    /// it is UUID-backed. The scalar column is what keeps the in-use check off
    /// JSON scanning (DESIGN §3.1).
    pub auth_plugin_uuid: Option<Uuid>,
}

/// The `oagw_route` row plus its dependent match, method, and tag rows.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteRow {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The tag rows of this parent, in sorted order.
    pub tags: Vec<String>,
    /// The route row content; its `tags` projection is materialized from the
    /// tag table, so `route.tags` and `tags` always agree.
    pub route: Route,
}

/// The `oagw_plugin` row: one custom tenant-defined plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRow {
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// The plugin row content.
    pub plugin: Plugin,
}

/// The `oagw_upstream_plugin` / `oagw_route_plugin` row content: one binding
/// of one plugin into one parent's chain.
///
/// Neither table carries a foreign key to `oagw_plugin`, because named plugins
/// have no row there; the reference is carried on every row and the UUID only
/// on a UUID-backed one.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginBinding {
    /// Chain position of the binding, contiguous from 0 within its parent.
    pub position: u32,
    /// The canonical plugin identifier the binding was written with.
    pub plugin_ref: String,
    /// The extracted UUID, present only for a UUID-backed plugin.
    pub plugin_uuid: Option<Uuid>,
    /// The plugin configuration the binding carries.
    pub config: Value,
}

/// What one pass of the periodic garbage-collection job did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginGcReport {
    /// The rows the pass marked eligible, each now carrying the instant its
    /// TTL elapses.
    pub marked: Vec<Uuid>,
    /// The rows the pass deleted, whose TTL had elapsed with no reference
    /// left.
    pub collected: Vec<Uuid>,
}

/// The plugin write set a parent write carries: the full replacement of the
/// parent's binding rows, and for an upstream the scalar identity columns of
/// its one auth plugin.
///
/// The set is built by the plugin system's binding validation and consumed by
/// the parent's single-transaction write, so the two land together or not at
/// all. A parent written without a plugin body carries [`BindingWrite::none`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BindingWrite {
    /// The binding rows to write, in position order.
    pub bindings: Vec<PluginBinding>,
    /// The auth plugin identity to write to the upstream's scalar columns.
    pub auth: Option<AuthIdentity>,
    /// Unix seconds the eligibility recompute marks an unlinked row at.
    pub marked_at: u64,
}

impl BindingWrite {
    /// The empty write set: no binding row, no auth plugin, and the marking
    /// instant a plain parent write reaches the eligibility recompute with.
    #[must_use]
    pub fn none(marked_at: u64) -> Self {
        Self {
            bindings: Vec::new(),
            auth: None,
            marked_at,
        }
    }

    /// The identifiers of the custom plugins this write references, which are
    /// the rows whose eligibility the write may change.
    #[must_use]
    pub fn referenced_uuids(&self) -> BTreeSet<Uuid> {
        self.bindings.iter().filter_map(|b| b.plugin_uuid).chain(self.auth.iter().filter_map(|a| a.plugin_uuid)).collect()
    }
}

/// The auth plugin identity an upstream row carries in its two scalar columns:
/// the canonical reference always, and the UUID only when the plugin is
/// UUID-backed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthIdentity {
    /// The canonical plugin identifier the auth sub-configuration named.
    pub plugin_ref: String,
    /// The extracted UUID, present only for a UUID-backed plugin.
    pub plugin_uuid: Option<Uuid>,
}

/// Why a write batch was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Another upstream of the same tenant already holds the alias.
    #[error("another upstream of the tenant already holds the alias")]
    AliasConflict,
    /// Another enabled route of the same upstream already holds the key.
    #[error("another enabled route of the upstream already holds the match key")]
    MatchConflict {
        /// Identifier of the colliding route.
        colliding_route_id: Uuid,
    },
    /// Another plugin of the same tenant already holds the name.
    #[error("another plugin of the tenant already holds the name")]
    PluginNameConflict,
    /// An internal invariant of the persisted model was breached; nothing was
    /// written.
    #[error("persisted model invariant breached: {reason}")]
    Invariant {
        /// Which invariant was breached.
        reason: String,
    },
}

/// The ten tables, keyed as the persisted model names them.
///
/// @cpt-begin:cpt-cf-oagw-dod-persisted-model:p1:inst-table-shapes
#[derive(Debug, Clone, Default)]
struct Tables {
    /// `oagw_upstream`, keyed on `id`.
    upstreams: BTreeMap<Uuid, UpstreamRow>,
    /// `oagw_route`, keyed on `id`.
    routes: BTreeMap<Uuid, RouteRow>,
    /// `oagw_route_http_match`, keyed on `route_id`.
    route_http_match: BTreeMap<Uuid, HttpMatch>,
    /// `oagw_route_grpc_match`, keyed on `route_id`.
    route_grpc_match: BTreeMap<Uuid, GrpcMatch>,
    /// `oagw_route_method`, keyed on `(route_id, method)`.
    route_methods: BTreeSet<RouteMethodKey>,
    /// `oagw_upstream_tag`, keyed on `(parent_id, tag)`.
    upstream_tags: BTreeSet<(Uuid, String)>,
    /// `oagw_route_tag`, keyed on `(parent_id, tag)`.
    route_tags: BTreeSet<(Uuid, String)>,
    /// The derived `(upstream_id, path, priority, method)` → `route_id` index
    /// over the enabled routes.
    enabled_match_index: BTreeMap<MatchKey, Uuid>,
    /// `oagw_plugin`, keyed on `id`.
    plugins: BTreeMap<Uuid, PluginRow>,
    /// `oagw_upstream_plugin`, keyed on `(parent_id, position)`.
    upstream_plugins: BTreeMap<(Uuid, u32), PluginBinding>,
    /// `oagw_route_plugin`, keyed on `(parent_id, position)`.
    route_plugins: BTreeMap<(Uuid, u32), PluginBinding>,
}
// @cpt-end:cpt-cf-oagw-dod-persisted-model:p1:inst-table-shapes

impl Tables {
    /// Rewrites the tag rows of an upstream from the value being stored.
    fn sync_upstream_tags(&mut self, id: Uuid, tags: &[String]) {
        self.upstream_tags.retain(|(parent, _)| *parent != id);
        for tag in tags {
            self.upstream_tags.insert((id, tag.clone()));
        }
    }

    /// Rewrites the tag rows of a route from the value being stored.
    fn sync_route_tags(&mut self, id: Uuid, tags: &[String]) {
        self.route_tags.retain(|(parent, _)| *parent != id);
        for tag in tags {
            self.route_tags.insert((id, tag.clone()));
        }
    }

    /// Rewrites the binding rows of an upstream from the write being stored.
    fn sync_upstream_plugins(&mut self, id: Uuid, bindings: &[PluginBinding]) {
        self.upstream_plugins.retain(|(parent, _), _| *parent != id);
        for binding in bindings {
            self.upstream_plugins.insert((id, binding.position), binding.clone());
        }
    }

    /// Rewrites the binding rows of a route from the write being stored.
    fn sync_route_plugins(&mut self, id: Uuid, bindings: &[PluginBinding]) {
        self.route_plugins.retain(|(parent, _), _| *parent != id);
        for binding in bindings {
            self.route_plugins.insert((id, binding.position), binding.clone());
        }
    }

    /// The identifiers of the custom plugins the parent row references, before
    /// or after a write: the binding rows' UUID-backed references and the
    /// upstream's scalar `auth_plugin_uuid` column.
    fn referenced_uuids(&self, tenant_id: Uuid, parent_id: Uuid) -> BTreeSet<Uuid> {
        let mut referenced = BTreeSet::new();
        for ((parent, _), binding) in &self.upstream_plugins {
            if *parent == parent_id && let Some(uuid) = binding.plugin_uuid {
                referenced.insert(uuid);
            }
        }
        for ((parent, _), binding) in &self.route_plugins {
            if *parent == parent_id && let Some(uuid) = binding.plugin_uuid {
                referenced.insert(uuid);
            }
        }
        if let Some(row) = self.upstreams.get(&parent_id)
            && row.tenant_id == tenant_id
            && let Some(uuid) = row.auth_plugin_uuid
        {
            referenced.insert(uuid);
        }
        referenced
    }

    /// Recomputes the garbage-collection eligibility of the custom plugins
    /// whose reference set the write may have changed.
    ///
    /// A row whose reference set the write emptied is marked eligible, and one
    /// that gained a reference loses the marking, so a plugin rebound before
    /// the TTL elapses never disappears under a live binding. A row that is
    /// already marked and stays unlinked keeps the marking it holds: the
    /// marking happens once, at the moment the last reference is lost. The
    /// marking stores the instant the TTL elapses, because the column is
    /// declared as the instant after which the row is collectable and the job
    /// deletes the rows whose stored instant is in the past.
    fn recompute_plugin_eligibility(
        &mut self,
        _tenant_id: Uuid,
        changed: &BTreeSet<Uuid>,
        marked_at: u64,
    ) {
        for uuid in changed {
            let referenced = plugin_is_referenced(self, *uuid);
            let Some(row) = self.plugins.get_mut(uuid) else {
                continue;
            };
            if referenced {
                // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-relink-if
                // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-relink
                // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-relink
                row.plugin.gc_eligible_at = None;
                // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-relink
                // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-relink
                // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-relink-if
            } else if row.plugin.gc_eligible_at.is_none() {
                // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-unlink-if
                // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-unlink
                // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-unlink
                row.plugin.gc_eligible_at = Some(marked_at.saturating_add(PLUGIN_GC_TTL_SECS));
                // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-unlink
                // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-unlink
                // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-unlink-if
            }
        }
    }

    /// Drops the match, method, and index rows of one route, so a replacement
    /// is checked against every other route before its own rows are recorded.
    fn clear_route_matches(&mut self, route_id: Uuid) {
        self.route_http_match.remove(&route_id);
        self.route_grpc_match.remove(&route_id);
        self.route_methods.retain(|key| key.route_id != route_id);
        self.enabled_match_index.retain(|_, held| *held != route_id);
    }

    /// Records the match, method, and index rows of a route.
    ///
    /// Only an enabled route contributes index rows: the disabled-route
    /// exemption of the match-uniqueness check is what makes a disabled route
    /// free to share its key.
    fn record_route_matches(&mut self, route: &Route) {
        if let Some(http) = &route.match_config.http {
            self.route_http_match.insert(route.id, http.clone());
            // @cpt-begin:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-set
            for method in &http.methods {
                self.route_methods.insert(RouteMethodKey {
                    route_id: route.id,
                    method: method.clone(),
                });
                if route.enabled.unwrap_or_default() {
                    self.enabled_match_index
                        .insert(match_key(route, &http.path, method), route.id);
                }
            }
            // @cpt-end:cpt-cf-oagw-algo-match-uniqueness:p1:inst-match-set
        }
        if let Some(grpc) = &route.match_config.grpc {
            self.route_grpc_match.insert(route.id, grpc.clone());
        }
    }

    /// Drops the dependent rows of one route.
    fn drop_route_dependents(&mut self, route_id: Uuid) {
        self.route_http_match.remove(&route_id);
        self.route_grpc_match.remove(&route_id);
        self.route_methods.retain(|key| key.route_id != route_id);
        self.route_tags.retain(|(parent, _)| *parent != route_id);
        self.enabled_match_index.retain(|_, held| *held != route_id);
    }

    /// Checks the invariants a batch must preserve before it is swapped in.
    fn verify(&self) -> Result<(), StoreError> {
        self.verify_parents()?;
        self.verify_dependents()?;
        self.verify_plugins()?;
        self.verify_index()
    }

    /// Every route names an upstream of its own tenant.
    fn verify_parents(&self) -> Result<(), StoreError> {
        for row in self.routes.values() {
            let Some(parent) = self.upstreams.get(&row.route.upstream_id) else {
                return Err(missing_upstream());
            };
            if parent.tenant_id != row.tenant_id {
                return Err(mismatched_tenant());
            }
        }
        Ok(())
    }

    /// Every dependent row names an existing route or upstream.
    fn verify_dependents(&self) -> Result<(), StoreError> {
        let http_routes = self.route_http_match.keys().copied();
        let grpc_routes = self.route_grpc_match.keys().copied();
        for route_id in http_routes.chain(grpc_routes) {
            self.assert_route(route_id)?;
        }
        for key in &self.route_methods {
            self.assert_route(key.route_id)?;
        }
        for (parent, _) in &self.route_tags {
            self.assert_route(*parent)?;
        }
        for (parent, _) in &self.upstream_tags {
            if !self.upstreams.contains_key(parent) {
                return Err(StoreError::Invariant {
                    reason: String::from("tag row references a missing upstream"),
                });
            }
        }
        for (parent, _) in self.upstream_plugins.keys() {
            self.assert_upstream(*parent)?;
        }
        for (parent, _) in self.route_plugins.keys() {
            self.assert_route(*parent)?;
        }
        Ok(())
    }

    /// Every plugin row is unique on `(tenant_id, name)` and every upstream
    /// names its auth plugin identity at most implicitly: a UUID column is
    /// carried only beside a reference.
    fn verify_plugins(&self) -> Result<(), StoreError> {
        let mut held: Vec<(Uuid, &str)> = Vec::new();
        for row in self.plugins.values() {
            if PluginFamily::from_type_literal(&row.plugin.plugin_type).is_none() {
                return Err(StoreError::Invariant {
                    reason: String::from("plugin row carries a plugin_type that names no family"),
                });
            }
            let name = row.plugin.name.as_str();
            if held
                .iter()
                .any(|(tenant, held_name)| *tenant == row.tenant_id && *held_name == name)
            {
                return Err(StoreError::PluginNameConflict);
            }
            held.push((row.tenant_id, name));
        }
        for row in self.upstreams.values() {
            if row.auth_plugin_uuid.is_some() && row.auth_plugin_ref.is_none() {
                return Err(StoreError::Invariant {
                    reason: String::from("upstream carries an auth plugin uuid and no reference"),
                });
            }
        }
        Ok(())
    }

    /// Every index entry agrees with the enabled route it names.
    fn verify_index(&self) -> Result<(), StoreError> {
        for (key, route_id) in &self.enabled_match_index {
            let Some(row) = self.routes.get(route_id) else {
                return Err(orphan_index());
            };
            let Some(http) = &row.route.match_config.http else {
                return Err(non_http_index());
            };
            let agrees = row.route.enabled.unwrap_or_default()
                && row.route.upstream_id == key.upstream_id
                && http.path == key.path
                && row.route.priority.unwrap_or_default() == key.priority
                && http.methods.contains(&key.method);
            if !agrees {
                return Err(disagreeing_index());
            }
        }
        Ok(())
    }

    fn assert_route(&self, route_id: Uuid) -> Result<(), StoreError> {
        if self.routes.contains_key(&route_id) {
            Ok(())
        } else {
            Err(StoreError::Invariant {
                reason: String::from("dependent row references a missing route"),
            })
        }
    }

    fn assert_upstream(&self, upstream_id: Uuid) -> Result<(), StoreError> {
        if self.upstreams.contains_key(&upstream_id) {
            Ok(())
        } else {
            Err(StoreError::Invariant {
                reason: String::from("dependent row references a missing upstream"),
            })
        }
    }
}

/// Builds the index key of one enabled route method.
fn match_key(route: &Route, path: &str, method: &str) -> MatchKey {
    MatchKey {
        upstream_id: route.upstream_id,
        path: path.to_owned(),
        priority: route.priority.unwrap_or_default(),
        method: method.to_owned(),
    }
}

fn missing_upstream() -> StoreError {
    StoreError::Invariant {
        reason: String::from("route references a missing upstream"),
    }
}

fn mismatched_tenant() -> StoreError {
    StoreError::Invariant {
        reason: String::from("route and upstream tenants differ"),
    }
}

fn orphan_index() -> StoreError {
    StoreError::Invariant {
        reason: String::from("match index references a missing route"),
    }
}

fn non_http_index() -> StoreError {
    StoreError::Invariant {
        reason: String::from("match index references a non-http route"),
    }
}

fn disagreeing_index() -> StoreError {
    StoreError::Invariant {
        reason: String::from("match index disagrees with the route it names"),
    }
}

/// The in-process transactional store.
///
/// Lives in an [`Arc`] and is `Send + Sync`; the whole table set sits behind
/// one [`RwLock`], so a batch and the reads around it are serialized.
pub struct OagwStore {
    tables: RwLock<Tables>,
}

impl Default for OagwStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OagwStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tables: RwLock::new(Tables::default()),
        }
    }

    /// Applies one batch against a cloned table set and swaps it in only when
    /// the whole batch succeeded.
    fn commit<R>(
        &self,
        batch: impl FnOnce(&mut Tables) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut guard = self.tables.write();
        let mut candidate = guard.clone();
        let produced = batch(&mut candidate)?;
        candidate.verify()?;
        *guard = candidate;
        Ok(produced)
    }

    /// Inserts one `oagw_upstream` row and its `oagw_upstream_tag` rows.
    ///
    /// The `(tenant_id, alias)` uniqueness check runs inside the same batch as
    /// the insert, after the row has been applied to the candidate set, so a
    /// violation leaves no row behind.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AliasConflict`] when another upstream of the
    /// calling tenant already holds the normalized alias, and
    /// [`StoreError::Invariant`] when the batch would breach the model.
    pub fn insert_upstream(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
    ) -> Result<UpstreamRow, StoreError> {
        self.insert_upstream_with_bindings(tenant_id, upstream, &BindingWrite::none(unix_now()))
    }

    /// Inserts one `oagw_upstream` row together with the plugin bindings the
    /// parent write carries.
    ///
    /// The binding rows and the two auth plugin identity columns are written
    /// by the same batch the parent row is, so a parent that fails leaves no
    /// binding behind and a binding that fails writes no parent.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`Self::insert_upstream`] does.
    pub fn insert_upstream_with_bindings(
        &self,
        tenant_id: Uuid,
        upstream: &Upstream,
        write: &BindingWrite,
    ) -> Result<UpstreamRow, StoreError> {
        let mut stored = upstream.clone();
        stored.tags.clear();
        let tags = upstream.tags.clone();
        let alias = upstream.alias.clone();
        let id = stored.id;
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-insert
        self.commit(|tables| {
            tables.upstreams.insert(
                id,
                UpstreamRow {
                    tenant_id,
                    tags: Vec::new(),
                    upstream: stored,
                    // The auth plugin identity columns are the plugin system's
                    // write, reached through the parent's own write path; an
                    // upstream written without that routine carries none.
                    auth_plugin_ref: write.auth.as_ref().map(|auth| auth.plugin_ref.clone()),
                    auth_plugin_uuid: write.auth.as_ref().and_then(|auth| auth.plugin_uuid),
                },
            );
            tables.sync_upstream_tags(id, &tags);
            tables.sync_upstream_plugins(id, &write.bindings);
            // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.recompute_plugin_eligibility(tenant_id, &write.referenced_uuids(), write.marked_at);
            // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            hold_alias(tables, tenant_id, id, &alias)?;
            read_upstream_row(tables, id)
        })
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-insert
    }

    /// Reads one `oagw_upstream` row by identifier and calling tenant.
    #[must_use]
    pub fn get_upstream(&self, tenant_id: Uuid, id: Uuid) -> Option<UpstreamRow> {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-if
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path
        let row = tables.upstreams.get(&id)?;
        let owned = row.tenant_id == tenant_id;
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-if
        owned.then(|| materialize_upstream(&tables, row))
    }

    /// Scans the `oagw_upstream` rows of the calling tenant.
    #[must_use]
    pub fn list_upstreams(&self, tenant_id: Uuid) -> Vec<UpstreamRow> {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-return
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list-if
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list
        tables
            .upstreams
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .map(|row| materialize_upstream(&tables, row))
            .collect()
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list-if
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-return
    }

    /// Reads the `oagw_upstream` row of the calling tenant whose normalized
    /// alias equals the one given.
    ///
    /// The tenant equality sits in the same predicate as the alias, so the
    /// scan never yields a foreign tenant's row (`cpt-cf-oagw-principle-tenant-scope`),
    /// and the comparison runs on the normalized form only — case-insensitive,
    /// trailing dot dropped, port participating in identity — through
    /// [`Alias::parse`], so a stored alias and a resolved alias cannot disagree
    /// about shape. This is the read the hierarchical walk issues once per
    /// chain element, keyed in the `upstream:{tenant_id}:{alias}` shape of
    /// ADR 0005.
    #[must_use]
    pub fn upstream_by_alias(&self, tenant_id: Uuid, alias: &Alias) -> Option<UpstreamRow> {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-lookup
        tables
            .upstreams
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .find(|row| {
                row.upstream
                    .alias
                    .as_deref()
                    .and_then(|held| Alias::parse(held).ok())
                    .is_some_and(|held| held == *alias)
            })
            .map(|row| materialize_upstream(&tables, row))
        // @cpt-end:cpt-cf-oagw-algo-tenant-chain-walk:p1:inst-walk-lookup
    }

    /// Reads the `oagw_route` rows of the calling tenant that belong to one
    /// upstream.
    ///
    /// The hierarchical resolution reads these to find the chain element whose
    /// route matches the request, so the read is scoped exactly as
    /// `upstream_by_alias` is: tenant first, then the owning upstream.
    #[must_use]
    pub fn routes_of_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Vec<RouteRow> {
        let tables = self.tables.read();
        tables
            .routes
            .values()
            .filter(|row| row.tenant_id == tenant_id && row.route.upstream_id == upstream_id)
            .map(|row| materialize_route(&tables, row))
            .collect()
    }

    /// Replaces one `oagw_upstream` row and rewrites its tag rows.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AliasConflict`] when another upstream of the
    /// calling tenant already holds the normalized alias, and
    /// [`StoreError::Invariant`] when the batch would breach the model.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream: &Upstream,
    ) -> Result<UpstreamRow, StoreError> {
        self.replace_upstream_with_bindings(tenant_id, id, upstream, &BindingWrite::none(unix_now()))
    }

    /// Replaces one `oagw_upstream` row together with the plugin bindings the
    /// parent write carries, in the same batch.
    ///
    /// The write set is the full replacement of the binding rows, so a body
    /// that omits the `plugins` sub-object clears them.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`Self::replace_upstream`] does.
    pub fn replace_upstream_with_bindings(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        upstream: &Upstream,
        write: &BindingWrite,
    ) -> Result<UpstreamRow, StoreError> {
        let mut stored = upstream.clone();
        stored.tags.clear();
        let tags = upstream.tags.clone();
        let alias = upstream.alias.clone();
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-write
        self.commit(|tables| {
            // The bindings the row held before the write are the ones whose
            // reference set the write may have emptied.
            let unlinked = tables.referenced_uuids(tenant_id, id);
            assert_stored_upstream(tables, tenant_id, id, &stored)?;
            tables.upstreams.insert(
                id,
                UpstreamRow {
                    tenant_id,
                    tags: Vec::new(),
                    upstream: stored,
                    // The plugin system's binding routine owns these two
                    // columns; a plain replacement that reaches the store
                    // directly carries none of it.
                    auth_plugin_ref: write.auth.as_ref().map(|auth| auth.plugin_ref.clone()),
                    auth_plugin_uuid: write.auth.as_ref().and_then(|auth| auth.plugin_uuid),
                },
            );
            tables.sync_upstream_tags(id, &tags);
            tables.sync_upstream_plugins(id, &write.bindings);
            let changed = tables.referenced_uuids(tenant_id, id);
            // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.recompute_plugin_eligibility(
                tenant_id,
                &unlinked.union(&changed).copied().collect(),
                write.marked_at,
            );
            // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            hold_alias(tables, tenant_id, id, &alias)?;
            read_upstream_row(tables, id)
        })
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-write
    }

    /// Deletes one `oagw_upstream` row, cascading into its routes and their
    /// dependents, and answers `false` when nothing matched the calling
    /// tenant's predicate.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invariant`] when the batch would breach the
    /// model.
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, StoreError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-delete-write
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-empty-if
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-empty
        let deleted = self.commit(|tables| {
            let Some(row) = tables.upstreams.get(&id) else {
                return Ok(false);
            };
            if row.tenant_id != tenant_id {
                return Ok(false);
            }
            let route_ids: Vec<Uuid> = tables
                .routes
                .values()
                .filter(|route| route.route.upstream_id == id)
                .map(|route| route.route.id)
                .collect();
            for route_id in route_ids {
                tables.routes.remove(&route_id);
                tables.drop_route_dependents(route_id);
                tables
                    .route_plugins
                    .retain(|(parent, _), _| *parent != route_id);
            }
            tables.upstream_tags.retain(|(parent, _)| *parent != id);
            tables
                .upstream_plugins
                .retain(|(parent, _), _| *parent != id);
            tables.upstreams.remove(&id);
            Ok(true)
        })?;
        Ok(deleted)
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-empty
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-path-empty-if
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-delete-write
    }

    /// Inserts one `oagw_route` row with its match, method, and tag rows.
    ///
    /// The enabled-match-key uniqueness check runs inside the same batch as
    /// the insert, after the row has been applied to the candidate set, so a
    /// collision leaves no row behind.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MatchConflict`] naming the colliding route when
    /// an enabled route of the same upstream already holds the match key, and
    /// [`StoreError::Invariant`] when the batch would breach the model.
    pub fn insert_route(&self, tenant_id: Uuid, route: &Route) -> Result<RouteRow, StoreError> {
        self.insert_route_with_bindings(tenant_id, route, &BindingWrite::none(unix_now()))
    }

    /// Inserts one `oagw_route` row together with the plugin bindings the
    /// parent write carries, in the same batch.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`Self::insert_route`] does.
    pub fn insert_route_with_bindings(
        &self,
        tenant_id: Uuid,
        route: &Route,
        write: &BindingWrite,
    ) -> Result<RouteRow, StoreError> {
        let mut stored = route.clone();
        stored.tags.clear();
        let tags = route.tags.clone();
        let route_id = stored.id;
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-insert
        self.commit(|tables| {
            tables.routes.insert(
                route_id,
                RouteRow {
                    tenant_id,
                    tags: Vec::new(),
                    route: stored,
                },
            );
            tables.sync_route_tags(route_id, &tags);
            tables.sync_route_plugins(route_id, &write.bindings);
            // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.recompute_plugin_eligibility(tenant_id, &write.referenced_uuids(), write.marked_at);
            // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.clear_route_matches(route_id);
            hold_match_key(tables, tenant_id, route, route_id)?;
            tables.record_route_matches(route);
            read_route_row(tables, route_id)
        })
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-insert
    }

    /// Reads one `oagw_route` row by identifier and calling tenant.
    #[must_use]
    pub fn get_route(&self, tenant_id: Uuid, id: Uuid) -> Option<RouteRow> {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-predicate
        let row = tables.routes.get(&id)?;
        let owned = row.tenant_id == tenant_id;
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-predicate
        owned.then(|| materialize_route(&tables, row))
    }

    /// Scans the `oagw_route` rows of the calling tenant, with the match,
    /// method, and tag rows of the whole scan read in the same pass.
    #[must_use]
    pub fn list_routes(&self, tenant_id: Uuid) -> Vec<RouteRow> {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list
        tables
            .routes
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .map(|row| materialize_route(&tables, row))
            .collect()
        // @cpt-end:cpt-cf-oagw-algo-tenant-scope:p1:inst-scope-list
    }

    /// Replaces one `oagw_route` row and rewrites its dependent rows.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MatchConflict`] naming the colliding route when
    /// another enabled route of the same upstream already holds the match key,
    /// and [`StoreError::Invariant`] when the batch would breach the model.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        route: &Route,
    ) -> Result<RouteRow, StoreError> {
        self.replace_route_with_bindings(tenant_id, id, route, &BindingWrite::none(unix_now()))
    }

    /// Replaces one `oagw_route` row together with the plugin bindings the
    /// parent write carries, in the same batch.
    ///
    /// # Errors
    ///
    /// Returns the same refusals [`Self::replace_route`] does.
    pub fn replace_route_with_bindings(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        route: &Route,
        write: &BindingWrite,
    ) -> Result<RouteRow, StoreError> {
        let mut stored = route.clone();
        stored.tags.clear();
        let tags = route.tags.clone();
        self.commit(|tables| {
            let unlinked = tables.referenced_uuids(tenant_id, id);
            assert_stored_route(tables, tenant_id, id, &stored)?;
            tables.routes.insert(
                id,
                RouteRow {
                    tenant_id,
                    tags: Vec::new(),
                    route: stored,
                },
            );
            tables.sync_route_tags(id, &tags);
            tables.sync_route_plugins(id, &write.bindings);
            let changed = tables.referenced_uuids(tenant_id, id);
            // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.recompute_plugin_eligibility(
                tenant_id,
                &unlinked.union(&changed).copied().collect(),
                write.marked_at,
            );
            // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-gc
            tables.clear_route_matches(id);
            hold_match_key(tables, tenant_id, route, id)?;
            tables.record_route_matches(route);
            read_route_row(tables, id)
        })
    }

    /// Deletes one `oagw_route` row and its dependent rows, leaving the
    /// upstream row untouched, and answers `false` when nothing matched the
    /// calling tenant's predicate.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invariant`] when the batch would breach the
    /// model.
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, StoreError> {
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-write
        self.commit(|tables| {
            let Some(row) = tables.routes.get(&id) else {
                return Ok(false);
            };
            if row.tenant_id != tenant_id {
                return Ok(false);
            }
            tables.routes.remove(&id);
            tables.drop_route_dependents(id);
            tables
                .route_plugins
                .retain(|(parent, _), _| *parent != id);
            Ok(true)
        })
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-write
    }

    /// Inserts one `oagw_plugin` row.
    ///
    /// The `(tenant_id, name)` uniqueness check runs inside the same batch as
    /// the insert, after the row has been applied to the candidate set, so a
    /// violation leaves no row behind. `gc_eligible_at` is left unset and
    /// `last_used_at` is left unset: a created plugin is linked to nothing and
    /// has been used by nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::PluginNameConflict`] when another plugin of the
    /// calling tenant already holds the name, and [`StoreError::Invariant`]
    /// when the batch would breach the model.
    pub fn insert_plugin(&self, tenant_id: Uuid, plugin: &Plugin) -> Result<PluginRow, StoreError> {
        let name = plugin.name.clone();
        let id = plugin.id;
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-insert
        self.commit(|tables| {
            tables.plugins.insert(
                id,
                PluginRow {
                    tenant_id,
                    plugin: plugin.clone(),
                },
            );
            hold_plugin_name(tables, tenant_id, id, &name)?;
            read_plugin_row(tables, id)
        })
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-insert
    }

    /// Reads one `oagw_plugin` row by identifier and calling tenant.
    ///
    /// A foreign-owned row and a nonexistent one answer `None` alike, and a
    /// named plugin — which has no row at all — answers `None` through the
    /// same predicate.
    #[must_use]
    pub fn get_plugin(&self, tenant_id: Uuid, id: Uuid) -> Option<PluginRow> {
        let tables = self.tables.read();
        owned_plugin(&tables, tenant_id, id).cloned()
    }

    /// Scans the `oagw_plugin` rows of the calling tenant, in identifier
    /// order.
    ///
    /// Named plugins are absent by construction: the scan reads the one table
    /// the custom rows live in.
    #[must_use]
    pub fn list_plugins(&self, tenant_id: Uuid) -> Vec<PluginRow> {
        let tables = self.tables.read();
        tables
            .plugins
            .values()
            .filter(|row| row.tenant_id == tenant_id)
            .cloned()
            .collect()
    }

    /// Deletes one `oagw_plugin` row and answers `false` when nothing matched
    /// the calling tenant's predicate.
    ///
    /// No binding row is removed by this deletion: neither binding table
    /// carries a foreign key to `oagw_plugin`, so a plugin deletion removes no
    /// reference to it (DESIGN §3.1). The in-use check that gates this write
    /// is the caller's.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Invariant`] when the batch would breach the
    /// model.
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, StoreError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-write
        self.commit(|tables| {
            let Some(row) = tables.plugins.get(&id) else {
                return Ok(false);
            };
            if row.tenant_id != tenant_id {
                return Ok(false);
            }
            tables.plugins.remove(&id);
            Ok(true)
        })
        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-terminal
        // `Deleted` is terminal: the row is gone, no binding row referenced
        // it, and no transition returns it.
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-terminal
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-write
    }

    /// Whether any reference of the calling tenant holds one plugin.
    ///
    /// The scan reads the two binding tables' `plugin_uuid` column and the
    /// upstream rows' scalar `auth_plugin_uuid` column; the scalar column is
    /// what keeps this check off JSON scanning (DESIGN §3.1). A named plugin
    /// has no row and is never addressed here.
    #[must_use]
    pub fn plugin_in_use(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let tables = self.tables.read();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-named-if
        // A named plugin has no row in `oagw_plugin` at all, so the routine is
        // not applicable to it: it is never stored, never garbage-collected,
        // and never deleteable, and the caller answers its absence with 404
        // before it reaches this scan.
        if owned_plugin(&tables, tenant_id, id).is_none() {
            // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-named-return
            return false;
            // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-named-return
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-named-if
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-scan
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-return
        plugin_is_referenced(&tables, id)
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-return
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-scan
    }

    /// Sets the garbage-collection eligibility of one plugin row to the
    /// instant `marked_at` plus the TTL of §1.5.
    ///
    /// The write the eligibility tests arrange their rows with: a live binding
    /// write marks a row only when it removes the row's last reference, and
    /// this method exists so a test can place a row on either side of the TTL
    /// without driving a whole binding history to get there.
    pub fn mark_plugin_eligible(&self, tenant_id: Uuid, id: Uuid, marked_at: u64) {
        self.commit(|tables| {
            if owned_plugin(tables, tenant_id, id).is_some()
                && let Some(row) = tables.plugins.get_mut(&id)
            {
                row.plugin.gc_eligible_at = Some(marked_at.saturating_add(PLUGIN_GC_TTL_SECS));
            }
            Ok(())
        })
        .expect("the eligibility marking is an in-memory write");
    }

    /// Records the last use of the named custom plugin rows at `now`.
    ///
    /// The write the data-plane proxy issues after the response is produced,
    /// coalesced per plugin: no decision reads `last_used_at`, so the write is
    /// the whole of the obligation, and a row that is no longer stored is left
    /// alone rather than resurrected.
    pub fn record_plugin_use(&self, used: &[Uuid], now: u64) {
        self.commit(|tables| {
            for id in used {
                if let Some(row) = tables.plugins.get_mut(id) {
                    row.plugin.last_used_at = Some(now);
                }
            }
            Ok(())
        })
        .expect("the last-use record is an in-memory write");
    }

    /// Runs one pass of the periodic garbage-collection job of §1.4 at `now`.
    ///
    /// The pass marks every custom row whose reference set is empty and which
    /// carries no marking — whether it lost its last reference to a binding
    /// write or never gained one at all — and then deletes only the rows whose
    /// `gc_eligible_at` is in the past and whose reference set is still empty
    /// at the moment it runs, so a plugin rebound between the marking and the
    /// deletion is never removed. Everything else is left alone, and no
    /// decision reads `last_used_at`.
    #[must_use]
    pub fn run_plugin_garbage_collection(&self, now: u64) -> PluginGcReport {
        let mut tables = self.tables.write();
        let mut report = PluginGcReport::default();

        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-unlink
        // The job's own reference scan is the same one a binding write runs,
        // applied to every row: a row it finds with no reference is marked,
        // which is the `Linked` to `Unlinked` transition.
        let scanned: BTreeSet<Uuid> = tables.plugins.keys().copied().collect();
        let marked_before: BTreeSet<Uuid> = scanned
            .iter()
            .copied()
            .filter(|id| {
                tables
                    .plugins
                    .get(id)
                    .is_some_and(|row| row.plugin.gc_eligible_at.is_some())
            })
            .collect();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-continue
        // The reference set each row carries is the input the eligibility
        // decision consumes: the recompute marks only the rows whose reference
        // set is empty, so a row the scan finds still referenced is left with
        // the eligibility it had.
        tables.recompute_plugin_eligibility(Uuid::nil(), &scanned, now);
        report.marked = scanned
            .into_iter()
            .filter(|id| {
                !marked_before.contains(id)
                    && tables
                        .plugins
                        .get(id)
                        .is_some_and(|row| row.plugin.gc_eligible_at.is_some())
            })
            .collect();
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-continue
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-unlink

        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-gc
        for (id, _) in tables
            .plugins
            .iter()
            .filter(|(_, row)| row.plugin.gc_eligible_at.is_some_and(|at| at <= now))
            .map(|(id, row)| (*id, row.clone()))
            .collect::<Vec<_>>()
        {
            if plugin_is_referenced(&tables, id) {
                continue;
            }
            tables.plugins.remove(&id);
            report.collected.push(id);
        }
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-gc
        report
    }

    /// Reads the `oagw_upstream_plugin` rows of one upstream, in position
    /// order.
    #[must_use]
    pub fn upstream_plugin_rows(&self, tenant_id: Uuid, parent_id: Uuid) -> Vec<PluginBinding> {
        let tables = self.tables.read();
        if owned_upstream(&tables, tenant_id, parent_id).is_none() {
            return Vec::new();
        }
        binding_rows(&tables.upstream_plugins, parent_id)
    }

    /// Reads the `oagw_route_plugin` rows of one route, in position order.
    #[must_use]
    pub fn route_plugin_rows(&self, tenant_id: Uuid, parent_id: Uuid) -> Vec<PluginBinding> {
        let tables = self.tables.read();
        if owned_route(&tables, tenant_id, parent_id).is_none() {
            return Vec::new();
        }
        binding_rows(&tables.route_plugins, parent_id)
    }

    /// Reads the `oagw_route_http_match` row of one route.
    #[must_use]
    pub fn route_http_match(&self, tenant_id: Uuid, route_id: Uuid) -> Option<HttpMatch> {
        let tables = self.tables.read();
        owned_route(&tables, tenant_id, route_id)?;
        tables.route_http_match.get(&route_id).cloned()
    }

    /// Reads the `oagw_route_grpc_match` row of one route.
    #[must_use]
    pub fn route_grpc_match(&self, tenant_id: Uuid, route_id: Uuid) -> Option<GrpcMatch> {
        let tables = self.tables.read();
        owned_route(&tables, tenant_id, route_id)?;
        tables.route_grpc_match.get(&route_id).cloned()
    }

    /// Reads the `oagw_route_method` rows of one route.
    #[must_use]
    pub fn route_methods(&self, tenant_id: Uuid, route_id: Uuid) -> Vec<String> {
        let tables = self.tables.read();
        if owned_route(&tables, tenant_id, route_id).is_none() {
            return Vec::new();
        }
        tables
            .route_methods
            .iter()
            .filter(|key| key.route_id == route_id)
            .map(|key| key.method.clone())
            .collect()
    }

    /// Reads the `oagw_upstream_tag` rows of one upstream.
    #[must_use]
    pub fn upstream_tag_rows(&self, tenant_id: Uuid, parent_id: Uuid) -> Vec<String> {
        let tables = self.tables.read();
        if owned_upstream(&tables, tenant_id, parent_id).is_none() {
            return Vec::new();
        }
        tag_rows(&tables.upstream_tags, parent_id)
    }

    /// Reads the `oagw_route_tag` rows of one route.
    #[must_use]
    pub fn route_tag_rows(&self, tenant_id: Uuid, parent_id: Uuid) -> Vec<String> {
        let tables = self.tables.read();
        if owned_route(&tables, tenant_id, parent_id).is_none() {
            return Vec::new();
        }
        tag_rows(&tables.route_tags, parent_id)
    }

    /// Reads the derived enabled-match index restricted to the calling
    /// tenant's routes.
    #[must_use]
    pub fn enabled_match_index(&self, tenant_id: Uuid) -> BTreeMap<MatchKey, Uuid> {
        let tables = self.tables.read();
        tables
            .enabled_match_index
            .iter()
            .filter(|(_, route_id)| {
                tables
                    .routes
                    .get(*route_id)
                    .is_some_and(|row| row.tenant_id == tenant_id)
            })
            .map(|(key, route_id)| (key.clone(), *route_id))
            .collect()
    }

    /// Builds a store whose derived index names a route no route row backs.
    ///
    /// Test-only: the state is unreachable through the write path and exists
    /// to exercise the invariant check that answers the storage failure.
    #[cfg(feature = "test-utils")]
    #[must_use]
    pub fn with_orphaned_match_index() -> Self {
        let store = Self::new();
        {
            let mut tables = store.tables.write();
            tables.enabled_match_index.insert(
                MatchKey {
                    upstream_id: Uuid::nil(),
                    path: String::from("/"),
                    priority: 0,
                    method: String::from("GET"),
                },
                Uuid::nil(),
            );
        }
        store
    }
}

/// Reads back the upstream row a batch just wrote.
fn read_upstream_row(tables: &Tables, id: Uuid) -> Result<UpstreamRow, StoreError> {
    let Some(row) = tables.upstreams.get(&id) else {
        return Err(unreadable_row());
    };
    Ok(materialize_upstream(tables, row))
}

/// Reads back the route row a batch just wrote.
fn read_route_row(tables: &Tables, id: Uuid) -> Result<RouteRow, StoreError> {
    let Some(row) = tables.routes.get(&id) else {
        return Err(unreadable_row());
    };
    Ok(materialize_route(tables, row))
}

/// The row a committed batch must have left readable.
fn unreadable_row() -> StoreError {
    StoreError::Invariant {
        reason: String::from("committed batch left no readable row"),
    }
}

/// Reads back the plugin row a batch just wrote.
fn read_plugin_row(tables: &Tables, id: Uuid) -> Result<PluginRow, StoreError> {
    let Some(row) = tables.plugins.get(&id) else {
        return Err(unreadable_row());
    };
    Ok(row.clone())
}

/// Rejects a plugin name another plugin of the same tenant already holds,
/// excluding the row the batch is writing.
fn hold_plugin_name(
    tables: &Tables,
    tenant_id: Uuid,
    written: Uuid,
    name: &str,
) -> Result<(), StoreError> {
    let taken = tables.plugins.values().any(|row| {
        row.tenant_id == tenant_id && row.plugin.id != written && row.plugin.name == name
    });
    if taken {
        return Err(StoreError::PluginNameConflict);
    }
    Ok(())
}

/// The row of one plugin when the calling tenant owns it.
fn owned_plugin(tables: &Tables, tenant_id: Uuid, id: Uuid) -> Option<&PluginRow> {
    let row = tables.plugins.get(&id)?;
    (row.tenant_id == tenant_id).then_some(row)
}

/// Whether any reference in the store carries one plugin.
///
/// The scan is the scalar-column scan DESIGN §3.6 names: the two binding
/// tables' `plugin_uuid` column and the upstream rows' `auth_plugin_uuid`
/// column, and never a JSON configuration column.
fn plugin_is_referenced(tables: &Tables, id: Uuid) -> bool {
    let bound = tables
        .upstream_plugins
        .values()
        .chain(tables.route_plugins.values())
        .any(|binding| binding.plugin_uuid == Some(id));
    let owned = tables
        .upstreams
        .values()
        .any(|row| row.auth_plugin_uuid == Some(id));
    bound || owned
}

/// Copies the binding rows of one parent into a list, in position order.
fn binding_rows(
    rows: &BTreeMap<(Uuid, u32), PluginBinding>,
    parent_id: Uuid,
) -> Vec<PluginBinding> {
    rows.iter()
        .filter(|((parent, _), _)| *parent == parent_id)
        .map(|(_, binding)| binding.clone())
        .collect()
}

/// Rejects an alias another upstream of the same tenant already holds,
/// excluding the row the batch is writing.
fn hold_alias(
    tables: &Tables,
    tenant_id: Uuid,
    written: Uuid,
    alias: &Option<String>,
) -> Result<(), StoreError> {
    let Some(alias) = alias.as_deref() else {
        return Ok(());
    };
    let taken = tables.upstreams.values().any(|row| {
        row.tenant_id == tenant_id
            && row.upstream.id != written
            && row
                .upstream
                .alias
                .as_deref()
                .is_some_and(|held| held.eq_ignore_ascii_case(alias))
    });
    if taken {
        return Err(StoreError::AliasConflict);
    }
    Ok(())
}

/// Rejects an enabled match key another enabled route of the same upstream
/// already holds, excluding the row the batch is writing.
fn hold_match_key(
    tables: &Tables,
    tenant_id: Uuid,
    route: &Route,
    written: Uuid,
) -> Result<(), StoreError> {
    let Some(http) = &route.match_config.http else {
        return Ok(());
    };
    if !route.enabled.unwrap_or_default() {
        return Ok(());
    }
    for method in &http.methods {
        let key = match_key(route, &http.path, method);
        if let Some(holder) = tables.enabled_match_index.get(&key) {
            let same_tenant = tables
                .routes
                .get(holder)
                .is_some_and(|row| row.tenant_id == tenant_id);
            if same_tenant && *holder != written {
                return Err(StoreError::MatchConflict {
                    colliding_route_id: *holder,
                });
            }
        }
    }
    Ok(())
}

/// The row of one upstream when the calling tenant owns it.
fn owned_upstream(tables: &Tables, tenant_id: Uuid, id: Uuid) -> Option<&UpstreamRow> {
    let row = tables.upstreams.get(&id)?;
    (row.tenant_id == tenant_id).then_some(row)
}

/// The row of one route when the calling tenant owns it.
fn owned_route(tables: &Tables, tenant_id: Uuid, id: Uuid) -> Option<&RouteRow> {
    let row = tables.routes.get(&id)?;
    (row.tenant_id == tenant_id).then_some(row)
}

/// Confirms the row a replacement batch writes is present, owned, and
/// addressed under the identifier it carries.
fn assert_stored_upstream(
    tables: &Tables,
    tenant_id: Uuid,
    id: Uuid,
    replacement: &Upstream,
) -> Result<(), StoreError> {
    if replacement.id != id {
        return Err(immutable_id());
    }
    if owned_upstream(tables, tenant_id, id).is_some() {
        Ok(())
    } else {
        Err(StoreError::Invariant {
            reason: String::from("replaced upstream row is missing or foreign"),
        })
    }
}

/// Confirms the row a replacement batch writes is present, owned, and
/// addressed under the identifier it carries.
fn assert_stored_route(
    tables: &Tables,
    tenant_id: Uuid,
    id: Uuid,
    replacement: &Route,
) -> Result<(), StoreError> {
    if replacement.id != id {
        return Err(immutable_id());
    }
    if owned_route(tables, tenant_id, id).is_some() {
        Ok(())
    } else {
        Err(StoreError::Invariant {
            reason: String::from("replaced route row is missing or foreign"),
        })
    }
}

/// The identifier a row is addressed by is immutable.
fn immutable_id() -> StoreError {
    StoreError::Invariant {
        reason: String::from("the identifier a row is addressed by is immutable"),
    }
}

/// Copies the tag rows of one parent into a list, in sorted order.
fn tag_rows(rows: &BTreeSet<(Uuid, String)>, parent_id: Uuid) -> Vec<String> {
    rows.iter()
        .filter(|(parent, _)| *parent == parent_id)
        .map(|(_, tag)| tag.clone())
        .collect()
}

/// Materializes the tag projection of an upstream row.
fn materialize_upstream(tables: &Tables, row: &UpstreamRow) -> UpstreamRow {
    let tags = tag_rows(&tables.upstream_tags, row.upstream.id);
    let mut upstream = row.upstream.clone();
    upstream.tags = tags.clone();
    UpstreamRow {
        tenant_id: row.tenant_id,
        tags,
        upstream,
        auth_plugin_ref: row.auth_plugin_ref.clone(),
        auth_plugin_uuid: row.auth_plugin_uuid,
    }
}

/// Materializes the tag projection of a route row.
fn materialize_route(tables: &Tables, row: &RouteRow) -> RouteRow {
    let tags = tag_rows(&tables.route_tags, row.route.id);
    let mut route = row.route.clone();
    route.tags = tags.clone();
    RouteRow {
        tenant_id: row.tenant_id,
        tags,
        route,
    }
}
