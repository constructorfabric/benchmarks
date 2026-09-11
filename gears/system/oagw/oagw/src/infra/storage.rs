//! In-memory, tenant-scoped repository implementations.
//!
//! The store is the source of truth for the gear's configuration objects.
//! Every read and write takes the tenant id as a parameter, so a caller
//! cannot address another tenant's rows even by guessing an id — the
//! data-layer half of `cpt-cf-oagw-nfr-multi-tenancy`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::OagwError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::timeutil;

/// Seconds in a day, for GC TTL arithmetic.
const SECS_PER_DAY: u64 = 86_400;

/// Shared in-memory store backing all three repositories.
#[derive(Default)]
pub struct MemoryStore {
    upstreams: DashMap<Uuid, Upstream>,
    routes: DashMap<Uuid, Route>,
    plugins: DashMap<Uuid, Plugin>,
    /// Serializes the read-then-write sections that enforce uniqueness.
    write_lock: RwLock<()>,
}

impl MemoryStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap in an [`Arc`] for sharing across the three repository facades.
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

/// Upstream repository over [`MemoryStore`].
pub struct MemoryUpstreamRepo(Arc<MemoryStore>);

/// Route repository over [`MemoryStore`].
pub struct MemoryRouteRepo(Arc<MemoryStore>);

/// Plugin repository over [`MemoryStore`].
pub struct MemoryPluginRepo(Arc<MemoryStore>);

