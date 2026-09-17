//! Route CRUD (control plane).
//!
//! Enforces the DESIGN.md §3.4 route semantics: `upstream_id` must belong to
//! the calling tenant (ancestor upstreams are not addressable), `upstream_id`
//! is immutable on replace, and match rules are unique within an upstream.

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{
    GrpcMatch, HttpMatch, MatchConfig, Route, RouteSpec, RouteUpdate, UpstreamProtocol,
    validate_tag,
};
use crate::domain::repo::{Repositories, RouteRepository, UpstreamRepository};
use crate::domain::services::ListQuery;

/// Route management operations.
#[derive(Clone)]
pub struct RouteService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    /// L1 data-plane cache, invalidated on every write (ADR 0005).
    cache: Option<Arc<crate::infra::cache::ConfigCache>>,
}

impl std::fmt::Debug for RouteService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteService").finish_non_exhaustive()
    }
}

impl RouteService {
    /// Builds a service over the repository bundle.
    #[must_use]
    pub fn new(repos: &Repositories) -> Self {
        Self {
            upstreams: repos.upstreams.clone(),
            routes: repos.routes.clone(),
            cache: None,
        }
    }

    /// Shares the data plane's L1 cache with this service, so a write
    /// invalidates the proxied requests' copy in the same tick.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::infra::cache::ConfigCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Bumps the L1 cache generation after a successful write.
    fn invalidate(&self) {
        if let Some(cache) = self.cache.as_ref() {
            cache.invalidate();
        }
    }

    /// Creates a route.
    ///
    /// # Errors
    ///
    /// * [`DomainError::ImmutableField`] (409) for caller-supplied `id`/`tenant_id`.
    /// * [`DomainError::Validation`] (400) for an invalid body.
    /// * [`DomainError::NotFound`] (404) when `upstream_id` is not the caller's.
    /// * [`DomainError::DuplicateRouteMatch`] (409) on a match-rule collision.
    pub fn create(&self, tenant_id: Uuid, spec: RouteSpec) -> Result<Route, DomainError> {
        check_immutable_ids(tenant_id, spec.id, spec.tenant_id)?;
        let upstream_id = spec
            .upstream_id
            .ok_or_else(|| DomainError::Validation("upstream_id is required".to_owned()))?;
        validate_match(&spec.match_rules)?;
        validate_tags(&spec.tags)?;
        self.ensure_upstream(tenant_id, upstream_id)?;
        ensure_no_duplicate(&*self.routes, upstream_id, &spec.match_rules, None)?;

        let route = Route::from_spec(spec, Uuid::new_v4(), tenant_id, upstream_id);
        let inserted = self.routes.insert(route)?;
        self.invalidate();
        Ok(inserted)
    }

    /// Replaces a route; `upstream_id` is immutable and omitted optionals are
    /// cleared.
    ///
    /// # Errors
    ///
    /// * [`DomainError::NotFound`] (404) for an unknown id.
    /// * [`DomainError::ImmutableField`] (409) for `id`/`upstream_id` overrides.
    /// * [`DomainError::DuplicateRouteMatch`] (409) on a match-rule collision.
    pub fn replace(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        update: RouteUpdate,
    ) -> Result<Route, DomainError> {
        if update.id.is_some() {
            return Err(DomainError::ImmutableField {
                resource: "route",
                field: "id",
            });
        }
        let existing = self
            .routes
            .find(tenant_id, id)?
            .ok_or_else(|| DomainError::NotFound(route_not_found(id)))?;
        if let Some(upstream_id) = update.upstream_id
            && upstream_id != existing.upstream_id
        {
            return Err(DomainError::ImmutableField {
                resource: "route",
                field: "upstream_id",
            });
        }
        validate_match(&update.match_rules)?;
        validate_tags(&update.tags)?;
        ensure_no_duplicate(
            &*self.routes,
            existing.upstream_id,
            &update.match_rules,
            Some(existing.id),
        )?;

        let route = Route {
            id: existing.id,
            tenant_id: existing.tenant_id,
            upstream_id: existing.upstream_id,
            tags: update.tags,
            match_rules: update.match_rules,
            plugins: update.plugins,
            rate_limit: update.rate_limit,
        };
        let replaced = self.routes.replace(route)?;
        self.invalidate();
        Ok(replaced)
    }

