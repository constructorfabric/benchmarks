//! In-memory control-plane store.
//!
//! The gear is `stateful` but deliberately **not** `db`: this store is the
//! persistence layer. It is a `DashMap` of tenant-scoped resources guarded by a
//! write mutex that makes alias/name uniqueness checks race-free.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::{
    Plugin, Route, RouteConfig, Upstream, UpstreamConfig,
};
use crate::domain::error::DomainError;
use crate::domain::repo::{ListQuery, Page, PluginRepo, RouteRepo, UpstreamRepo};

/// In-memory store for upstreams, routes and plugins.
#[derive(Default)]
pub struct InMemoryStore {
    /// Serialises every mutation so uniqueness checks stay race-free.
    write_gate: parking_lot::Mutex<()>,
    upstreams: DashMap<(Uuid, Uuid), Upstream>,
    /// tenant → alias → upstream id.
    alias_index: DashMap<(Uuid, String), Uuid>,
    routes: DashMap<(Uuid, Uuid), Route>,
    plugins: DashMap<(Uuid, Uuid), Plugin>,
    /// tenant → plugin name → plugin id.
    plugin_name_index: DashMap<(Uuid, String), Uuid>,
}

impl std::fmt::Debug for InMemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryStore")
            .field("upstreams", &self.upstreams.len())
            .field("routes", &self.routes.len())
            .field("plugins", &self.plugins.len())
            .finish()
    }
}

impl InMemoryStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Number of stored upstreams (test/inspection helper).
    #[must_use]
    pub fn upstream_count(&self) -> usize {
        self.upstreams.len()
    }

    /// Number of stored routes (test/inspection helper).
    #[must_use]
    pub fn route_count_total(&self) -> usize {
        self.routes.len()
    }
}

/// Apply OData `$orderby` / `$top` / `$skip` / `$search` to an item list.
///
/// The default ordering the caller applied stays in place when no `$orderby`
/// is requested.
///
/// # Errors
/// [`DomainError::Validation`] when the `$orderby` expression names a field
/// the resource does not expose.
pub fn paginate<T, F>(
    items: Vec<T>,
    query: &ListQuery,
    matches_search: F,
) -> Result<Page<T>, DomainError>
where
    T: std::fmt::Debug + serde::Serialize,
    F: Fn(&T) -> bool,
{
    let mut filtered: Vec<T> = match &query.search {
        Some(needle) if !needle.is_empty() => {
            let needle = needle.to_ascii_lowercase();
            items
                .into_iter()
                .filter(|item| matches_search(item))
                .filter(|item| haystack_of(item).contains(&needle))
                .collect()
        }
        _ => items,
    };
    let total = filtered.len();
    apply_orderby(&mut filtered, query)?;
    let skipped = query.skip.unwrap_or(0);
    let mut page: Vec<T> = filtered
        .into_iter()
        .skip(skipped)
        .collect();
    if let Some(top) = query.top {
        page.truncate(top);
    }
    Ok(Page { items: page, total })
}

/// One `$orderby` term: a field name plus an optional direction.
struct OrderTerm {
    field: String,
    descending: bool,
}

/// Parse `alias desc, created_at` (the `alias asc` short form included).
fn parse_orderby(raw: &str) -> Vec<OrderTerm> {
    raw.split(',')
        .map(str::trim)
        .filter(|term| !term.is_empty())
        .map(|term| {
            let mut parts = term.split_whitespace();
            let field = parts.next().unwrap_or_default().to_owned();
            let descending = matches!(
                parts.next().map(str::to_ascii_lowercase).as_deref(),
                Some("desc") | Some("descending")
            );
            OrderTerm { field, descending }
        })
        .filter(|term| !term.field.is_empty())
        .collect()
}