impl MemoryUpstreamRepo {
    /// Bind a repository facade to `store`.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

impl MemoryRouteRepo {
    /// Bind a repository facade to `store`.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

impl MemoryPluginRepo {
    /// Bind a repository facade to `store`.
    #[must_use]
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self(store)
    }
}

fn sort_by_created<T, F>(items: &mut [T], key: F)
where
    F: Fn(&T) -> (&str, Uuid),
{
    items.sort_by(|a, b| {
        let (a_ts, a_id) = key(a);
        let (b_ts, b_id) = key(b);
        a_ts.cmp(b_ts).then_with(|| a_id.cmp(&b_id))
    });
}

#[async_trait]
impl UpstreamRepository for MemoryUpstreamRepo {
    async fn insert(&self, upstream: Upstream) -> Result<Upstream, OagwError> {
        let _guard = self.0.write_lock.write();
        let clash = self.0.upstreams.iter().any(|entry| {
            entry.tenant_id == upstream.tenant_id && entry.spec.alias == upstream.spec.alias
        });
        if clash {
            return Err(OagwError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.spec.alias
            ))
            .with("alias", upstream.spec.alias.clone()));
        }
        self.0.upstreams.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn replace(&self, upstream: Upstream) -> Result<Upstream, OagwError> {
        let _guard = self.0.write_lock.write();
        let exists = self
            .0
            .upstreams
            .get(&upstream.id)
            .is_some_and(|e| e.tenant_id == upstream.tenant_id);
        if !exists {
            return Err(OagwError::not_found("upstream does not exist"));
        }
        let clash = self.0.upstreams.iter().any(|entry| {
            entry.id != upstream.id
                && entry.tenant_id == upstream.tenant_id
                && entry.spec.alias == upstream.spec.alias
        });
        if clash {
            return Err(OagwError::conflict(format!(
                "an upstream with alias '{}' already exists for this tenant",
                upstream.spec.alias
            )));
        }
        self.0.upstreams.insert(upstream.id, upstream.clone());
        Ok(upstream)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Upstream> {
        self.0
            .upstreams
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    async fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Option<Upstream> {
        let mut found: Vec<Upstream> = self
            .0
            .upstreams
            .iter()
            .filter(|e| e.tenant_id == tenant_id && e.spec.alias == alias)
            .map(|e| e.clone())
            .collect();
        sort_by_created(&mut found, |u| (u.created_at.as_str(), u.id));
        found.into_iter().next()
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Upstream> {
        let mut items: Vec<Upstream> = self
            .0
            .upstreams
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        sort_by_created(&mut items, |u| (u.created_at.as_str(), u.id));
        items
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let _guard = self.0.write_lock.write();
        let owned = self
            .0
            .upstreams
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if !owned {
            return false;
        }
        self.0.upstreams.remove(&id).is_some()
    }

    async fn all(&self) -> Vec<Upstream> {
        self.0.upstreams.iter().map(|e| e.clone()).collect()
    }
}

impl MemoryRouteRepo {
    /// Detect a match-rule collision under the same upstream.
    ///
    /// Route determinism (`cpt-cf-oagw-db-schema`): no two *enabled* routes
    /// under one upstream may share `(path_prefix, priority)` for the same
    /// method.
    fn collides(&self, route: &Route) -> bool {
        if !route.spec.enabled {
            return false;
        }
        let key = route.match_key();
        self.0.routes.iter().any(|entry| {
            entry.id != route.id
                && entry.upstream_id == route.upstream_id
                && entry.spec.enabled
                && entry.match_key() == key
        })
    }
}

#[async_trait]
impl RouteRepository for MemoryRouteRepo {
    async fn insert(&self, route: Route) -> Result<Route, OagwError> {
        let _guard = self.0.write_lock.write();
        if self.collides(&route) {
            return Err(OagwError::conflict(
                "another enabled route under this upstream already matches the same method, \
                 path and priority",
            ));
        }
        self.0.routes.insert(route.id, route.clone());
        Ok(route)
    }

    async fn replace(&self, route: Route) -> Result<Route, OagwError> {
        let _guard = self.0.write_lock.write();
        let exists = self
            .0
            .routes
            .get(&route.id)
            .is_some_and(|e| e.tenant_id == route.tenant_id);
        if !exists {
            return Err(OagwError::not_found("route does not exist"));
        }
        if self.collides(&route) {
            return Err(OagwError::conflict(
                "another enabled route under this upstream already matches the same method, \
                 path and priority",
            ));
        }
        self.0.routes.insert(route.id, route.clone());
        Ok(route)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Route> {
        self.0
            .routes
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Route> {
        let mut items: Vec<Route> = self
            .0
            .routes
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        sort_by_created(&mut items, |r| (r.created_at.as_str(), r.id));
        items
    }

    async fn list_by_upstream(&self, upstream_id: Uuid) -> Vec<Route> {
        let mut items: Vec<Route> = self
            .0
            .routes
            .iter()
            .filter(|e| e.upstream_id == upstream_id)
            .map(|e| e.clone())
            .collect();
        sort_by_created(&mut items, |r| (r.created_at.as_str(), r.id));
        items
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let _guard = self.0.write_lock.write();
        let owned = self
            .0
            .routes
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if !owned {
            return false;
        }
        self.0.routes.remove(&id).is_some()
    }

    async fn delete_by_upstream(&self, upstream_id: Uuid) -> usize {
        let _guard = self.0.write_lock.write();
        let victims: Vec<Uuid> = self
            .0
            .routes
            .iter()
            .filter(|e| e.upstream_id == upstream_id)
            .map(|e| e.id)
            .collect();
        let count = victims.len();
        for id in victims {
            self.0.routes.remove(&id);
        }
        count
    }

    async fn all(&self) -> Vec<Route> {
        self.0.routes.iter().map(|e| e.clone()).collect()
    }
}

#[async_trait]
impl PluginRepository for MemoryPluginRepo {
    async fn insert(&self, plugin: Plugin) -> Result<Plugin, OagwError> {
        let _guard = self.0.write_lock.write();
        let clash = self
            .0
            .plugins
            .iter()
            .any(|e| e.tenant_id == plugin.tenant_id && e.name == plugin.name);
        if clash {
            return Err(OagwError::conflict(format!(
                "a plugin named '{}' already exists for this tenant",
                plugin.name
            )));
        }
        self.0.plugins.insert(plugin.id, plugin.clone());
        Ok(plugin)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Option<Plugin> {
        self.0
            .plugins
            .get(&id)
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
    }

    async fn list(&self, tenant_id: Uuid) -> Vec<Plugin> {
        let mut items: Vec<Plugin> = self
            .0
            .plugins
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.clone())
            .collect();
        sort_by_created(&mut items, |p| (p.created_at.as_str(), p.id));
        items
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> bool {
        let _guard = self.0.write_lock.write();
        let owned = self
            .0
            .plugins
            .get(&id)
            .is_some_and(|e| e.tenant_id == tenant_id);
        if !owned {
            return false;
        }
        self.0.plugins.remove(&id).is_some()
    }

    async fn touch(&self, id: Uuid) {
        if let Some(mut entry) = self.0.plugins.get_mut(&id) {
            entry.last_used_at = Some(timeutil::now_rfc3339());
        }
    }

    async fn collect_garbage(&self, linked: &[Uuid], ttl_days: u64) -> usize {
        let _guard = self.0.write_lock.write();
        let now = timeutil::now_rfc3339();
        let mut deleted = 0usize;
        let mut victims = Vec::new();
        for mut entry in self.0.plugins.iter_mut() {
            if linked.contains(&entry.id) {
                // Re-linked: it is no longer a GC candidate.
                entry.gc_eligible_at = None;
                continue;
            }
            match entry.gc_eligible_at.clone() {
                None => {
                    entry.gc_eligible_at = Some(timeutil::rfc3339_in(Duration::from_secs(
                        ttl_days * SECS_PER_DAY,
                    )));
                }
                Some(at) if at <= now => victims.push(entry.id),
                Some(_) => {}
            }
        }
        for id in victims {
            if self.0.plugins.remove(&id).is_some() {
                deleted += 1;
            }
        }
        deleted
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryPluginRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo};
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchConfig, PathSuffixMode, Plugin, PluginKind, Route, RouteSpec,
        ServerConfig, Upstream, UpstreamSpec,
    };
    use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
    use crate::domain::timeutil;
    use std::sync::Arc;
    use uuid::Uuid;

    fn upstream(tenant: Uuid, alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            created_at: timeutil::now_rfc3339(),
            updated_at: timeutil::now_rfc3339(),
            spec: UpstreamSpec {
                enabled: true,
                alias: alias.to_owned(),
                tags: Vec::new(),
                server: ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: "https".to_owned(),
                        host: alias.to_owned(),
                        port: 443,
                    }],
                },
                protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
                auth: None,
                headers: None,
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        }
    }

    fn route(tenant: Uuid, upstream_id: Uuid, path: &str, priority: i32) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            created_at: timeutil::now_rfc3339(),
            updated_at: timeutil::now_rfc3339(),
            spec: RouteSpec {
                enabled: true,
                priority,
                tags: Vec::new(),
                match_config: MatchConfig {
                    http: Some(HttpMatch {
                        methods: vec!["GET".to_owned()],
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        }
    }

    #[tokio::test]
    async fn alias_is_unique_per_tenant_not_globally() {
        let store = MemoryStore::shared();
        let repo = MemoryUpstreamRepo::new(Arc::clone(&store));
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();

        repo.insert(upstream(a, "api.openai.com"))
            .await
            .expect("first");
        let err = repo
            .insert(upstream(a, "api.openai.com"))
            .await
            .expect_err("duplicate in the same tenant");
        assert_eq!(err.status, 409);
        // A different tenant may shadow the same alias.
        repo.insert(upstream(b, "api.openai.com"))
            .await
            .expect("other tenant");
    }

    #[tokio::test]
    async fn reads_are_tenant_scoped() {
        let store = MemoryStore::shared();
        let repo = MemoryUpstreamRepo::new(Arc::clone(&store));
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        let created = repo
            .insert(upstream(owner, "api.openai.com"))
            .await
            .expect("insert");

        assert!(repo.get(owner, created.id).await.is_some());
        assert!(
            repo.get(stranger, created.id).await.is_none(),
            "another tenant must not see the row even with the right id"
        );
        assert!(!repo.delete(stranger, created.id).await);
        assert!(repo.get(owner, created.id).await.is_some());
    }

    #[tokio::test]
    async fn route_match_collision_is_rejected() {
        let store = MemoryStore::shared();
        let routes = MemoryRouteRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();

        routes
            .insert(route(tenant, upstream_id, "/v1/chat", 0))
            .await
            .expect("first");
        let err = routes
            .insert(route(tenant, upstream_id, "/v1/chat", 0))
            .await
            .expect_err("same path+priority+method");
        assert_eq!(err.status, 409);

        // A different priority is a different match key.
        routes
            .insert(route(tenant, upstream_id, "/v1/chat", 5))
            .await
            .expect("distinct priority");
        // A disabled route never collides.
        let mut disabled = route(tenant, upstream_id, "/v1/chat", 0);
        disabled.spec.enabled = false;
        routes.insert(disabled).await.expect("disabled route");
    }

    #[tokio::test]
    async fn deleting_an_upstream_cascades_to_routes() {
        let store = MemoryStore::shared();
        let routes = MemoryRouteRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        routes
            .insert(route(tenant, upstream_id, "/a", 0))
            .await
            .expect("insert");
        routes
            .insert(route(tenant, upstream_id, "/b", 0))
            .await
            .expect("insert");
        assert_eq!(routes.delete_by_upstream(upstream_id).await, 2);
        assert!(routes.list(tenant).await.is_empty());
    }

    fn plugin(tenant: Uuid, name: &str) -> Plugin {
        Plugin {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            kind: PluginKind::Guard,
            name: name.to_owned(),
            description: None,
            phases: Vec::new(),
            config_schema: None,
            source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
            created_at: timeutil::now_rfc3339(),
            last_used_at: None,
            gc_eligible_at: None,
        }
    }

    #[tokio::test]
    async fn plugin_names_are_unique_per_tenant() {
        let store = MemoryStore::shared();
        let repo = MemoryPluginRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        repo.insert(plugin(tenant, "validator"))
            .await
            .expect("first");
        let err = repo
            .insert(plugin(tenant, "validator"))
            .await
            .expect_err("duplicate");
        assert_eq!(err.status, 409);
    }

    #[tokio::test]
    async fn gc_marks_then_deletes() {
        let store = MemoryStore::shared();
        let repo = MemoryPluginRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let created = repo.insert(plugin(tenant, "orphan")).await.expect("insert");

        // The first sweep only marks: a plugin that is re-bound before the TTL
        // elapses must survive.
        assert_eq!(repo.collect_garbage(&[], 0).await, 0);
        let marked = repo.get(tenant, created.id).await.expect("still there");
        assert!(marked.gc_eligible_at.is_some());

        // The second sweep is past the (zero-day) due date, so it deletes.
        assert_eq!(repo.collect_garbage(&[], 0).await, 1);
        assert!(repo.get(tenant, created.id).await.is_none());
    }

    #[tokio::test]
    async fn gc_clears_the_mark_when_a_plugin_is_relinked() {
        let store = MemoryStore::shared();
        let repo = MemoryPluginRepo::new(Arc::clone(&store));
        let tenant = Uuid::new_v4();
        let created = repo
            .insert(plugin(tenant, "relinked"))
            .await
            .expect("insert");
        repo.collect_garbage(&[], 30).await;
        assert!(
            repo.get(tenant, created.id)
                .await
                .expect("present")
                .gc_eligible_at
                .is_some()
        );
        repo.collect_garbage(&[created.id], 30).await;
        assert!(
            repo.get(tenant, created.id)
                .await
                .expect("present")
                .gc_eligible_at
                .is_none()
        );
    }
}
