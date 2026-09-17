//! Route matching (DESIGN.md §3.3 “Proxy API”, ADR-0001 Appendix A).
//!
//! HTTP upstreams match with a method allowlist plus a longest path prefix;
//! gRPC upstreams match on `(service, method)` parsed from the request path.
//! The suffix captured from the proxy URL is either appended to
//! `match.http.path` or rejected, depending on `path_suffix_mode`.

use crate::domain::model::Route;

/// Result of matching a request against a route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    /// Path forwarded to the upstream: `match.http.path` plus the appended
    /// suffix when `path_suffix_mode` is `append`.
    pub upstream_path: String,
}

/// Suffix captured from `/oagw/v1/proxy/{alias}/…`, normalized to either be
/// empty or start with `/`.
#[must_use]
pub fn normalize_suffix(suffix: &str) -> String {
    let trimmed = suffix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("/{trimmed}")
    }
}

/// Whether `suffix` is accepted by `path` and what remains of it.
fn suffix_split(path: &str, suffix: &str, append: bool) -> Option<String> {
    if !append {
        return (suffix.is_empty()).then(String::new);
    }
    if path == "/" {
        return Some(suffix.trim_start_matches('/').to_owned());
    }
    if suffix == path {
        return Some(String::new());
    }
    let rest = suffix.strip_prefix(path)?;
    let rest = rest.strip_prefix('/')?;
    Some(rest.to_owned())
}

/// Tries to match an HTTP route; returns the upstream path on success.
///
/// `suffix` must already be normalized with [`normalize_suffix`].
#[must_use]
pub fn match_http(route: &Route, method: &str, suffix: &str) -> Option<RouteMatch> {
    let http = route.spec.match_rule.http.as_ref()?;
    if http.methods.iter().all(|allowed| allowed != method) {
        return None;
    }
    let append = http.path_suffix_mode.is_append();
    let remaining = suffix_split(&http.path, suffix, append)?;
    let upstream_path = if remaining.is_empty() {
        http.path.clone()
    } else if http.path.ends_with('/') {
        format!("{}{remaining}", http.path)
    } else {
        format!("{}/{remaining}", http.path.trim_end_matches('/'))
    };
    Some(RouteMatch { upstream_path })
}

/// Tries to match a gRPC route against `/service/method`.
#[must_use]
pub fn match_grpc(route: &Route, path: &str, method: &str) -> Option<RouteMatch> {
    let grpc = route.spec.match_rule.grpc.as_ref()?;
    if method != "POST" {
        return None;
    }
    let expected = format!("/{}/{}", grpc.service, grpc.method);
    if path != expected {
        return None;
    }
    Some(RouteMatch {
        upstream_path: expected,
    })
}

/// Selects the best route among `routes`: the HTTP route with the longest
/// `match.http.path` wins, then the lowest id for determinism.
#[must_use]
pub fn select<'a>(routes: &'a [Route], method: &str, suffix: &str) -> Option<&'a Route> {
    let path_len = |route: &Route| {
        route
            .spec
            .match_rule
            .http
            .as_ref()
            .map(|http| http.path.len())
            .unwrap_or(0)
    };
    routes
        .iter()
        .filter_map(|route| match_http(route, method, suffix).map(|matched| (route, matched)))
        // `max_by` keeps the candidate when the comparison is `Greater`, so the
        // comparator is written in natural order: longer path first, then the
        // lower id.
        .max_by(|(a, _), (b, _)| path_len(a).cmp(&path_len(b)).then_with(|| b.id.cmp(&a.id)))
        .map(|(route, _)| route)
}

