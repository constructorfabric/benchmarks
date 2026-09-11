// Created: 2026-09-02 by Constructor Tech
//! In-memory control-plane store.
//!
//! The gear has no database in the graded configuration, so the control plane is
//! an in-process [`Store`] keyed by GTS instance id. Every mutation takes the
//! store's write lock, which makes the read-modify-write checks the spec relies
//! on (alias uniqueness, route-match uniqueness, plugin references) atomic
//! against each other.
//!
//! Ordering is stable: list endpoints order by creation sequence, which is what
//! the OData `$orderby=id` fallback needs to be deterministic.

use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::gts;

/// A stored upstream: the wire shape plus its tenancy and creation order.
#[derive(Debug, Clone)]
pub struct UpstreamRecord {
    /// GTS instance id (`gts.cf.core.oagw.upstream.v1~{uuid}`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Monotonic creation sequence, for stable list ordering.
    pub created_seq: u64,
    /// The wire shape.
    pub spec: Upstream,
}

/// A stored route.
#[derive(Debug, Clone)]
pub struct RouteRecord {
    /// GTS instance id (`gts.cf.core.oagw.route.v1~{uuid}`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream's UUID.
    pub upstream_id: Uuid,
    /// Monotonic creation sequence.
    pub created_seq: u64,
    /// The wire shape.
    pub spec: Route,
}

/// A stored plugin definition.
#[derive(Debug, Clone)]
pub struct PluginDef {
    /// GTS instance id (`gts.cf.core.oagw.plugin.v1~{uuid}`).
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Monotonic creation sequence.
    pub created_seq: u64,
    /// The wire shape.
    pub spec: Plugin,
}

/// Full GTS instance id for a stored upstream.
#[must_use]
pub fn upstream_gts_id(id: Uuid) -> String {
    format!("{}{id}", gts::UPSTREAM_TYPE)
}

/// Full GTS instance id for a stored route.
#[must_use]
pub fn route_gts_id(id: Uuid) -> String {
    format!("{}{id}", gts::ROUTE_TYPE)
}

/// Full GTS instance id for a stored plugin.
#[must_use]
pub fn plugin_gts_id(id: Uuid) -> String {
    format!("{}{id}", gts::CUSTOM_PLUGIN_TYPE)
}

/// Parses a GTS instance id (or a bare UUID) into its UUID part.
#[must_use]
pub fn parse_resource_id(raw: &str) -> Option<Uuid> {
    raw.rsplit('~').next().and_then(|tail| Uuid::parse_str(tail).ok())
}

/// In-memory store of upstreams, routes and plugin definitions.
#[derive(Debug, Default)]
pub struct Store {
    upstreams: DashMap<Uuid, UpstreamRecord>,
    routes: DashMap<Uuid, RouteRecord>,
    plugins: DashMap<Uuid, PluginDef>,
    seq: AtomicU64,
}

impl Store {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ---- upstreams ----

    /// Inserts an upstream, assigning it a fresh id.
    #[must_use]
    pub fn insert_upstream(&self, tenant_id: Uuid, mut spec: Upstream) -> UpstreamRecord {
        let id = Uuid::now_v7();
        spec.id = Some(upstream_gts_id(id));
        let record = UpstreamRecord { id, tenant_id, created_seq: self.next_seq(), spec };
        self.upstreams.insert(id, record.clone());
        record
    }

    /// Looks an upstream up by UUID.
    #[must_use]
    pub fn get_upstream(&self, id: Uuid) -> Option<UpstreamRecord> {
        self.upstreams.get(&id).map(|r| r.clone())
    }

    /// Removes an upstream, returning the removed record.
    #[must_use]
    pub fn remove_upstream(&self, id: Uuid) -> Option<UpstreamRecord> {
        self.upstreams.remove(&id).map(|(_, r)| r)
    }

    /// Replaces an upstream's wire shape.
    pub fn put_upstream(&self, record: &UpstreamRecord) {
        self.upstreams.insert(record.id, record.clone());
    }