    /// Loads one route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the id is unknown to this tenant.
    pub fn get(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError> {
        self.routes
            .find(tenant_id, id)?
            .ok_or_else(|| DomainError::NotFound(route_not_found(id)))
    }

    /// Lists the routes of one tenant, applying the list-query subset.
    ///
    /// # Errors
    ///
    /// Storage failures surface as [`DomainError::Internal`].
    pub fn list(&self, tenant_id: Uuid, query: &ListQuery) -> Result<Vec<Route>, DomainError> {
        let mut items = self.routes.list(tenant_id)?;
        if let Some(raw) = query.filter_value("upstream_id") {
            let upstream_id = Uuid::parse_str(raw).map_err(|_| {
                DomainError::Validation("upstream_id filter must be a UUID".to_owned())
            })?;
            items.retain(|r| r.upstream_id == upstream_id);
        }
        if let Some(tag) = query.filter_value("tag") {
            items.retain(|r| r.tags.iter().any(|t| t == tag));
        }
        if let Some((field, descending)) = &query.order_by {
            match field.as_str() {
                "upstream_id" => {
                    sort_by(&mut items, *descending, |r| r.upstream_id.to_string())
                }
                "id" => sort_by(&mut items, *descending, |r| r.id.to_string()),
                _ => {}
            }
        }
        Ok(page(&mut items, query))
    }

    /// Deletes a route.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotFound`] when the id is unknown to this tenant.
    pub fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<(), DomainError> {
        if self.routes.delete(tenant_id, id)? {
            self.invalidate();
            Ok(())
        } else {
            Err(DomainError::NotFound(route_not_found(id)))
        }
    }

    /// `404` unless `upstream_id` belongs to the calling tenant.
    fn ensure_upstream(&self, tenant_id: Uuid, upstream_id: Uuid) -> Result<(), DomainError> {
        self.upstreams
            .find(tenant_id, upstream_id)?
            .ok_or_else(|| DomainError::NotFound(format!("no upstream with id {upstream_id}")))?;
        Ok(())
    }
}

/// Sorts `items` by `key`, ascending or descending, in place.
fn sort_by<T, F>(items: &mut [T], descending: bool, mut key: F)
where
    F: FnMut(&T) -> String,
{
    if descending {
        items.sort_by_key(|a| std::cmp::Reverse(key(a)));
    } else {
        items.sort_by_key(|a| key(a));
    }
}

/// Applies `$skip`/`$top` (default 50, max 100) and consumes `items`.
fn page<T>(items: &mut Vec<T>, query: &ListQuery) -> Vec<T> {
    let top = query.top.unwrap_or(50).min(100);
    let skip = query.skip.unwrap_or(0);
    if skip >= items.len() {
        items.clear();
        return Vec::new();
    }
    items.drain(..skip);
    items.truncate(top);
    std::mem::take(items)
}

fn check_immutable_ids(
    tenant_id: Uuid,
    id: Option<Uuid>,
    supplied_tenant: Option<Uuid>,
) -> Result<(), DomainError> {
    if id.is_some() {
        return Err(DomainError::ImmutableField {
            resource: "route",
            field: "id",
        });
    }
    if supplied_tenant.is_some() && supplied_tenant != Some(tenant_id) {
        return Err(DomainError::ImmutableField {
            resource: "route",
            field: "tenant_id",
        });
    }
    Ok(())
}

fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        validate_tag(tag)?;
    }
    Ok(())
}

/// Validates the protocol-scoped match rules (`route.v1.schema.json`).
///
/// Exactly one of `http`/`grpc` must be present; HTTP matches need 1.. methods
/// and a leading-slash path, gRPC matches a fully qualified service name.
///
/// # Errors
///
/// [`DomainError::Validation`] for every structural violation.
pub fn validate_match(match_rules: &MatchConfig) -> Result<(), DomainError> {
    match (&match_rules.http, &match_rules.grpc) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(grpc)) => validate_grpc_match(grpc),
        (Some(_), Some(_)) => Err(DomainError::Semantic(
            "match must set exactly one of http or grpc".to_owned(),
        )),
        (None, None) => Err(DomainError::Semantic(
            "match must set exactly one of http or grpc".to_owned(),
        )),
    }
}