/// Sort `items` by the `$orderby` expression, comparing the serialized field
/// values so the ordering matches what the client sees.
fn apply_orderby<T: serde::Serialize>(
    items: &mut Vec<T>,
    query: &ListQuery,
) -> Result<(), DomainError> {
    let Some(raw) = query.orderby.as_deref().map(str::trim).filter(|r| !r.is_empty())
    else {
        return Ok(());
    };
    let terms = parse_orderby(raw);
    if terms.is_empty() {
        return Ok(());
    }
    // Serialize once; the list endpoints are small and this keeps the ordering
    // identical to the wire representation the client sees.
    let mut keyed: Vec<(serde_json::Value, T)> = items
        .drain(..)
        .map(|item| {
            let value = serde_json::to_value(&item).unwrap_or(serde_json::Value::Null);
            (value, item)
        })
        .collect();
    for term in terms.iter().rev() {
        let name = term.field.to_ascii_lowercase();
        // A field the resource does not expose is a client error, not a
        // silent no-op: an operator asking for `orderby=naem` must learn now.
        let known = keyed.iter().any(|(value, _)| value.get(&name).is_some());
        if !known {
            return Err(DomainError::Validation(format!(
                "unsupported $orderby field '{}' (supported: {})",
                term.field,
                keyed
                    .first()
                    .and_then(|(value, _)| value.as_object())
                    .map(|fields| {
                        let mut names: Vec<String> =
                            fields.keys().map(String::to_owned).collect();
                        names.sort();
                        names.join(", ")
                    })
                    .unwrap_or_default()
            )));
        }
        keyed.sort_by(|(a, _), (b, _)| {
            let order = compare_json(
                a.get(&name).unwrap_or(&serde_json::Value::Null),
                b.get(&name).unwrap_or(&serde_json::Value::Null),
            );
            if term.descending {
                order.reverse()
            } else {
                order
            }
        });
    }
    items.extend(keyed.into_iter().map(|(_, item)| item));
    Ok(())
}

