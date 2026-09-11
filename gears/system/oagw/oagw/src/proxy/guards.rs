//! Apply the Inbound Guard Rules (`cpt-cf-oagw-algo-proxy-apply-guard-rules`).

use crate::model::route::{HttpMatch, PathSuffixMode};

/// A guard-rule rejection: always `400 ValidationError`, distinguished only
/// by the caller-facing detail message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardError(pub String);

/// The effective outbound path and query string.
#[derive(Debug, Clone)]
pub(crate) struct GuardedRequest {
    pub outbound_path: String,
    pub outbound_query: Vec<(String, String)>,
}

/// Join the matched `match.http.path` prefix with the unmatched remainder
/// of `path_expr`, handling the degenerate cases explicitly
/// (`inst-proxy-guard-path-degenerate`): an empty remainder yields the
/// prefix exactly, and a root (`/`) prefix appends the whole suffix (since
/// `path_expr` already carries the leading `/` the one-character `/`
/// prefix itself consumed).
fn join_path(prefix: &str, path_expr: &str, matched_prefix_len: usize) -> String {
    if prefix == "/" {
        return path_expr.to_owned();
    }
    let remainder = &path_expr[matched_prefix_len.min(path_expr.len())..];
    if remainder.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}{remainder}")
    }
}

/// `cpt-cf-oagw-algo-proxy-apply-guard-rules`: enforce `path_suffix_mode`
/// and the query allowlist, computing the effective outbound path/query.
// @cpt-algo:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-guard-rules:p1
pub(crate) fn apply_guard_rules(
    http_match: &HttpMatch,
    matched_prefix_len: usize,
    path_suffix: &str,
    path_expr: &str,
    query_pairs: &[(String, String)],
) -> Result<GuardedRequest, GuardError> {
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-suffix-disabled
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return-suffix-disabled
    if http_match.path_suffix_mode == PathSuffixMode::Disabled && !path_suffix.is_empty() {
        return Err(GuardError(
            "a path suffix was supplied but this route disables path_suffix_mode".to_owned(),
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return-suffix-disabled
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-suffix-disabled

    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-suffix-none
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-exact
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-else-append
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-append
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-degenerate
    let outbound_path = if http_match.path_suffix_mode == PathSuffixMode::Disabled {
        http_match.path.clone()
    } else {
        join_path(&http_match.path, path_expr, matched_prefix_len)
    };
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-degenerate
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-append
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-else-append
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-path-exact
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-suffix-none

    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-foreach-query
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-query-unknown
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return-query-unknown
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-query-keep
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-no-silent-drop
    let mut outbound_query = Vec::with_capacity(query_pairs.len());
    for (name, value) in query_pairs {
        if !http_match
            .query_allowlist
            .iter()
            .any(|allowed| allowed == name)
        {
            return Err(GuardError(format!(
                "query parameter '{name}' is not in this route's allowlist"
            )));
        }
        outbound_query.push((name.clone(), value.clone()));
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-no-silent-drop
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-query-keep
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return-query-unknown
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-if-query-unknown
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-foreach-query

    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-method-passthrough
    // @cpt-begin:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return
    Ok(GuardedRequest {
        outbound_path,
        outbound_query,
    })
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-return
    // @cpt-end:cpt-cf-oagw-algo-proxy-apply-guard-rules:p2:inst-proxy-guard-method-passthrough
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn http_match(path: &str, mode: PathSuffixMode, allowlist: Vec<&str>) -> HttpMatch {
        HttpMatch {
            methods: Vec::new(),
            path: path.to_owned(),
            query_allowlist: allowlist.into_iter().map(str::to_owned).collect(),
            path_suffix_mode: mode,
        }
    }

    #[test]
    fn append_mode_joins_prefix_and_remainder_with_one_separator() {
        let m = http_match("/v1/models", PathSuffixMode::Append, vec![]);
        // matched prefix len = "/v1/models".len(), path_expr = "/v1/models/extra"
        let path_expr = "/v1/models/extra";
        let result = apply_guard_rules(&m, "/v1/models".len(), "extra", path_expr, &[]).unwrap();
        assert_eq!(result.outbound_path, "/v1/models/extra");
    }

    #[test]
    fn append_mode_degenerate_empty_remainder_is_exact_prefix() {
        let m = http_match("/v1/models", PathSuffixMode::Append, vec![]);
        let path_expr = "/v1/models";
        let result = apply_guard_rules(&m, "/v1/models".len(), "", path_expr, &[]).unwrap();
        assert_eq!(result.outbound_path, "/v1/models");
    }

    #[test]
    fn append_mode_degenerate_root_prefix_appends_whole_suffix() {
        let m = http_match("/", PathSuffixMode::Append, vec![]);
        let path_expr = "/anything/here";
        let result = apply_guard_rules(&m, 1, "anything/here", path_expr, &[]).unwrap();
        assert_eq!(result.outbound_path, "/anything/here");
    }

    #[test]
    fn disabled_mode_with_suffix_supplied_is_rejected() {
        let m = http_match("/v1", PathSuffixMode::Disabled, vec![]);
        assert!(apply_guard_rules(&m, 3, "extra", "/v1/extra", &[]).is_err());
    }

    #[test]
    fn disabled_mode_without_suffix_uses_exact_path() {
        let m = http_match("/v1", PathSuffixMode::Disabled, vec![]);
        let result = apply_guard_rules(&m, 3, "", "/v1", &[]).unwrap();
        assert_eq!(result.outbound_path, "/v1");
    }

    #[test]
    fn allowlisted_query_param_is_kept() {
        let m = http_match("/v1", PathSuffixMode::Append, vec!["q"]);
        let pairs = vec![("q".to_owned(), "x".to_owned())];
        let result = apply_guard_rules(&m, 3, "", "/v1", &pairs).unwrap();
        assert_eq!(result.outbound_query, pairs);
    }

    #[test]
    fn non_allowlisted_query_param_is_rejected() {
        let m = http_match("/v1", PathSuffixMode::Append, vec!["q"]);
        let pairs = vec![("other".to_owned(), "x".to_owned())];
        assert!(apply_guard_rules(&m, 3, "", "/v1", &pairs).is_err());
    }

    #[test]
    fn empty_allowlist_rejects_any_parameter() {
        let m = http_match("/v1", PathSuffixMode::Append, vec![]);
        let pairs = vec![("q".to_owned(), "x".to_owned())];
        assert!(apply_guard_rules(&m, 3, "", "/v1", &pairs).is_err());
    }
}