fn validate_http_match(match_rules: &HttpMatch) -> Result<(), DomainError> {
    if match_rules.methods.is_empty() {
        return Err(DomainError::Validation(
            "match.http.methods must list at least one method".to_owned(),
        ));
    }
    for method in &match_rules.methods {
        if !crate::domain::models::is_known_http_method(method.as_str()) {
            return Err(DomainError::Validation(format!(
                "match.http.methods entry '{}' is not a supported HTTP method",
                method.as_str()
            )));
        }
    }
    if match_rules.path.is_empty() {
        return Err(DomainError::Validation(
            "match.http.path must not be empty".to_owned(),
        ));
    }
    if !match_rules.path.starts_with('/') {
        return Err(DomainError::Validation(format!(
            "match.http.path '{}' must start with '/'",
            match_rules.path
        )));
    }
    if let Some(dup) = duplicated(match_rules.methods.iter().map(|m| m.as_str())) {
        return Err(DomainError::Validation(format!(
            "match.http.methods lists '{dup}' twice"
        )));
    }
    for name in &match_rules.query_allowlist {
        if name.is_empty() || name.chars().any(char::is_whitespace) {
            return Err(DomainError::Validation(format!(
                "match.http.query_allowlist entry '{name}' is not a valid query parameter name"
            )));
        }
    }
    Ok(())
}

fn validate_grpc_match(match_rules: &GrpcMatch) -> Result<(), DomainError> {
    for (field, value) in [("service", &match_rules.service), ("method", &match_rules.method)] {
        if value.is_empty() {
            return Err(DomainError::Validation(format!(
                "match.grpc.{field} must not be empty"
            )));
        }
        if value.contains(char::is_whitespace) {
            return Err(DomainError::Validation(format!(
                "match.grpc.{field} '{value}' must not contain whitespace"
            )));
        }
    }
    Ok(())
}

/// First duplicated entry of `values`, if any.
fn duplicated<'a, I>(values: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut seen = std::collections::BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Some(value.to_owned());
        }
    }
    None
}

/// Rejects a second enabled route matching the same `(path, methods)` triple on
/// one upstream.
///
/// # Errors
///
/// [`DomainError::DuplicateRouteMatch`].
fn ensure_no_duplicate(
    repo: &dyn RouteRepository,
    upstream_id: Uuid,
    match_rules: &MatchConfig,
    allowed: Option<Uuid>,
) -> Result<(), DomainError> {
    if matches!(match_rules.protocol(), UpstreamProtocol::Grpc) {
        return Ok(());
    }
    let http = match_rules.http.as_ref().ok_or_else(|| {
        DomainError::Semantic("match must set exactly one of http or grpc".to_owned())
    })?;
    for other in repo.list_by_upstream(upstream_id)? {
        if allowed == Some(other.id) {
            continue;
        }
        let Some(other_http) = other.match_rules.http.as_ref() else {
            continue;
        };
        let overlaps = other_http
            .methods
            .iter()
            .any(|m| http.methods.contains(m));
        if other_http.path == http.path && overlaps {
            return Err(DomainError::DuplicateRouteMatch(format!(
                "a route for upstream {upstream_id} already matches {} {}",
                other_http.methods[0].as_str(),
                other_http.path
            )));
        }
    }
    Ok(())
}

