//! Route selection: method allowlist + longest path prefix, highest priority,
//! insertion position last (`DESIGN.md` §"Route").

use std::sync::Arc;

use crate::domain::model::{HttpMatch, PathSuffixMode, Route};
use crate::infra::store::normalize_pattern;

/// Result of matching a request path against a route.
#[derive(Debug, Clone)]
pub struct SelectedRoute {
    /// The matching route.
    pub route: Arc<Route>,
    /// Path forwarded upstream (route path for `disabled` suffix mode, the
    /// client path as the caller spelled it for `append`).
    pub upstream_path: String,
    /// Normalized client path the match was made against.
    pub client_path: String,
}

/// Normalize a client path so comparisons are stable: always starts with `/`
/// and never ends with `/`.
///
/// The result is for matching only; [`forward_path`] keeps the caller's
/// spelling for the path that travels upstream.
#[must_use]
pub fn normalize_client_path(path: &str) -> String {
    normalize_pattern(&forward_path(path))
}

/// Slash-prefix `path` without touching anything else about it.
#[must_use]
pub fn forward_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Whether `method` is in `route`'s `match.http.methods` allowlist.
///
/// `HEAD` is folded onto `GET` (`HttpMethod::from_http`): the schema has no
/// `HEAD` variant, so a route that serves `GET` also serves its `HEAD`
/// equivalent, and the relay still forwards the request as `HEAD`. A gRPC route
/// has no HTTP allowlist and allows no HTTP method.
#[must_use]
pub fn method_allowed(route: &Route, method: &http::Method) -> bool {
    let Some(requested) = crate::domain::model::HttpMethod::from_http(method) else {
        return false;
    };
    route
        .match_rule
        .http()
        .is_some_and(|http| http.methods.contains(&requested))
}

/// Whether `route`'s HTTP path pattern matches `client_path`, returning the
/// upstream path the route would produce for it.
///
/// The comparison runs against [`normalize_client_path`] so a pattern matches
/// whatever spelling the client used, while the returned path keeps the
/// client's own spelling (`ADR 0001`): matching is case-insensitive, relaying
/// is not.
#[must_use]
pub fn match_route_path(route: &Route, client_path: &str) -> Option<String> {
    let http = route.match_rule.http()?;
    let pattern = normalize_pattern(&http.path);
    let normalized = normalize_client_path(client_path);
    path_matches(&pattern, &normalized, http)
        .then(|| upstream_path(pattern, client_path, http.path_suffix_mode))
}

/// Whether `pattern` matches the normalized client path under `http`'s suffix
/// mode.
fn path_matches(pattern: &str, normalized: &str, http: &HttpMatch) -> bool {
    pattern == "/"
        || normalized == pattern
        || (http.path_suffix_mode == PathSuffixMode::Append
            && normalized.starts_with(pattern)
            && normalized.as_bytes().get(pattern.len()) == Some(&b'/'))
}

/// Whether `route` matches `method` and `client_path`, returning the upstream
/// path it would produce.
///
/// The comparison runs against [`normalize_client_path`] so a pattern matches
/// whatever spelling the client used, while the returned path keeps the
/// client's own spelling (`ADR 0001`): matching is case-insensitive, relaying
/// is not.
#[must_use]
pub fn match_route(route: &Route, method: &http::Method, client_path: &str) -> Option<String> {
    if !method_allowed(route, method) {
        return None;
    }
    match_route_path(route, client_path)
}

fn upstream_path(pattern: String, client_path: &str, mode: PathSuffixMode) -> String {
    if mode == PathSuffixMode::Disabled {
        return pattern;
    }
    forward_path(client_path)
}

/// The outcome of route selection for one request.
///
/// The distinction matters on the data plane: a path no route claims is
/// relayed against the upstream as a fall-through, while a path a route claims
/// with a method that route refuses is a rejection (`DESIGN.md` §"Guard
/// Rules": the method must be in `match.http.methods`).
#[derive(Debug, Clone)]
pub enum Selection {
    /// A route matched both the method and the path.
    Matched(SelectedRoute),
    /// The path matched at least one enabled HTTP route but none of those
    /// routes allowed the request method. The payload is the path pattern that
    /// matched, so the caller can name it in the rejection.
    MethodRejected(String),
    /// No enabled route's path pattern matched the request path.
    Unmatched,
}

