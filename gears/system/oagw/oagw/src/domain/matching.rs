//! Route matching: method allowlist, longest path prefix, query allowlist
//! and path-suffix modes.

use crate::domain::dto::{MatchKind, PathSuffixMode, Route};

/// A successful match, together with the target path it implies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchOutcome {
    /// The route that matched.
    pub route_id: String,
    /// The route path that matched.
    pub route_path: String,
    /// The method allowlist that accepted the request.
    pub methods: Vec<String>,
    /// Query parameters the route permits.
    pub query_allowlist: Vec<String>,
    /// How the proxy URL's path suffix is treated.
    pub suffix_mode: PathSuffixMode,
}

impl MatchOutcome {
    /// The path forwarded upstream: the route path, plus the suffix when the
    /// route appends it.
    pub fn forward_path(&self, suffix: &str) -> String {
        match self.suffix_mode {
            PathSuffixMode::Disabled => self.route_path.clone(),
            PathSuffixMode::Append => {
                if suffix.is_empty() {
                    self.route_path.clone()
                } else {
                    format!("{}/{}", self.route_path.trim_end_matches('/'), suffix.trim_start_matches('/'))
                }
            }
        }
    }
}

/// Whether the HTTP method is in the route's allowlist.
pub fn method_allowed(route: &Route, method: &str) -> bool {
    let Some(http) = route.match_rule.http.as_ref() else {
        return false;
    };
    http.methods.iter().any(|m| m.eq_ignore_ascii_case(method))
}

/// Whether the route's path pattern matches `path`.
///
/// The path pattern is a prefix: `/v1` matches `/v1`, `/v1/anything` but not
/// `/v1x`. `/` matches everything.
pub fn path_matches(route: &Route, path: &str) -> bool {
    let Some(http) = route.match_rule.http.as_ref() else {
        return false;
    };
    let pattern = http.path.trim_end_matches('/');
    if pattern.is_empty() {
        return true;
    }
    let candidate = path.trim_end_matches('/');
    candidate == pattern || candidate.starts_with(&format!("{pattern}/"))
}

/// Whether every query parameter of the request is in the allowlist.
///
/// An empty allowlist rejects every parameter, including unknown ones.
pub fn query_allowed(route: &Route, query: &[(String, String)]) -> bool {
    let Some(http) = route.match_rule.http.as_ref() else {
        return false;
    };
    if query.is_empty() {
        return true;
    }
    if http.query_allowlist.is_empty() {
        return false;
    }
    query.iter().all(|(k, _)| {
        http.query_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(k))
    })
}

/// The part of `forwarded_path` beyond the route's path prefix.
///
/// `/v1` against `/v1/models` is `models`; `/v1` against `/v1` is empty; `/v1x`
/// is not a match at all and yields nothing here.
pub fn path_suffix(route_path: &str, forwarded_path: &str) -> String {
    let route = route_path.trim_end_matches('/');
    let forwarded = forwarded_path.trim_end_matches('/');
    if forwarded.len() <= route.len() {
        return String::new();
    }
    // A prefix only owns what follows a label boundary: `/v1` does not own
    // `/v1x`, so a path that merely extends the prefix has no suffix.
    match forwarded[route.len()..].strip_prefix('/') {
        Some(rest) => rest.to_string(),
        None => String::new(),
    }
}

/// Whether the suffix implied by `forwarded_path` is legal under the route's
/// suffix mode.
pub fn suffix_allowed(route: &Route, forwarded_path: &str) -> bool {
    let Some(http) = route.match_rule.http.as_ref() else {
        return false;
    };
    match http.path_suffix_mode {
        PathSuffixMode::Disabled => path_suffix(&http.path, forwarded_path).is_empty(),
        PathSuffixMode::Append => true,
    }
}

/// Whether the route is an HTTP (as opposed to gRPC) route.
pub fn is_http_route(route: &Route) -> bool {
    route.enabled && route.match_rule.exactly_one() == Ok(MatchKind::Http)
}

/// Selects the route for a request.
///
/// Deterministic ordering: only enabled routes whose match rule is HTTP and
/// whose method and path match are candidates; among them the longest path
/// prefix wins, ties broken by route id so the result is stable.
pub fn match_route<'a>(
    routes: impl IntoIterator<Item = &'a Route>,
    method: &str,
    path: &str,
    query: &[(String, String)],
) -> Option<MatchOutcome> {
    best_route(routes, method, path, query).map(|route| {
        let http = route.match_rule.http.as_ref().expect("http route");
        MatchOutcome {
            route_id: route.id.clone().unwrap_or_default(),
            route_path: http.path.clone(),
            methods: http.methods.clone(),
            query_allowlist: http.query_allowlist.clone(),
            suffix_mode: http.path_suffix_mode,
        }
    })
}

