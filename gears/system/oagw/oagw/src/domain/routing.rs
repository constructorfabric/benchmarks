//! Route matching: method allowlist + longest path-prefix match.

use crate::domain::model::{MatchRule, PathSuffixMode, Route};

/// Score describing how well a route matches a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MatchScore {
    /// Length of the matched path prefix (longer wins).
    prefix_len: usize,
}

/// Pick the best matching route for `method` + `path`.
///
/// Selection order (DESIGN §3.1 Request Routing):
/// 1. only HTTP match rules participate in the HTTP proxy path
/// 2. method must be present in `match.http.methods`
/// 3. longest matching prefix of `path` wins
///
/// `path` is the suffix carried by the proxy URL (may be empty).
#[must_use]
pub fn best_match<'a>(routes: &'a [Route], method: &http::Method, path: &str) -> Option<&'a Route> {
    let mut best: Option<(MatchScore, &'a Route)> = None;
    for route in routes {
        let MatchRule::Http(http) = &route.r#match else {
            continue;
        };
        if !http
            .methods
            .iter()
            .any(|m| http::Method::from_bytes(m.as_bytes()).is_ok_and(|m| m == method))
        {
            continue;
        }
        let Some(pattern) = normalize_pattern(&http.path) else {
            continue;
        };
        let matched = if path.is_empty() {
            pattern.is_empty()
        } else if pattern.is_empty() {
            true
        } else {
            path == pattern
                || path.starts_with(&format!("{pattern}/"))
                || path.starts_with(pattern)
                    && (pattern.ends_with('/') || http.path_suffix_mode == PathSuffixMode::Append)
        };
        if !matched {
            continue;
        }
        let score = MatchScore {
            prefix_len: pattern.len(),
        };
        if best.as_ref().is_none_or(|(b, _)| score > *b) {
            best = Some((score, route));
        }
    }
    best.map(|(_, route)| route)
}

/// Strip a trailing slash so `/v1/` and `/v1` match identically.
fn normalize_pattern(pattern: &str) -> Option<&str> {
    let p = pattern.strip_suffix('/').unwrap_or(pattern);
    p.strip_prefix('/').or(if pattern == "/" { Some("") } else { None })
}

/// Build the upstream path for a matched route.
///
/// `path_suffix_mode: append` appends the suffix after the configured prefix;
/// `disabled` rejects any non-empty suffix.
pub fn build_target_path(
    pattern: &str,
    mode: crate::domain::model::PathSuffixMode,
    suffix: &str,
) -> Result<String, crate::error::OagwError> {
    match mode {
        crate::domain::model::PathSuffixMode::Append => {
            let base = pattern.trim_end_matches('/');
            if suffix.is_empty() {
                Ok(if base.is_empty() { "/".to_owned() } else { base.to_owned() })
            } else {
                Ok(format!("{}/{}", base, suffix.trim_start_matches('/')))
            }
        }
        crate::domain::model::PathSuffixMode::Disabled => {
            if suffix.is_empty() {
                Ok(pattern.to_owned())
            } else {
                Err(crate::error::OagwError::new(
                    crate::error::ErrorKind::ValidationError,
                    "this route does not accept a path suffix (path_suffix_mode: disabled)",
                )
                .with_ext("path", suffix))
            }
        }
    }
}

/// Validate query parameters against the route allowlist.
///
/// An empty allowlist permits no parameters.
pub fn check_query_allowlist(
    allowlist: &[String],
    query_pairs: &[(&str, &str)],
) -> Result<(), crate::error::OagwError> {
    for (key, _) in query_pairs {
        if !allowlist.iter().any(|a| a == key) {
            return Err(crate::error::OagwError::new(
                crate::error::ErrorKind::ValidationError,
                format!("query parameter '{key}' is not allowed by this route"),
            )
            .with_ext("fields", serde_json::Value::from(["query"])));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{
        GrpcMatch, HttpMatch, PathSuffixMode, PROTOCOL_HTTP, Route, Upstream,
    };
    use uuid::Uuid;

    fn route(path: &str, methods: &[&str], mode: PathSuffixMode) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            tags: vec![],
            r#match: MatchRule::Http(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: mode,
            }),
            plugins: Default::default(),
            rate_limit: None,
            cors: None,
            headers: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn upstream() -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "x".to_owned(),
            enabled: true,
            server: crate::domain::model::ServerConfig { endpoints: vec![] },
            protocol: PROTOCOL_HTTP.to_owned(),
            tags: vec![],
            headers: Default::default(),
            rate_limit: None,
            cors: None,
            auth: None,
            plugins: Default::default(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn longest_prefix_wins() {
        let short = route("/v1", &["GET"], Default::default());
        let long = route("/v1/chat", &["GET"], Default::default());
        let picked_id = long.id;
        let routes = [short, long];
        let picked = best_match(&routes, &http::Method::GET, "v1/chat/completions").unwrap();
        assert_eq!(picked.id, picked_id);
    }

    #[test]
    fn method_must_be_allowed() {
        let r = route("/v1", &["POST"], Default::default());
        let routes = [r];
        assert!(best_match(&routes, &http::Method::GET, "v1/x").is_none());
    }

    #[test]
    fn grpc_routes_do_not_match_http_requests() {
        let mut r = route("/v1", &["GET"], Default::default());
        r.r#match = MatchRule::Grpc(GrpcMatch {
            service: "foo.v1.UserService".to_owned(),
            method: "GetUser".to_owned(),
        });
        let routes = [r];
        assert!(best_match(&routes, &http::Method::GET, "v1/x").is_none());
    }

    #[test]
    fn build_target_path_appends_suffix() {
        let got = build_target_path("/v1/chat", Default::default(), "completions/x").unwrap();
        assert_eq!(got, "/v1/chat/completions/x");
    }

    #[test]
    fn build_target_path_empty_suffix_keeps_base() {
        let got = build_target_path("/v1/chat", Default::default(), "").unwrap();
        assert_eq!(got, "/v1/chat");
    }

    #[test]
    fn disabled_mode_rejects_suffix() {
        assert!(build_target_path("/v1", crate::domain::model::PathSuffixMode::Disabled, "x").is_err());
        assert_eq!(
            build_target_path("/v1", crate::domain::model::PathSuffixMode::Disabled, "").unwrap(),
            "/v1"
        );
    }

    #[test]
    fn query_allowlist_rejects_unknown() {
        assert!(check_query_allowlist(&["model".to_owned()], &[("model", "gpt")]).is_ok());
        assert!(check_query_allowlist(&[], &[("model", "gpt")]).is_err());
    }

    #[test]
    fn upstream_helper_builds() {
        let _ = upstream();
    }
}