/// Pick the route for a request.
///
/// The order is the one `DESIGN.md` §"Route" specifies: the longest matching
/// path pattern wins, and among patterns of equal length the route with the
/// highest `priority` wins. Insertion order — the store's monotonic `position`
/// — is only the final tie-breaker, applied when pattern length and priority
/// are equal, so a request never depends on which of two identical
/// declarations the store happened to see first. The store refuses the one
/// configuration that would make that tie-break observable: two enabled routes
/// of one upstream sharing a `(path, priority, method)` claim.
///
/// A route whose pattern matches the path but whose method allowlist refuses
/// the request is not silently skipped: when no other route fully matches,
/// [`Selection::MethodRejected`] reports the strongest path match so the
/// caller can reject the request instead of relaying it. A path no route
/// claims at all stays a fall-through, which is how an upstream with no routes
/// (or routes for other paths) keeps proxying.
#[must_use]
pub fn select_route(routes: &[Arc<Route>], method: &http::Method, client_path: &str) -> Selection {
    let normalized = normalize_client_path(client_path);
    let mut best: Option<Candidate> = None;
    let mut rejected: Option<(usize, String)> = None;
    for route in routes {
        if !route.enabled {
            continue;
        }
        let Some(http) = route.match_rule.http() else {
            continue;
        };
        let pattern = normalize_pattern(&http.path);
        let pattern_len = pattern.len();
        if !path_matches(&pattern, &normalized, http) {
            continue;
        }
        if method_allowed(route, method) {
            let candidate = Candidate {
                pattern_len,
                priority: route.priority,
                position: usize::try_from(route.position).unwrap_or(usize::MAX),
                route: Arc::clone(route),
                upstream_path: upstream_path(pattern, client_path, http.path_suffix_mode),
            };
            if best.as_ref().is_none_or(|best| candidate.beats(best)) {
                best = Some(candidate);
            }
        } else if rejected
            .as_ref()
            .is_none_or(|(best_len, _)| pattern_len > *best_len)
        {
            rejected = Some((pattern_len, pattern));
        }
    }
    if let Some(candidate) = best {
        return Selection::Matched(SelectedRoute {
            route: candidate.route,
            upstream_path: candidate.upstream_path,
            client_path: normalized,
        });
    }
    match rejected {
        Some((_, pattern)) => Selection::MethodRejected(pattern),
        None => Selection::Unmatched,
    }
}

/// One route that matched both the path and the method, ranked by the
/// selection order: longest pattern first, then highest priority, then
/// insertion position.
#[derive(Debug)]
struct Candidate {
    /// Length of the normalized pattern that matched.
    pattern_len: usize,
    /// Explicit `Route::priority`.
    priority: i64,
    /// Store insertion index: the final tie-breaker.
    position: usize,
    /// The matching route.
    route: Arc<Route>,
    /// Path forwarded upstream.
    upstream_path: String,
}