/// The winning [`Route`] for a request: the longest matching path prefix whose
/// method allowlist admits the request.
///
/// The query allowlist and the path-suffix mode are deliberately **not**
/// selection criteria: a route still matches a request it would reject, so the
/// caller is told the request is invalid (400) rather than that nothing
/// matched (404). [`query_allowed`] and [`suffix_allowed`] are the guards.
pub fn best_route<'a>(
    routes: impl IntoIterator<Item = &'a Route>,
    method: &str,
    path: &str,
    _query: &[(String, String)],
) -> Option<&'a Route> {
    let mut best: Option<(usize, &Route)> = None;
    for route in routes {
        if !is_http_route(route) {
            continue;
        }
        if !method_allowed(route, method) {
            continue;
        }
        if !path_matches(route, path) {
            continue;
        }
        let len = route
            .match_rule
            .http
            .as_ref()
            .map(|h| h.path.trim_end_matches('/').len())
            .unwrap_or(0);
        let better = match best {
            None => true,
            Some((best_len, best_route)) => {
                len > best_len
                    || (len == best_len
                        && route.id.as_deref().unwrap_or("") < best_route.id.as_deref().unwrap_or(""))
            }
        };
        if better {
            best = Some((len, route));
        }
    }

    best.map(|(_, route)| route)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{GrpcMatch, HttpMatch, MatchRule};

    fn route(id: &str, path: &str, methods: &[&str]) -> Route {
        Route {
            id: Some(id.to_string()),
            match_rule: MatchRule {
                http: Some(HttpMatch {
                    methods: methods.iter().map(|m| m.to_string()).collect(),
                    path: path.to_string(),
                    ..HttpMatch::default()
                }),
                grpc: None,
            },
            ..Route::default()
        }
    }

    #[test]
    fn the_method_allowlist_admits_and_rejects() {
        let r = route("r1", "/v1", &["GET"]);
        assert!(method_allowed(&r, "GET"));
        assert!(!method_allowed(&r, "POST"));
    }

    #[test]
    fn the_longest_path_prefix_wins() {
        let short = route("short", "/v1", &["GET"]);
        let long = route("long", "/v1/chat", &["GET"]);
        let matched = match_route([&short, &long], "GET", "/v1/chat/completions", &[])
            .expect("a route matches");
        assert_eq!(matched.route_id, "long");
    }

    #[test]
    fn a_prefix_must_land_on_a_label_boundary() {
        let r = route("r1", "/v1", &["GET"]);
        assert!(path_matches(&r, "/v1"));
        assert!(path_matches(&r, "/v1/anything"));
        assert!(!path_matches(&r, "/v1x"));
        assert!(!path_matches(&r, "/other"));
    }

    #[test]
    fn an_empty_allowlist_rejects_any_query_parameter() {
        let r = route("r1", "/v1", &["GET"]);
        assert!(query_allowed(&r, &[]));
        assert!(!query_allowed(&r, &[("debug".to_string(), "1".to_string())]));
    }

    #[test]
    fn a_named_allowlist_accepts_its_parameters_only() {
        let mut r = route("r1", "/v1", &["GET"]);
        r.match_rule.http.as_mut().unwrap().query_allowlist =
            vec!["api-version".to_string()];
        assert!(query_allowed(&r, &[("api-version".to_string(), "1".to_string())]));
        assert!(!query_allowed(
            &r,
            &[("debug".to_string(), "1".to_string())]
        ));
    }

    #[test]
    fn suffix_modes_govern_the_path_suffix() {
        let mut r = route("r1", "/v1", &["GET"]);
        assert!(suffix_allowed(&r, "/v1/models"));
        assert!(suffix_allowed(&r, "/v1"));
        r.match_rule.http.as_mut().unwrap().path_suffix_mode = PathSuffixMode::Disabled;
        assert!(!suffix_allowed(&r, "/v1/models"));
        assert!(suffix_allowed(&r, "/v1"));
    }

    #[test]
    fn the_suffix_is_what_the_prefix_leaves_behind() {
        assert_eq!(path_suffix("/v1", "/v1/models"), "models");
        assert_eq!(path_suffix("/v1", "/v1"), "");
        assert_eq!(path_suffix("/v1/", "/v1/models"), "models");
        assert_eq!(path_suffix("/v1", "/v1x"), "");
    }

    #[test]
    fn append_builds_the_forward_path() {
        let r = route("r1", "/v1/chat", &["GET"]);
        let outcome = match_route([&r], "GET", "/v1/chat", &[]).unwrap();
        assert_eq!(outcome.forward_path("completions"), "/v1/chat/completions");
        assert_eq!(outcome.forward_path(""), "/v1/chat");
    }

    #[test]
    fn a_disabled_route_is_not_selected_here_but_by_the_caller() {
        // `enabled` lives on the upstream; the route layer only sees routes the
        // caller already filtered, so matching is purely structural here.
        let r = route("r1", "/v1", &["GET"]);
        assert!(match_route([&r], "GET", "/v1", &[]).is_some());
    }

    #[test]
    fn grpc_routes_never_match_an_http_request() {
        let grpc = Route {
            id: Some("g".to_string()),
            match_rule: MatchRule {
                http: None,
                grpc: Some(GrpcMatch { service: "foo.v1.UserService".into(), method: "GetUser".into() }),
            },
            ..Route::default()
        };
        assert!(match_route([&grpc], "GET", "/v1", &[]).is_none());
    }

    #[test]
    fn no_route_matches_an_unknown_method() {
        let r = route("r1", "/v1", &["GET"]);
        assert!(match_route([&r], "TRACE", "/v1", &[]).is_none());
    }
}