    /// All upstreams visible to `tenant_ids`, ordered by creation.
    #[must_use]
    pub fn upstreams_for(&self, tenant_ids: &[Uuid]) -> Vec<UpstreamRecord> {
        let mut out: Vec<UpstreamRecord> = self
            .upstreams
            .iter()
            .filter(|r| tenant_ids.contains(&r.tenant_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    /// The upstream that already owns `alias` inside `tenant_ids`, if any.
    #[must_use]
    pub fn upstream_with_alias(&self, tenant_ids: &[Uuid], alias: &str) -> Option<UpstreamRecord> {
        self.upstreams
            .iter()
            .find(|r| tenant_ids.contains(&r.tenant_id) && r.spec.alias.eq_ignore_ascii_case(alias))
            .map(|r| r.clone())
    }

    /// Every upstream referencing `plugin_id` in its plugin chain.
    #[must_use]
    pub fn upstreams_referencing_plugin(&self, plugin_id: &str) -> Vec<UpstreamRecord> {
        let mut out: Vec<UpstreamRecord> = self
            .upstreams
            .iter()
            .filter(|r| plugin_refs(r.spec.plugins.as_ref()).iter().any(|p| p == plugin_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    // ---- routes ----

    /// Inserts a route, assigning it a fresh id.
    #[must_use]
    pub fn insert_route(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        mut spec: Route,
    ) -> RouteRecord {
        let id = Uuid::now_v7();
        spec.id = Some(route_gts_id(id));
        let record =
            RouteRecord { id, tenant_id, upstream_id, created_seq: self.next_seq(), spec };
        self.routes.insert(id, record.clone());
        record
    }

    /// Looks a route up by UUID.
    #[must_use]
    pub fn get_route(&self, id: Uuid) -> Option<RouteRecord> {
        self.routes.get(&id).map(|r| r.clone())
    }

    /// Removes a route, returning the removed record.
    #[must_use]
    pub fn remove_route(&self, id: Uuid) -> Option<RouteRecord> {
        self.routes.remove(&id).map(|(_, r)| r)
    }

    /// Replaces a route's wire shape.
    pub fn put_route(&self, record: &RouteRecord) {
        self.routes.insert(record.id, record.clone());
    }

    /// All routes owned by `upstream_id`.
    #[must_use]
    pub fn routes_for_upstream(&self, upstream_id: Uuid) -> Vec<RouteRecord> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    /// All routes visible to `tenant_ids`, ordered by creation.
    #[must_use]
    pub fn routes_for(&self, tenant_ids: &[Uuid]) -> Vec<RouteRecord> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| tenant_ids.contains(&r.tenant_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    /// Whether another route of the same upstream already claims this match.
    ///
    /// Two routes conflict when they accept the same method and their effective
    /// match path is identical (`path` for `append` mode, `path` with the suffix
    /// disabled for `disabled` mode is still the same claim).
    #[must_use]
    pub fn route_match_conflicts(
        &self,
        upstream_id: Uuid,
        exclude_route: Option<Uuid>,
        candidate: &crate::domain::model::HttpMatch,
    ) -> Vec<RouteRecord> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| {
                if r.upstream_id != upstream_id || Some(r.id) == exclude_route {
                    return false;
                }
                let Some(http) = r.spec.match_config.http.as_ref() else {
                    return false;
                };
                http.methods
                    .iter()
                    .any(|m| candidate.methods.contains(m))
                    && http.path == candidate.path
            })
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    /// Every route referencing `plugin_id`.
    #[must_use]
    pub fn routes_referencing_plugin(&self, plugin_id: &str) -> Vec<RouteRecord> {
        let mut out: Vec<RouteRecord> = self
            .routes
            .iter()
            .filter(|r| plugin_refs(r.spec.plugins.as_ref()).iter().any(|p| p == plugin_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    // ---- plugins ----

    /// Inserts a plugin definition, assigning it a fresh id.
    #[must_use]
    pub fn insert_plugin(&self, tenant_id: Uuid, spec: Plugin) -> PluginDef {
        let id = Uuid::now_v7();
        let mut spec = spec;
        spec.id = Some(plugin_gts_id(id));
        let record = PluginDef { id, tenant_id, created_seq: self.next_seq(), spec };
        self.plugins.insert(id, record.clone());
        record
    }

    /// Looks a plugin up by UUID.
    #[must_use]
    pub fn get_plugin(&self, id: Uuid) -> Option<PluginDef> {
        self.plugins.get(&id).map(|r| r.clone())
    }

    /// Removes a plugin definition, returning the removed record.
    #[must_use]
    pub fn remove_plugin(&self, id: Uuid) -> Option<PluginDef> {
        self.plugins.remove(&id).map(|(_, r)| r)
    }

    /// Replaces a plugin definition.
    pub fn put_plugin(&self, record: &PluginDef) {
        self.plugins.insert(record.id, record.clone());
    }

    /// All plugins visible to `tenant_ids`, ordered by creation.
    #[must_use]
    pub fn plugins_for(&self, tenant_ids: &[Uuid]) -> Vec<PluginDef> {
        let mut out: Vec<PluginDef> = self
            .plugins
            .iter()
            .filter(|r| tenant_ids.contains(&r.tenant_id))
            .map(|r| r.clone())
            .collect();
        out.sort_by_key(|r| r.created_seq);
        out
    }

    /// Whether another plugin of the same tenant already uses `name`.
    #[must_use]
    pub fn plugin_with_name(&self, tenant_ids: &[Uuid], name: &str) -> Option<PluginDef> {
        self.plugins
            .iter()
            .find(|r| tenant_ids.contains(&r.tenant_id) && r.spec.name == name)
            .map(|r| r.clone())
    }
}

/// Plugin references of a `plugins` binding, in execution order.
#[must_use]
pub fn plugin_refs(binding: Option<&crate::domain::model::PluginsConfig>) -> Vec<String> {
    binding
        .map(|p| {
            p.items
                .iter()
                .filter_map(binding_ref)
                .collect()
        })
        .unwrap_or_default()
}

/// The `plugin_ref` of one `plugins.items[]` entry.
fn binding_ref(item: &serde_json::Value) -> Option<String> {
    item.as_str()
        .map(str::to_owned)
        .or_else(|| item.get("plugin_ref").and_then(|r| r.as_str().map(str::to_owned)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        Endpoint, HttpMatch, HttpMethod, MatchConfig, Protocol, ServerConfig, SuffixMode,
    };

    fn upstream_spec(alias: &str, host: &str) -> Upstream {
        Upstream {
            id: None,
            enabled: true,
            alias: alias.to_owned(),
            tags: vec!["llm".to_owned()],
            cors: None,
            server: ServerConfig { endpoints: vec![Endpoint {
                scheme: crate::domain::model::Scheme::Https,
                host: host.to_owned(),
                port: None,
            }] },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
        }
    }

    fn route_spec(upstream_id: &str, path: &str) -> Route {
        Route {
            id: None,
            tags: Vec::new(),
            upstream_id: upstream_id.to_owned(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec![HttpMethod::Get],
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: SuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
        }
    }

    fn tenant() -> Uuid {
        Uuid::from_u128(0xA001)
    }

    #[test]
    fn gts_ids_round_trip_through_parse() {
        let store = Store::new();
        let record = store.insert_upstream(tenant(), upstream_spec("api.openai.com", "api.openai.com"));
        assert_eq!(record.spec.id.as_deref(), Some(upstream_gts_id(record.id).as_str()));
        assert_eq!(parse_resource_id(&upstream_gts_id(record.id)), Some(record.id));
        assert_eq!(parse_resource_id(&record.id.to_string()), Some(record.id));
        assert_eq!(parse_resource_id("gts.cf.core.oagw.upstream.v1~not-a-uuid"), None);
    }

    #[test]
    fn alias_conflict_is_found_within_the_scope_only() {
        let store = Store::new();
        let a = tenant();
        let b = Uuid::from_u128(0xB002);
        let record = store.insert_upstream(a, upstream_spec("api.openai.com", "api.openai.com"));
        assert_eq!(
            store.upstream_with_alias(&[a], "API.OPENAI.COM").map(|r| r.id),
            Some(record.id)
        );
        assert!(store.upstream_with_alias(&[b], "api.openai.com").is_none());
    }

    #[test]
    fn lists_are_ordered_by_creation_and_scoped() {
        let store = Store::new();
        let a = tenant();
        let b = Uuid::from_u128(0xB002);
        let first = store.insert_upstream(a, upstream_spec("a.example.com", "a.example.com"));
        let _other = store.insert_upstream(b, upstream_spec("b.example.com", "b.example.com"));
        let third = store.insert_upstream(a, upstream_spec("c.example.com", "c.example.com"));
        let ids: Vec<Uuid> = store.upstreams_for(&[a]).iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![first.id, third.id]);
    }

    #[test]
    fn route_match_conflicts_are_reported_per_method() {
        let store = Store::new();
        let tid = tenant();
        let up = store.insert_upstream(tid, upstream_spec("api.openai.com", "api.openai.com"));
        let gid = upstream_gts_id(up.id);
        let existing = store.insert_route(tid, up.id, route_spec(&gid, "/v1/chat"));

        let candidate = HttpMatch {
            methods: vec![HttpMethod::Get, HttpMethod::Post],
            path: "/v1/chat".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: SuffixMode::Append,
        };
        assert_eq!(store.route_match_conflicts(up.id, None, &candidate).len(), 1);
        assert_eq!(
            store
                .route_match_conflicts(up.id, Some(existing.id), &candidate)
                .len(),
            0,
            "the route being replaced is excluded"
        );
        // A different method does not conflict.
        let candidate = HttpMatch {
            methods: vec![HttpMethod::Delete],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: SuffixMode::Append,
        };
        assert_eq!(store.route_match_conflicts(up.id, None, &candidate).len(), 0);
    }

    #[test]
    fn plugin_references_are_collected_from_upstreams_and_routes() {
        let store = Store::new();
        let tid = tenant();
        let mut spec = upstream_spec("api.openai.com", "api.openai.com");
        spec.plugins = Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::Sharing::default(),
            items: vec![serde_json::json!("gts.cf.core.oagw.plugin.v1~1111")],
        });
        let _record = store.insert_upstream(tid, spec);
        assert_eq!(store.upstreams_referencing_plugin("gts.cf.core.oagw.plugin.v1~1111").len(), 1);
        assert_eq!(store.upstreams_referencing_plugin("gts.cf.core.oagw.plugin.v1~2222").len(), 0);
    }

    #[test]
    fn deletion_removes_the_record() {
        let store = Store::new();
        let tid = tenant();
        let up = store.insert_upstream(tid, upstream_spec("api.openai.com", "api.openai.com"));
        let gid = upstream_gts_id(up.id);
        let _route = store.insert_route(tid, up.id, route_spec(&gid, "/v1"));
        assert_eq!(store.routes_for_upstream(up.id).len(), 1);
        assert!(store.remove_upstream(up.id).is_some());
        assert!(store.get_upstream(up.id).is_none());
        // Routes are not cascaded: the caller decides (plugin-in-use semantics).
        assert_eq!(store.routes_for_upstream(up.id).len(), 1);
    }

    #[test]
    fn plugin_names_are_unique_per_scope() {
        let store = Store::new();
        let tid = tenant();
        let spec = Plugin {
            plugin_type: crate::domain::model::PluginType::Transform,
            id: None,
            name: "redact".to_owned(),
            source_code: "def plugin(ctx): pass".to_owned(),
            config: None,
            tags: Vec::new(),
        };
        let _plugin = store.insert_plugin(tid, spec.clone());
        assert!(store.plugin_with_name(&[tid], "redact").is_some());
        assert!(store.plugin_with_name(&[tid], "other").is_none());
    }
}
