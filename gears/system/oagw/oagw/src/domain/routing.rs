//! Alias and route resolution (DESIGN §3.1 "Alias Resolution", §3.2 "Request
//! Routing", ADR 0001 "Request Routing").
//!
//! Both functions here are pure: they receive the candidate rows already
//! fetched from the store and return the winner. Keeping them in the domain
//! lets the store stay a dumb key/value seam and lets the shadowing /
//! longest-prefix rules be unit tested without a storage layer.
//!
//! **Chain direction convention.** [`resolve_alias`] takes the tenant chain
//! ordered *leaf first* — the direction DESIGN describes ("walks tenant
//! hierarchy from descendant to root") — and returns the ancestors re-ordered
//! *root first* so that [`crate::domain::merge`] can consume
//! `[ancestors…, selected]` as a root → leaf configuration chain.

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::models::{HttpMethod, PathSuffixMode, Route, Upstream};

/// Outcome of resolving `{METHOD} /proxy/{alias}/…` across a tenant chain.
#[derive(Debug)]
pub struct ResolvedAlias<'a> {
    /// Closest (leaf-most) upstream bound to the alias.
    pub selected: &'a Upstream,
    /// Upstreams the selected one shadows, ordered **root → leaf**.
    ///
    /// These carry the `sharing: enforce` constraints that shadowing must not
    /// bypass (DESIGN: "Enforced ancestor constraints are never bypassed by
    /// shadowing"). Disabled entries stay in this list for that reason.
    pub ancestors: Vec<&'a Upstream>,
}

/// Ranks a tenant id by its distance from the leaf of the chain (0 = leaf).
fn chain_rank(chain: &[Uuid], tenant_id: Uuid) -> Option<usize> {
    chain.iter().position(|tenant| *tenant == tenant_id)
}

/// Resolves an alias across a tenant chain ordered **leaf → root**.
///
/// Shadowing is decided by the *closest* (leaf-most) upstream bound to
/// `alias`, regardless of its `enabled` flag: a disabled selected upstream is
/// reported to the caller so it can answer `503 LinkUnavailable` (PRD §5.2 —
/// "a disabled upstream MUST cause all proxy requests to be rejected with
/// 503"). Falling back to an ancestor's upstream would silently route traffic
/// to a different owner's service, which is not a permitted recovery.
///
/// Review evidence (privilege boundary — cross-tenant routing):
/// * Guardrail: DESIGN "Shadowing Behavior" + PRD §5.2 enable/disable
///   semantics.
/// * Rationale: only upstreams whose `tenant_id` appears in the resolved
///   chain are considered, so a caller can never reach another tenant's
///   upstream by guessing an alias.
/// * Validation performed: `resolve_alias_*` unit tests cover shadowing, the
///   disabled-selected case and the "unknown alias" case.
#[must_use]
pub fn resolve_alias<'a>(
    chain: &[Uuid],
    upstreams: &'a [Upstream],
    alias: &str,
) -> Option<ResolvedAlias<'a>> {
    let wanted = crate::domain::alias::normalize_alias(alias);
    if wanted.is_empty() {
        return None;
    }
    let mut ranked: Vec<(usize, &Upstream)> = upstreams
        .iter()
        .filter(|upstream| upstream.normalized_alias() == wanted)
        .filter_map(|upstream| {
            chain_rank(chain, upstream.tenant_id).map(|rank| (rank, upstream))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);

    let (selected, rest) = ranked.split_first()?;
    // Ancestors are reported root → leaf so the caller can feed them straight
    // into the configuration merge.
    let mut ancestors: Vec<&Upstream> = rest.iter().map(|(_, upstream)| *upstream).collect();
    ancestors.reverse();
    Some(ResolvedAlias {
        selected: selected.1,
        ancestors,
    })
}

/// A route matched against an incoming request.
#[derive(Debug)]
pub struct RouteMatch<'a> {
    /// Winning route.
    pub route: &'a Route,
    /// Part of the request path left after removing `match.http.path`.
    pub suffix: String,
}