/// Total order over the JSON values an `$orderby` can sort on.
fn compare_json(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x
            .as_f64()
            .partial_cmp(&y.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        // Nulls sort last, whichever direction the client asked for.
        (Value::Null, _) => std::cmp::Ordering::Greater,
        (_, Value::Null) => std::cmp::Ordering::Less,
        (Value::Array(x), Value::Array(y)) => {
            for (a, b) in x.iter().zip(y.iter()) {
                let order = compare_json(a, b);
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            x.len().cmp(&y.len())
        }
        (x, y) => x.to_string().to_ascii_lowercase().cmp(&y.to_string().to_ascii_lowercase()),
    }
}

/// Text haystack used for `$search`.
fn haystack_of(item: &dyn std::fmt::Debug) -> String {
    format!("{item:?}").to_ascii_lowercase()
}

/// Lower-case a tag list for stable sorting.
fn sort_key(tags: &[String]) -> BTreeMap<String, String> {
    tags.iter().map(|t| (t.to_ascii_lowercase(), String::new())).collect()
}

#[async_trait]
impl UpstreamRepo for InMemoryStore {
    async fn create(&self, upstream: Upstream) -> Result<Upstream, DomainError> {
        let _gate = self.write_gate.lock();
        let key = (upstream.tenant_id, upstream.id);
        if self.upstreams.contains_key(&key) {
            return Err(DomainError::AliasConflict(upstream.id.to_string()));
        }
        if let Some(alias) = upstream.config.alias.as_deref() {
            let alias_key = (upstream.tenant_id, alias.to_owned());
            if self.alias_index.contains_key(&alias_key) {
                return Err(DomainError::AliasConflict(alias.to_owned()));
            }
            self.alias_index.insert(alias_key, upstream.id);
        }
        self.upstreams.insert(key, upstream.clone());
        Ok(upstream)
    }

    async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: UpstreamConfig,
    ) -> Result<Upstream, DomainError> {
        let _gate = self.write_gate.lock();
        let key = (tenant_id, id);
        let mut entry = self
            .upstreams
            .get_mut(&key)
            .ok_or(DomainError::UpstreamNotFound)?;
        let previous_alias = entry.config.alias.clone();
        let alias = config.alias.clone();
        if previous_alias != alias {
            if let Some(old) = previous_alias.as_deref() {
                self.alias_index.remove(&(tenant_id, old.to_owned()));
            }
            if let Some(new) = alias.as_deref() {
                let alias_key = (tenant_id, new.to_owned());
                if self
                    .alias_index
                    .get(&alias_key)
                    .is_some_and(|existing| *existing != id)
                {
                    drop(entry);
                    return Err(DomainError::AliasConflict(new.to_owned()));
                }
                self.alias_index.insert(alias_key, id);
            }
        }
        entry.config = config;
        let updated = entry.clone();
        drop(entry);
        Ok(updated)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError> {
        self.upstreams
            .get(&(tenant_id, id))
            .map(|e| e.clone())
            .ok_or(DomainError::UpstreamNotFound)
    }

    async fn get_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError> {
        let id = *self
            .alias_index
            .get(&(tenant_id, alias.to_ascii_lowercase()))
            .ok_or(DomainError::UpstreamNotFound)?;
        crate::domain::UpstreamRepo::get(self, tenant_id, id).await
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let _gate = self.write_gate.lock();
        let key = (tenant_id, id);
        let removed = self.upstreams.remove(&key).ok_or(DomainError::UpstreamNotFound)?;
        if let Some(alias) = removed.1.config.alias {
            self.alias_index.remove(&(tenant_id, alias));
        }
        Ok(())
    }

    async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Upstream>, DomainError> {
        if let Some(filter) = &query.filter {
            let normalized = filter.replace(' ', "").to_ascii_lowercase();
            if !(normalized.starts_with("aliaseq") || normalized.is_empty()) {
                return Err(DomainError::Validation(format!(
                    "unsupported $filter expression '{filter}' (only 'alias eq …' is supported)"
                )));
            }
        }
        let mut items: Vec<Upstream> = self
            .upstreams
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        if let Some(filter) = &query.filter {
            let normalized = filter.replace(' ', "").to_ascii_lowercase();
            if let Some(value) = normalized
                .strip_prefix("aliaseq")
                .map(|v| v.trim_matches('\'').trim_matches('"').to_owned())
            {
                items.retain(|u| {
                    u.config
                        .alias
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case(&value))
                });
            }
        }
        items.sort_by(|a, b| {
            a.config
                .alias
                .cmp(&b.config.alias)
                .then_with(|| sort_key(&a.config.tags).cmp(&sort_key(&b.config.tags)))
                .then_with(|| a.id.cmp(&b.id))
        });
        paginate(items, query, |_| true)
    }

    async fn route_count(&self, tenant_id: Uuid, id: Uuid) -> Result<usize, DomainError> {
        Ok(self
            .routes
            .iter()
            .filter(|entry| {
                let key = entry.key();
                key.0 == tenant_id
                    && (key.1 == id || entry.value().config.upstream_id == id)
            })
            .count())
    }

    async fn find_by_alias_any(&self, alias: &str) -> Result<Vec<Upstream>, DomainError> {
        let needle = alias.to_ascii_lowercase();
        let mut items: Vec<Upstream> = self
            .upstreams
            .iter()
            .filter(|entry| {
                entry
                    .value()
                    .config
                    .alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&needle))
            })
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id)));
        Ok(items)
    }
}

#[async_trait]
impl RouteRepo for InMemoryStore {
    async fn create(&self, route: Route) -> Result<Route, DomainError> {
        let _gate = self.write_gate.lock();
        let key = (route.tenant_id, route.id);
        if self.routes.contains_key(&key) {
            return Err(DomainError::RouteConflict);
        }
        let collision = self
            .routes
            .iter()
            .any(|entry| entry.key().0 == route.tenant_id && collides(entry.value(), &route.config));
        if collision {
            return Err(DomainError::RouteConflict);
        }
        self.routes.insert(key, route.clone());
        Ok(route)
    }

    async fn update(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        config: RouteConfig,
    ) -> Result<Route, DomainError> {
        let _gate = self.write_gate.lock();
        let key = (tenant_id, id);
        let collision = self
            .routes
            .iter()
            .any(|entry| entry.key() != &key && collides(entry.value(), &config));
        if collision {
            return Err(DomainError::RouteConflict);
        }
        let mut entry = self
            .routes
            .get_mut(&key)
            .ok_or(DomainError::RouteNotFound)?;
        // `Route.upstream_id` is immutable (DESIGN §"Immutable fields"): the
        // denormalized field and the config copy must keep agreeing even if a
        // caller bypasses the service layer.
        let mut config = config;
        config.upstream_id = entry.config.upstream_id;
        entry.config = config;
        Ok(entry.clone())
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .get(&(tenant_id, id))
            .map(|e| e.clone())
            .ok_or(DomainError::RouteNotFound)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        self.routes
            .remove(&(tenant_id, id))
            .map(|_| ())
            .ok_or(DomainError::RouteNotFound)
    }