/// Whether `candidate` collides with `other` (same path and a shared method).
#[must_use]
pub fn collides(
    candidate: &crate::domain::model::MatchConfig,
    other: &crate::domain::model::MatchConfig,
) -> bool {
    let (Some(a), Some(b)) = (&candidate.http, &other.http) else {
        return false;
    };
    if a.path != b.path {
        return false;
    }
    a.methods
        .iter()
        .any(|method| b.methods.iter().any(|m| m == method))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{GrpcMatch, HttpMatch, MatchConfig, PathSuffixMode, RouteSpec};
    use uuid::Uuid;

    fn route(methods: &[&str], path: &str, mode: PathSuffixMode) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            spec: RouteSpec {
                match_rule: MatchConfig {
                    http: Some(HttpMatch {
                        methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: mode,
                    }),
                    grpc: None,
                },
                ..RouteSpec::default()
            },
        }
    }

    #[test]
    fn appends_the_suffix_to_the_match_path() {
        let chat = route(&["POST"], "/v1", PathSuffixMode::Append);
        let matched = match_http(&chat, "POST", "/v1/chat/completions").expect("match");
        assert_eq!(matched.upstream_path, "/v1/chat/completions");
        let exact = match_http(&chat, "POST", "/v1").expect("match");
        assert_eq!(exact.upstream_path, "/v1");
        assert!(match_http(&chat, "POST", "/v1x").is_none());
    }

    #[test]
    fn disabled_suffix_rejects_any_suffix() {
        let root = route(&["GET"], "/", PathSuffixMode::Disabled);
        assert!(match_http(&root, "GET", "/anything").is_none());
        assert!(match_http(&root, "GET", "").is_some());
    }

    #[test]
    fn root_path_matches_every_suffix_when_appending() {
        let root = route(&["GET", "POST"], "/", PathSuffixMode::Append);
        let matched = match_http(&root, "GET", "/v1/embeddings").expect("match");
        assert_eq!(matched.upstream_path, "/v1/embeddings");
        let bare = match_http(&root, "GET", "").expect("match");
        assert_eq!(bare.upstream_path, "/");
    }

    #[test]
    fn method_allowlist_rejects_other_methods() {
        let chat = route(&["POST"], "/v1", PathSuffixMode::Append);
        assert!(match_http(&chat, "GET", "/v1/chat").is_none());
    }

    #[test]
    fn the_longest_prefix_wins() {
        let root = route(&["GET"], "/", PathSuffixMode::Append);
        let deep = route(&["GET"], "/v1", PathSuffixMode::Append);
        let routes = vec![root, deep];
        assert_eq!(
            select(&routes, "GET", "/v1/models").map(|route| route.id),
            Some(routes[1].id)
        );
        assert_eq!(
            select(&routes, "GET", "/healthz").map(|route| route.id),
            Some(routes[0].id)
        );
    }

    #[test]
    fn grpc_routes_match_service_and_method() {
        let mut route = route(&["POST"], "/", PathSuffixMode::Append);
        route.spec.match_rule.http = None;
        route.spec.match_rule.grpc = Some(GrpcMatch {
            service: "foo.v1.UserService".to_owned(),
            method: "GetUser".to_owned(),
        });
        let matched = match_grpc(&route, "/foo.v1.UserService/GetUser", "POST").expect("match");
        assert_eq!(matched.upstream_path, "/foo.v1.UserService/GetUser");
        assert!(match_grpc(&route, "/foo.v1.UserService/List", "POST").is_none());
    }

    #[test]
    fn collides_detects_overlapping_paths_and_methods() {
        let a = crate::domain::model::MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned(), "POST".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        let b = crate::domain::model::MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["POST".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        let other_path = crate::domain::model::MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["POST".to_owned()],
                path: "/v2".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(collides(&a, &b));
        assert!(!collides(&a, &other_path));
    }

    #[test]
    fn suffixes_are_normalized() {
        assert_eq!(normalize_suffix("v1/chat"), "/v1/chat");
        assert_eq!(normalize_suffix("/v1/chat/"), "/v1/chat");
        assert_eq!(normalize_suffix(""), "");
        assert_eq!(normalize_suffix("/"), "");
    }
}
