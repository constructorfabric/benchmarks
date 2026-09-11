//! Route matching: which route answers a proxied request.
//!
//! Matching is longest-prefix-first with method filtering, exactly as the PRD
//! specifies: the most specific enabled route for the caller's tenant wins,
//! `{param}` segments capture values, and a request that no route matches is
//! `ErrorKind::RouteNotFound`.

use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::model::Route;
use crate::domain::repo::RouteRepository;

/// The result of matching a request against the route table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    /// The route that won.
    pub route: Route,
    /// Captured `{param}` values, keyed by parameter name.
    pub params: std::collections::HashMap<String, String>,
    /// The route path prefix that matched.
    pub matched_prefix: String,
}

/// Find the route that answers `(method, path)` for a tenant.
///
/// `tenant_scope` is ordered descendant → root; only the first entry owns
/// routes, because a route belongs to exactly one tenant.
///
/// # Errors
/// Returns [`ErrorKind::RouteNotFound`] when no enabled route matches.
pub async fn match_route(
    repo: &Arc<dyn RouteRepository>,
    tenant_id: uuid::Uuid,
    method: &str,
    path: &str,
) -> Result<RouteMatch, DomainError> {
    let candidates = repo.list_matching(tenant_id, method, path).await?;
    let mut best: Option<Scored> = None;
    for route in candidates {
        let Some((literals, params)) = match_prefix(&route.path, path) else {
            continue;
        };
        let scored = Scored {
            literals,
            priority: route.priority,
            path: route.path.clone(),
            route,
            params,
        };
        let better = match &best {
            None => true,
            Some(current) => {
                scored.literals > current.literals
                    || (scored.literals == current.literals && scored.priority > current.priority)
                    || (scored.literals == current.literals
                        && scored.priority == current.priority
                        && scored.path < current.path)
            }
        };
        if better {
            best = Some(scored);
        }
    }
    let scored = best
        .ok_or_else(|| DomainError::route_not_found(format!("no route matches {method} {path}")))?;
    Ok(RouteMatch {
        route: scored.route,
        params: scored.params,
        matched_prefix: scored.path,
    })
}

struct Scored {
    literals: usize,
    priority: u32,
    path: String,
    route: Route,
    params: std::collections::HashMap<String, String>,
}

/// Match `template` against `path` at a segment boundary, returning the number
/// of literal segments matched and the captured parameters.
#[must_use]
pub fn match_prefix(
    template: &str,
    path: &str,
) -> Option<(usize, std::collections::HashMap<String, String>)> {
    let template_segments: Vec<&str> = template
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if template_segments.is_empty() {
        // A root route matches every path.
        return Some((0, std::collections::HashMap::new()));
    }
    let path_segments: Vec<&str> = path
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if path_segments.len() < template_segments.len() {
        return None;
    }
    let mut params = std::collections::HashMap::new();
    let mut literals = 0usize;
    for (index, segment) in template_segments.iter().enumerate() {
        let request = path_segments[index];
        if let Some(name) = parameter_name(segment) {
            if let Some(rest) = name.strip_prefix('*') {
                // `{*name}` swallows the remainder of the path.
                params.insert(rest.to_owned(), path_segments[index..].join("/"));
                literals += 1;
                return Some((literals, params));
            }
            if request.is_empty() {
                return None;
            }
            params.insert(name.to_owned(), (*request).to_owned());
        } else if *segment == request {
            literals += 1;
        } else {
            return None;
        }
    }
    Some((literals, params))
}

fn parameter_name(segment: &str) -> Option<&str> {
    let inner = segment.strip_prefix('{')?.strip_suffix('}')?;
    Some(inner)
}