    async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| {
            path_of(&a.config.matcher)
                .cmp(&path_of(&b.config.matcher))
                .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
        });
        paginate(items, query, |_| true)
    }

    async fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError> {
        let mut items: Vec<Route> = self
            .routes
            .iter()
            .filter(|entry| {
                let key = entry.key();
                key.0 == tenant_id && entry.value().config.upstream_id == upstream_id
            })
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| path_of(&a.config.matcher).cmp(&path_of(&b.config.matcher)));
        Ok(items)
    }

    async fn find_match(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<Route>, DomainError> {
        let method_upper = method.to_ascii_uppercase();
        let mut best: Option<(u32, Route)> = None;
        for entry in self.routes.iter() {
            let key = entry.key();
            if key.0 != tenant_id || entry.value().config.upstream_id != upstream_id {
                continue;
            }
            let route = entry.value();
            if !route.config.enabled {
                // A disabled route leaves the matching space entirely, so a
                // more specific but disabled rule never shadows a live one.
                continue;
            }
            let Some(score) = route_score(&route.config.matcher, &method_upper, path, query) else {
                continue;
            };
            if best.as_ref().is_none_or(|(best_score, _)| score > *best_score) {
                best = Some((score, route.clone()));
            }
        }
        Ok(best.map(|(_, route)| route))
    }
}

fn path_of(matcher: &crate::domain::dto::MatchRules) -> String {
    match matcher {
        crate::domain::dto::MatchRules::Http(m) => m.path.clone(),
        crate::domain::dto::MatchRules::Grpc(m) => m.service.clone(),
    }
}

/// `true` when `candidate` claims the same (method, path) space as `existing`.
fn collides(existing: &Route, candidate: &RouteConfig) -> bool {
    if existing.config.upstream_id != candidate.upstream_id {
        return false;
    }
    match (&existing.config.matcher, &candidate.matcher) {
        (
            crate::domain::dto::MatchRules::Http(a),
            crate::domain::dto::MatchRules::Http(b),
        ) => {
            let same_path = a.path.eq_ignore_ascii_case(&b.path);
            if !same_path {
                return false;
            }
            let a_all = a.methods.iter().any(|m| m == "*");
            let b_all = b.methods.iter().any(|m| m == "*");
            if a_all || b_all {
                return true;
            }
            a.methods.iter().any(|m| {
                b.methods
                    .iter()
                    .any(|other| other.eq_ignore_ascii_case(m))
            })
        }
        (
            crate::domain::dto::MatchRules::Grpc(a),
            crate::domain::dto::MatchRules::Grpc(b),
        ) => a.service == b.service && (a.method == b.method || a.method.is_empty() || b.method.is_empty()),
        _ => false,
    }
}

/// Specificity score, or `None` when the route does not match.
fn route_score(
    matcher: &crate::domain::dto::MatchRules,
    method: &str,
    path: &str,
    query: &[(String, String)],
) -> Option<u32> {
    let crate::domain::dto::MatchRules::Http(http_match) = matcher else {
        return None;
    };
    let method_ok = http_match
        .methods
        .iter()
        .any(|m| m == "*" || m.eq_ignore_ascii_case(method));
    if !method_ok {
        return None;
    }
    let pattern = http_match.path.trim_end_matches('/');
    let candidate = path.trim_end_matches('/');
    let path_ok = if candidate == pattern {
        true
    } else {
        matches!(http_match.path_suffix_mode, crate::domain::dto::PathSuffixMode::Append)
            && candidate.starts_with(pattern)
            && (candidate.as_bytes().get(pattern.len()) == Some(&b'/'))
    };
    if !path_ok {
        return None;
    }
    if !http_match.query_allowlist.is_empty() {
        let allowed: Vec<&str> = http_match
            .query_allowlist
            .iter()
            .map(String::as_str)
            .collect();
        if query.iter().any(|(k, _)| !allowed.contains(&k.as_str())) {
            return None;
        }
    } else if !query.is_empty() {
        return None;
    }

    let mut score: u32 = 1;
    score += u32::try_from(pattern.split('/').count()).unwrap_or(u32::MAX) * 10;
    if !http_match.methods.iter().any(|m| m == "*") {
        score += 100;
    }
    if !http_match.query_allowlist.is_empty() {
        score += 1_000;
    }
    Some(score)
}