fn route_not_found(id: Uuid) -> String {
    format!("no route with id {id}")
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{HttpMethod, PathSuffixMode};

    fn http_match() -> MatchConfig {
        MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1/models".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        }
    }

    #[test]
    fn match_rules_must_pick_exactly_one_protocol() {
        assert!(validate_match(&http_match()).is_ok());
        let both = MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: Some(GrpcMatch {
                service: "svc.v1.S".to_owned(),
                method: "Get".to_owned(),
            }),
        };
        assert!(matches!(
            validate_match(&both),
            Err(DomainError::Semantic(_))
        ));
        assert!(validate_match(&MatchConfig::default()).is_err());
    }

    #[test]
    fn http_matches_need_methods_and_a_leading_slash() {
        let mut rules = http_match();
        rules.http.as_mut().unwrap().methods.clear();
        assert!(matches!(
            validate_match(&rules),
            Err(DomainError::Validation(_))
        ));

        let mut rules = http_match();
        rules.http.as_mut().unwrap().path = "v1/models".to_owned();
        assert!(matches!(
            validate_match(&rules),
            Err(DomainError::Validation(_))
        ));

        let mut rules = http_match();
        rules.http.as_mut().unwrap().methods = vec![HttpMethod::Get, HttpMethod::Get];
        assert!(matches!(
            validate_match(&rules),
            Err(DomainError::Validation(_))
        ));
    }

    #[test]
    fn grpc_matches_need_service_and_method() {
        let rules = MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: "foo.v1.UserService".to_owned(),
                method: "GetUser".to_owned(),
            }),
        };
        assert!(validate_match(&rules).is_ok());
        let bad = MatchConfig {
            grpc: Some(GrpcMatch {
                service: " ".to_owned(),
                method: "GetUser".to_owned(),
            }),
            http: None,
        };
        assert!(validate_match(&bad).is_err());
    }

    #[test]
    fn duplicate_detection_ignores_self() {
        let other_id = Uuid::new_v4();
        let existing = Route {
            id: other_id,
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            tags: Vec::new(),
            match_rules: http_match(),
            plugins: Default::default(),
            rate_limit: None,
        };
        // In-memory stand-in for the repository trait object.
        let repo = RecordingRepo {
            routes: vec![existing],
        };
        // Another route with the same match rules is a duplicate.
        assert!(matches!(
            ensure_no_duplicate(&repo, Uuid::nil(), &http_match(), Some(Uuid::new_v4())),
            Err(DomainError::DuplicateRouteMatch(_))
        ));
        // The route re-supplying its own id is exempt.
        assert!(ensure_no_duplicate(&repo, Uuid::nil(), &http_match(), Some(other_id)).is_ok());
        // A different path, or a non-overlapping method set, is allowed.
        let mut other_path = http_match();
        other_path.http.as_mut().unwrap().path = "/v1/other".to_owned();
        assert!(ensure_no_duplicate(&repo, Uuid::nil(), &other_path, Some(other_id)).is_ok());
        let mut other_method = http_match();
        other_method.http.as_mut().unwrap().methods = vec![HttpMethod::Post];
        assert!(ensure_no_duplicate(&repo, Uuid::nil(), &other_method, Some(other_id)).is_ok());
        // Re-supplying the same match rules without the self id is a duplicate.
        assert!(matches!(
            ensure_no_duplicate(&repo, Uuid::nil(), &http_match(), None),
            Err(DomainError::DuplicateRouteMatch(_))
        ));
    }

    #[test]
    fn page_and_sort_helpers_behave() {
        let mut items = vec!["b", "a", "c"];
        sort_by(&mut items, false, |s| (*s).to_owned());
        assert_eq!(items, ["a", "b", "c"]);
        sort_by(&mut items, true, |s| (*s).to_owned());
        assert_eq!(items, ["c", "b", "a"]);
    }

    /// Minimal [`RouteRepository`] stand-in used by the duplicate-match test.
    struct RecordingRepo {
        routes: Vec<Route>,
    }

impl crate::domain::repo::RouteRepository for RecordingRepo {
    fn insert(&self, _route: Route) -> Result<Route, DomainError> {
        Err(DomainError::Internal("not used".to_owned()))
    }

    fn find(&self, _tenant_id: Uuid, _id: Uuid) -> Result<Option<Route>, DomainError> {
        Ok(None)
    }

    fn list(&self, _tenant_id: Uuid) -> Result<Vec<Route>, DomainError> {
        Ok(self.routes.clone())
    }

    fn list_by_upstream(&self, upstream_id: Uuid) -> Result<Vec<Route>, DomainError> {
        Ok(self
            .routes
            .iter()
            .filter(|r| r.upstream_id == upstream_id)
            .cloned()
            .collect())
    }

    fn replace(&self, _route: Route) -> Result<Route, DomainError> {
        Err(DomainError::Internal("not used".to_owned()))
    }

    fn delete(&self, _tenant_id: Uuid, _id: Uuid) -> Result<bool, DomainError> {
        Ok(false)
    }

    fn delete_by_upstream(&self, _upstream_id: Uuid) -> Result<usize, DomainError> {
        Ok(0)
    }
}
}