#[cfg(test)]
mod matching_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::repo::RouteRepository;
    use async_trait::async_trait;

    #[derive(Default)]
    struct FakeRepo {
        routes: Vec<Route>,
    }

    #[async_trait]
    impl RouteRepository for FakeRepo {
        async fn insert(&self, _route: &Route) -> Result<(), DomainError> {
            Ok(())
        }
        async fn get(&self, _t: uuid::Uuid, _id: uuid::Uuid) -> Result<Option<Route>, DomainError> {
            Ok(None)
        }
        async fn list(&self, _t: uuid::Uuid) -> Result<Vec<Route>, DomainError> {
            Ok(self.routes.clone())
        }
        async fn list_matching(
            &self,
            _t: uuid::Uuid,
            method: &str,
            _path: &str,
        ) -> Result<Vec<Route>, DomainError> {
            Ok(self
                .routes
                .iter()
                .filter(|r| r.enabled && r.allows_method(method))
                .cloned()
                .collect())
        }
        async fn routes_referencing_alias(
            &self,
            _scope: &[uuid::Uuid],
            _alias: &str,
        ) -> Result<Vec<Route>, DomainError> {
            Ok(Vec::new())
        }
        async fn update(&self, _route: &Route) -> Result<(), DomainError> {
            Ok(())
        }
        async fn delete(&self, _t: uuid::Uuid, _id: uuid::Uuid) -> Result<bool, DomainError> {
            Ok(true)
        }
    }

    fn route(path: &str, methods: &[&str], priority: u32) -> Route {
        let mut r = Route::for_test();
        r.path = path.to_owned();
        r.methods = methods.iter().map(|s| (*s).to_owned()).collect();
        r.priority = priority;
        r
    }

    #[tokio::test]
    async fn longest_prefix_wins() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo {
            routes: vec![route("/v1", &["*"], 0), route("/v1/chat", &["*"], 0)],
        });
        let m = match_route(&repo, uuid::Uuid::nil(), "POST", "/v1/chat/completions")
            .await
            .unwrap();
        assert_eq!(m.route.path, "/v1/chat");
    }

    #[tokio::test]
    async fn method_allowlist_filters_candidates() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo {
            routes: vec![route("/v1/chat", &["GET"], 0)],
        });
        let err = match_route(&repo, uuid::Uuid::nil(), "POST", "/v1/chat")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::RouteNotFound);
    }

    #[tokio::test]
    async fn wildcard_method_matches_anything() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo {
            routes: vec![route("/v1", &["*"], 0)],
        });
        let m = match_route(&repo, uuid::Uuid::nil(), "PATCH", "/v1/x")
            .await
            .unwrap();
        assert_eq!(m.route.path, "/v1");
    }

    #[tokio::test]
    async fn parameter_segments_capture_values() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo {
            routes: vec![route("/v1/entities/{gts_id}", &["GET"], 0)],
        });
        let m = match_route(
            &repo,
            uuid::Uuid::nil(),
            "GET",
            "/v1/entities/gts.acme.core.x.v1~",
        )
        .await
        .unwrap();
        assert_eq!(
            m.params.get("gts_id").map(String::as_str),
            Some("gts.acme.core.x.v1~")
        );
    }

    #[tokio::test]
    async fn disabled_routes_never_match() {
        let mut r = route("/v1/chat", &["*"], 0);
        r.enabled = false;
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo { routes: vec![r] });
        let err = match_route(&repo, uuid::Uuid::nil(), "GET", "/v1/chat")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::RouteNotFound);
    }

    #[tokio::test]
    async fn higher_priority_wins_on_equal_specificity() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo {
            routes: vec![route("/v1", &["*"], 0), route("/v1", &["*"], 10)],
        });
        let m = match_route(&repo, uuid::Uuid::nil(), "GET", "/v1/x")
            .await
            .unwrap();
        assert_eq!(m.route.priority, 10);
    }

    #[tokio::test]
    async fn no_match_is_route_not_found() {
        let repo: Arc<dyn RouteRepository> = Arc::new(FakeRepo::default());
        let err = match_route(&repo, uuid::Uuid::nil(), "GET", "/nope")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::RouteNotFound);
        assert_eq!(err.http_status(), http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn prefix_matching_respects_segment_boundaries() {
        assert!(match_prefix("/v1", "/v1/chat").is_some());
        assert!(
            match_prefix("/v1", "/v10").is_none(),
            "/v1 must not match /v10"
        );
        assert!(match_prefix("/v1", "/v1").is_some());
        assert!(match_prefix("/", "/anything/at/all").is_some());
        assert!(match_prefix("/v1/chat", "/v1").is_none());
    }
}