#[async_trait]
impl PluginRepo for InMemoryStore {
    async fn create(&self, plugin: Plugin) -> Result<Plugin, DomainError> {
        let _gate = self.write_gate.lock();
        let key = (plugin.tenant_id, plugin.id);
        if self.plugins.contains_key(&key) {
            return Err(DomainError::PluginInUse);
        }
        let name_key = (plugin.tenant_id, plugin.name.to_ascii_lowercase());
        if self.plugin_name_index.contains_key(&name_key) {
            return Err(DomainError::Validation(format!(
                "a plugin named '{}' already exists in this tenant",
                plugin.name
            )));
        }
        self.plugin_name_index.insert(name_key, plugin.id);
        self.plugins.insert(key, plugin.clone());
        Ok(plugin)
    }

    async fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError> {
        self.plugins
            .get(&(tenant_id, id))
            .map(|e| e.clone())
            .ok_or(DomainError::PluginNotFound)
    }

    async fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        let _gate = self.write_gate.lock();
        let key = (tenant_id, id);
        let removed = self.plugins.remove(&key).ok_or(DomainError::PluginNotFound)?;
        self.plugin_name_index
            .remove(&(tenant_id, removed.1.name.to_ascii_lowercase()));
        Ok(())
    }

    async fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Page<Plugin>, DomainError> {
        let mut items: Vec<Plugin> = self
            .plugins
            .iter()
            .filter(|entry| entry.key().0 == tenant_id)
            .map(|entry| entry.value().clone())
            .collect();
        items.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        paginate(items, query, |_| true)
    }

    async fn reference_count(&self, tenant_id: Uuid, id: Uuid) -> Result<usize, DomainError> {
        let needle = id.to_string();
        let count_upstreams = self
            .upstreams
            .iter()
            .filter(|entry| {
                let key = entry.key();
                key.0 == tenant_id && entry.value().config.plugins.items.iter().any(|r| r.contains(&needle))
            })
            .count();
        let count_routes = self
            .routes
            .iter()
            .filter(|entry| {
                let key = entry.key();
                key.0 == tenant_id && entry.value().config.plugins.items.iter().any(|r| r.contains(&needle))
            })
            .count();
        Ok(count_upstreams + count_routes)
    }
}