/// Whether `path` is covered by the route prefix `pattern`.
///
/// The prefix must end on a path-segment boundary: `/v1/chat` covers
/// `/v1/chat` and `/v1/chat/completions` but not `/v1/chatbot`.
#[must_use]
pub fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() || prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }
    if path == prefix {
        return true;
    }
    if path.len() > prefix.len()
        && path.starts_with(prefix)
        && path.as_bytes()[prefix.len()] == b'/'
    {
        return true;
    }
    // A trailing slash on the prefix also covers the bare prefix
    // (`/v1/` matches `/v1` and `/v1/x`).
    if prefix.ends_with('/') {
        let trimmed = prefix.trim_end_matches('/');
        return path == trimmed || path.starts_with(trimmed);
    }
    false
}

/// Whether the route's match rules admit `(method, path)`.
///
/// `method` is matched case-insensitively against `match.http.methods`; a
/// route with an empty method list admits everything.
#[must_use]
pub fn route_matches(route: &Route, method: &str, path: &str) -> bool {
    if !route.enabled {
        return false;
    }
    let Some(http) = route.match_config.http.as_ref() else {
        return false;
    };
    if !http.methods.is_empty()
        && !http
            .methods
            .iter()
            .any(|allowed| http_method_label(*allowed).eq_ignore_ascii_case(method))
    {
        return false;
    }
    path_matches_prefix(path, &http.path)
}

/// Resolves the winning route for `(method, path)`.
///
/// Selection order (DESIGN §3.2 "Request Routing"): disabled routes are
/// excluded, the longest `match.http.path` prefix wins, ties are broken by the
/// higher `priority` value and finally by creation order.
#[must_use]
pub fn resolve_route<'a>(
    candidates: &'a [Route],
    method: &str,
    path: &str,
) -> Option<RouteMatch<'a>> {
    let mut best: Option<(usize, i32, usize, &'a Route)> = None;
    for (index, route) in candidates.iter().enumerate() {
        if !route_matches(route, method, path) {
            continue;
        }
        let prefix_len = route
            .match_config
            .http
            .as_ref()
            .map_or(0, |http| http.path.len());
        let better = match best {
            None => true,
            Some((best_len, best_priority, _, _)) => {
                prefix_len > best_len
                    || (prefix_len == best_len && route.priority > best_priority)
            }
        };
        if better {
            best = Some((prefix_len, route.priority, index, route));
        }
    }
    let (_, _, _, route) = best?;
    let prefix = route
        .match_config
        .http
        .as_ref()
        .map_or(String::new(), |http| http.path.clone());
    let suffix = path
        .strip_prefix(&prefix)
        .unwrap_or_default()
        .to_owned();
    Some(RouteMatch { route, suffix })
}

/// Enforces the route-level request guards (DESIGN §3.2 "Guard Rules").
///
/// * Query parameters outside `match.http.query_allowlist` are rejected
///   (an empty allowlist admits no query parameters).
/// * A path suffix is rejected when `path_suffix_mode` is `disabled`.
///
/// # Errors
///
/// Returns a [`DomainError::RouteError`] describing the first violated rule.
pub fn guard_route_match(route: &Route, query_pairs: &[(&str, &str)], suffix: &str) -> Result<(), DomainError> {
    let http = route
        .match_config
        .http
        .as_ref()
        .ok_or_else(|| DomainError::RouteError {
            detail: "route has no HTTP match configuration".to_owned(),
            invalid_value: None,
        })?;

    if let Some((name, _)) = query_pairs
        .iter()
        .find(|(name, _)| !http.query_allowlist.iter().any(|a| a == name))
    {
        return Err(DomainError::RouteError {
            detail: format!("query parameter '{name}' is not allowed by the route match rules"),
            invalid_value: Some((*name).to_owned()),
        });
    }

    if route
        .match_config
        .http
        .as_ref()
        .is_some_and(|http| http.path_suffix_mode == PathSuffixMode::Disabled)
        && !suffix.is_empty()
    {
        return Err(DomainError::RouteError {
            detail: "path suffix is not allowed by this route (path_suffix_mode: disabled)"
                .to_owned(),
            invalid_value: Some(suffix.to_owned()),
        });
    }
    Ok(())
}

/// Lowercase wire label of a configured [`HttpMethod`].
#[must_use]
pub fn http_method_label(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
    }
}

/// Normalises a method for metric / matching purposes, per the `OTel` semantic conventions.
///
/// Unknown verbs are reported as `_OTHER`.
#[must_use]
pub fn normalize_method(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "TRACE" => "TRACE",
        "CONNECT" => "CONNECT",
        _ => "_OTHER",
    }
}

#[cfg(test)]
#[path = "routing_tests.rs"]
mod tests;