impl Candidate {
    /// Whether `self` outranks `other` under the selection order.
    fn beats(&self, other: &Candidate) -> bool {
        self.pattern_len
            .cmp(&other.pattern_len)
            .then(self.priority.cmp(&other.priority))
            .then(other.position.cmp(&self.position))
            == std::cmp::Ordering::Greater
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use crate::domain::model::{HttpMatch, HttpMethod, RouteMatch};

    use super::*;

    /// The selected route, asserting the selection matched.
    fn matched(selection: Selection) -> SelectedRoute {
        match selection {
            Selection::Matched(selected) => selected,
            other => panic!("expected a matched route, got {other:?}"),
        }
    }

    /// The path pattern of the reported method refusal, asserting there is one.
    fn rejected(selection: Selection) -> String {
        match selection {
            Selection::MethodRejected(pattern) => pattern,
            other => panic!("expected a method rejection, got {other:?}"),
        }
    }

    fn route(path: &str, methods: &[HttpMethod], position: u64) -> Route {
        route_with_priority(path, methods, position, 0)
    }

    /// A route matching `path`, inserted at `position`, with an explicit
    /// selection `priority`.
    fn route_with_priority(
        path: &str,
        methods: &[HttpMethod],
        position: u64,
        priority: i64,
    ) -> Route {
        Route {
            match_rule: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::default(),
                }),
                grpc: None,
            },
            priority,
            position,
            ..Route::default()
        }
    }

    #[test]
    fn exact_match_wins_over_prefix() {
        let routes = vec![
            Arc::new(route("/v1", &[HttpMethod::Post], 0)),
            Arc::new(route("/v1/chat", &[HttpMethod::Post], 1)),
        ];
        let picked = matched(select_route(&routes, &http::Method::POST, "/v1/chat"));
        assert_eq!(
            picked.route.match_rule.http().expect("http").path,
            "/v1/chat",
            "the longest pattern wins, not the earliest insertion"
        );
        assert_eq!(picked.upstream_path, "/v1/chat");
    }

    #[test]
    fn equal_patterns_rank_by_priority_before_insertion_order() {
        // Two routes on the same pattern: the store would only admit this pair
        // because their priorities differ, so priority decides.
        let routes = vec![
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 0, 1)),
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 1, 9)),
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 2, 9)),
        ];
        let picked = matched(select_route(&routes, &http::Method::POST, "/v1/chat"));
        assert_eq!(picked.route.priority, 9, "the highest priority wins");
        assert_eq!(
            picked.route.position, 1,
            "the earliest insertion breaks the priority tie"
        );
        // A negative priority loses to the default 0.
        let routes = vec![
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 0, -5)),
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 1, 0)),
        ];
        let picked = matched(select_route(&routes, &http::Method::POST, "/v1"));
        assert_eq!(picked.route.priority, 0);
    }

    #[test]
    fn priority_never_outranks_a_longer_pattern() {
        let routes = vec![
            Arc::new(route_with_priority("/v1", &[HttpMethod::Post], 0, 100)),
            Arc::new(route_with_priority("/v1/chat", &[HttpMethod::Post], 1, 1)),
        ];
        let picked = matched(select_route(
            &routes,
            &http::Method::POST,
            "/v1/chat/completions",
        ));
        assert_eq!(
            picked.route.match_rule.http().expect("http").path,
            "/v1/chat",
            "pattern length is ranked before priority"
        );
    }

    #[test]
    fn prefix_requires_slash_boundary() {
        let routes = vec![Arc::new(route("/v1", &[HttpMethod::Post], 0))];
        assert!(
            matches!(
                select_route(&routes, &http::Method::POST, "/v10"),
                Selection::Unmatched
            ),
            "/v10 matches no pattern, so nothing is refused"
        );
        assert!(matches!(
            select_route(&routes, &http::Method::POST, "/v10/chat"),
            Selection::Unmatched
        ));
        let picked = matched(select_route(&routes, &http::Method::POST, "/v1/chat"));
        assert_eq!(picked.upstream_path, "/v1/chat");
    }

    #[test]
    fn root_pattern_matches_everything() {
        let routes = vec![Arc::new(route("/", &[HttpMethod::Get], 0))];
        let picked = matched(select_route(&routes, &http::Method::GET, "/anything/here"));
        assert_eq!(picked.upstream_path, "/anything/here");
    }

    #[test]
    fn method_must_match() {
        let routes = vec![Arc::new(route("/v1", &[HttpMethod::Post], 0))];
        assert_eq!(
            rejected(select_route(&routes, &http::Method::GET, "/v1")),
            "/v1",
            "the path is claimed but the method is not allowed"
        );
    }

    #[test]
    fn a_method_refused_by_one_route_is_overridden_by_one_that_allows_it() {
        // The longer pattern refuses `GET`, the shorter one allows it: the
        // request still matches, so it is not reported as a refusal.
        let routes = vec![
            Arc::new(route("/v1", &[HttpMethod::Get], 0)),
            Arc::new(route("/v1/chat", &[HttpMethod::Post], 1)),
        ];
        let picked = matched(select_route(
            &routes,
            &http::Method::GET,
            "/v1/chat/completions",
        ));
        assert_eq!(
            picked.route.match_rule.http().expect("http").path,
            "/v1",
            "the shorter pattern that allows the method is the one served"
        );
        assert_eq!(picked.upstream_path, "/v1/chat/completions");
        assert_eq!(
            rejected(select_route(
                &routes,
                &http::Method::DELETE,
                "/v1/chat/completions"
            )),
            "/v1/chat",
            "the strongest path match is the one reported"
        );
    }

    #[test]
    fn path_mismatch_is_not_reported_as_a_method_rejection() {
        let routes = vec![Arc::new(route("/v1", &[HttpMethod::Post], 0))];
        assert!(
            matches!(
                select_route(&routes, &http::Method::GET, "/other"),
                Selection::Unmatched
            ),
            "a path no route claims falls through"
        );
    }

    #[test]
    fn client_path_is_normalized() {
        assert_eq!(normalize_client_path(""), "/");
        assert_eq!(normalize_client_path("v1/chat/"), "/v1/chat");
        assert_eq!(normalize_client_path("/V1/CHAT/"), "/v1/chat");
    }

    #[test]
    fn forwarded_path_keeps_the_client_spelling() {
        assert_eq!(forward_path(""), "/");
        assert_eq!(forward_path("v1/chat"), "/v1/chat");
        assert_eq!(forward_path("/V1/Chat/"), "/V1/Chat/");
    }

    #[test]
    fn matching_normalizes_but_relaying_keeps_the_spelling() {
        let routes = vec![Arc::new(route("/v1", &[HttpMethod::Get], 0))];
        let picked = matched(select_route(&routes, &http::Method::GET, "V1/Chat/"));
        assert_eq!(picked.client_path, "/v1/chat", "the match is normalized");
        assert_eq!(
            picked.upstream_path, "/V1/Chat/",
            "the forwarded path is spelled as the client sent it"
        );
    }

    #[test]
    fn disabled_routes_are_skipped() {
        let mut disabled = route("/v1", &[HttpMethod::Get], 0);
        disabled.enabled = false;
        let routes = vec![Arc::new(disabled)];
        assert!(
            matches!(
                select_route(&routes, &http::Method::GET, "/v1"),
                Selection::Unmatched
            ),
            "a disabled route claims nothing, so the request falls through"
        );
    }
}