/// `Mutated` is re-exported from [`crate::domain::repo`] for the REST layer.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Endpoint, EndpointScheme, HttpMatch, MatchRules, Server};

    async fn put_upstream(
        store: &InMemoryStore,
        upstream: Upstream,
    ) -> Result<Upstream, crate::domain::error::DomainError> {
        crate::domain::UpstreamRepo::create(store, upstream).await
    }

    async fn put_route(
        store: &InMemoryStore,
        route: Route,
    ) -> Result<Route, crate::domain::error::DomainError> {
        crate::domain::RouteRepo::create(store, route).await
    }

    fn upstream(alias: &str, host: &str) -> Upstream {
        Upstream {
            id: Uuid::now_v7(),
            tenant_id: Uuid::nil(),
            created_at: 0,
            config: UpstreamConfig {
                enabled: true,
                alias: Some(alias.to_owned()),
                server: Server {
                    endpoints: vec![Endpoint {
                        scheme: EndpointScheme::Https,
                        host: host.to_owned(),
                        port: Some(443),
                    }],
                },
                ..UpstreamConfig::default()
            },
        }
    }

    fn route(upstream_id: Uuid, path: &str, methods: &[&str]) -> Route {
        Route {
            id: Uuid::now_v7(),
            tenant_id: Uuid::nil(),
            upstream_id,
            created_at: 0,
            config: RouteConfig {
                upstream_id,
                matcher: MatchRules::Http(HttpMatch {
                    methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                    path: path.to_owned(),
                    ..HttpMatch::default()
                }),
                ..RouteConfig::default()
            },
        }
    }

    #[tokio::test]
    async fn alias_index_is_maintained() {
        let store = InMemoryStore::new();
        let u = upstream("api.openai.com", "api.openai.com");
        let id = u.id;
        put_upstream(&store, u).await.expect("created");
        assert!(store.get_by_alias(Uuid::nil(), "api.openai.com").await.is_ok());
        assert!(store.get_by_alias(Uuid::nil(), "other").await.is_err());
        crate::domain::UpstreamRepo::delete(store.as_ref(), Uuid::nil(), id)
            .await
            .expect("deleted");
        assert!(store.get_by_alias(Uuid::nil(), "api.openai.com").await.is_err());
    }

    #[tokio::test]
    async fn duplicate_alias_is_rejected() {
        let store = InMemoryStore::new();
        put_upstream(&store, upstream("dup.example", "a.example"))
            .await
            .expect("first");
        assert!(put_upstream(&store, upstream("dup.example", "b.example"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn route_matching_prefers_the_most_specific() {
        let store = InMemoryStore::new();
        let u = upstream("api.example", "api.example");
        let upstream_id = u.id;
        put_upstream(&store, u).await.expect("upstream");
        let broad = route(upstream_id, "/v1", &["*"]);
        let broad_id = broad.id;
        put_route(&store, broad).await.expect("broad");
        let narrow = route(upstream_id, "/v1/chat", &["POST"]);
        let narrow_id = narrow.id;
        put_route(&store, narrow).await.expect("narrow");

        let hit = store
            .find_match(Uuid::nil(), upstream_id, "POST", "/v1/chat/completions", &[])
            .await
            .expect("match")
            .expect("narrow wins");
        assert_eq!(hit.id, narrow_id);

        let hit = store
            .find_match(Uuid::nil(), upstream_id, "GET", "/v1/models", &[])
            .await
            .expect("match")
            .expect("broad wins");
        assert_eq!(hit.id, broad_id);

        assert!(
            store
                .find_match(Uuid::nil(), upstream_id, "GET", "/other", &[])
                .await
                .expect("match")
                .is_none()
        );
    }

    #[tokio::test]
    async fn overlapping_routes_conflict() {
        let store = InMemoryStore::new();
        let u = upstream("api.example", "api.example");
        let upstream_id = u.id;
        put_upstream(&store, u).await.expect("upstream");
        put_route(&store, route(upstream_id, "/v1", &["GET"]))
            .await
            .expect("first");
        assert!(put_route(&store, route(upstream_id, "/v1", &["GET"])).await.is_err());
    }

    #[tokio::test]
    async fn query_allowlist_rejects_unknown_params() {
        let store = InMemoryStore::new();
        let u = upstream("api.example", "api.example");
        let upstream_id = u.id;
        put_upstream(&store, u).await.expect("upstream");
        put_route(
            &store,
            Route {
                id: Uuid::now_v7(),
                tenant_id: Uuid::nil(),
                upstream_id,
                created_at: 0,
                config: RouteConfig {
                    upstream_id,
                    matcher: MatchRules::Http(HttpMatch {
                        methods: vec![String::from("GET")],
                        path: String::from("/v1"),
                        query_allowlist: vec![String::from("limit")],
                        ..HttpMatch::default()
                    }),
                    ..RouteConfig::default()
                },
            },
        )
        .await
        .expect("route");
        let ok = store
            .find_match(
                Uuid::nil(),
                upstream_id,
                "GET",
                "/v1",
                &[("limit".into(), "10".into())],
            )
            .await
            .expect("match");
        assert!(ok.is_some());
        let bad = store
            .find_match(
                Uuid::nil(),
                upstream_id,
                "GET",
                "/v1",
                &[("nope".into(), "1".into())],
            )
            .await
            .expect("match");
        assert!(bad.is_none());
    }
}
