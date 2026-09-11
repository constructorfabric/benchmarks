//! Route matching over the resolved candidate set.
//!
//! Realizes `cpt-cf-oagw-algo-route-match`: the method allowlist, the longest
//! path prefix, the `priority` tie-break, and the `path_suffix_mode` decision.
//! The chain-level selection of which routes are candidates at all is
//! `cpt-cf-oagw-feature-hierarchical-config`'s, and this module only evaluates
//! the candidate set that feature produced — the split §1.5 of the FEATURE
//! records, which keeps one route selection from having two owners.

use uuid::Uuid;

use crate::domain::effective::{EffectiveCors, EffectivePluginChain, EffectiveRateLimit};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::{MatchedRoute, ResolvedUpstream, RouteCandidate};
use crate::domain::route::PathSuffixMode;
use crate::domain::upstream::SharingMode;

/// The merged families of the route layer a match carries.
pub type MergedRouteFamilies = (
    Uuid,
    Option<EffectiveRateLimit>,
    Option<EffectivePluginChain>,
    Option<EffectiveCors>,
);

/// The outcome of one match, which the caller maps to an answer.
#[derive(Debug, Clone)]
pub enum MatchOutcome {
    /// A route matched and the outbound path was built.
    Matched(Box<MatchedRoute>),
    /// No candidate matched the method or the path.
    NoMatch,
    /// The path suffix is supplied to a route whose mode rejects it.
    SuffixRejected,
}

// @cpt-dod:cpt-cf-oagw-dod-route-matching:p1

/// Selects the `MatchedRoute` from the candidate set of the resolved upstream.
///
/// The `resolved` carries the candidates; `method` and `path` are the request's,
/// where `path` is the request path the route paths address, and `suffix` is
/// the path suffix as supplied, which is `None` when the request addressed the
/// alias alone and only decides the `disabled` rejection. The tail the outbound
/// path appends is the request path's own remainder beyond the selected route's
/// path, which the matcher reads after it has selected.
#[must_use]
pub fn match_route(
    resolved: &ResolvedUpstream,
    merged_route: Option<&MergedRouteFamilies>,
    method: &str,
    path: &str,
    suffix: Option<&str>,
) -> MatchOutcome {
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-method
    // The method allowlist is the first filter: a route that does not declare
    // the request method is never a candidate, whatever its path.
    let by_method: Vec<&RouteCandidate> = resolved
        .route_candidates
        .iter()
        .filter(|candidate| enabled(candidate))
        .filter(|candidate| {
            candidate
                .route
                .match_config
                .http
                .as_ref()
                .is_some_and(|http| http.methods.iter().any(|declared| declared == method))
        })
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-method

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-method-if
    if by_method.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-method-return
        return MatchOutcome::NoMatch;
        // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-method-return
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-method-if

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-method-else
    // The ELSE of the method filter: the candidates that declared the request
    // method go on to the path filter, which is the only other filter a
    // candidate is subject to.
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-method-else

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-prefix
    // The longest configured path that addresses the request path wins.
    let by_prefix: Vec<&&RouteCandidate> = by_method
        .iter()
        .filter(|candidate| {
            candidate
                .route
                .match_config
                .http
                .as_ref()
                .is_some_and(|http| path_matches(&http.path, path))
        })
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-prefix

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-noprefix-if
    if by_prefix.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-noprefix-return
        return MatchOutcome::NoMatch;
        // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-noprefix-return
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-noprefix-if

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-tie-if
    // The longest prefix wins; of the candidates sharing it, the smallest
    // `priority` value wins, and a route that declares none is the least
    // specific of its prefix group.
    let longest = longest_prefix(&by_prefix);
    let finalists: Vec<&&RouteCandidate> = by_prefix
        .iter()
        .filter(|candidate| {
            candidate
                .route
                .match_config
                .http
                .as_ref()
                .is_some_and(|http| http.path.len() == longest)
        })
        .copied()
        .collect();
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-tie
    // The tie-break of the prefix group: the smallest declared `priority`
    // value wins, and `None` sorts after every declared value.
    let selected = finalists
        .into_iter()
        .min_by_key(|candidate| candidate.route.priority.unwrap_or(i64::MAX))
        .copied();
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-tie
    let Some(selected) = selected else {
        return MatchOutcome::NoMatch;
    };
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-tie-if

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none-else
    // The ELSE of the disabled rejection: the selected route admits the suffix
    // as presented, and the outbound path is decided by the mode.
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none-else


    let Some(http) = selected.route.match_config.http.as_ref() else {
        return MatchOutcome::NoMatch;
    };

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-read
    // The shipped-schema default is `append`, which is what a route that
    // declares no mode gets.
    let mode = selected
        .route
        .match_config
        .http
        .as_ref()
        .and_then(|http| http.path_suffix_mode)
        .unwrap_or(PathSuffixMode::Append);
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-read

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-disabled-if
    if mode == PathSuffixMode::Disabled && suffix.is_some() {
        // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-disabled-return
        return MatchOutcome::SuffixRejected;
        // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-disabled-return
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-disabled-if

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-append-else
    // The ELSE IF of the suffix decision: the mode is `append` and a path
    // suffix was supplied, so the outbound path is the route's own with the
    // tail appended.
    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-append
    // The tail the request carried beyond the matched path: the part of the
    // request path that `append` puts on the outbound path. A request that
    // addressed the route path alone carries none, and a remainder of only
    // separators appends nothing, because `append_suffix` trims them.
    let tail = if path.len() > http.path.len() {
        &path[http.path.len()..]
    } else {
        ""
    };
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-append
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-append-else

    let outbound_path = match (mode, suffix) {
        (PathSuffixMode::Append, Some(_)) => append_suffix(&http.path, tail),
        // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none-else
        // The ELSE of the suffix decision: no suffix was supplied, or the mode
        // forwards none, so the outbound path is the route's own.
        // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none
        _ => http.path.clone(),
        // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none
        // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-suffix-none-else
    };

    // @cpt-begin:cpt-cf-oagw-algo-route-match:p1:inst-match-return
    let (rate_limit, plugins, cors) = merged_families(selected, merged_route);
    MatchOutcome::Matched(Box::new(MatchedRoute {
        tenant_id: selected.tenant_id,
        route_id: selected.route.id,
        priority: selected.route.priority,
        outbound_path,
        match_pattern: http.path.clone(),
        query_allowlist: http.query_allowlist.clone(),
        rate_limit,
        plugins,
        cors,
    }))
    // @cpt-end:cpt-cf-oagw-algo-route-match:p1:inst-match-return
}

/// Maps a match outcome to the failure the caller answers, which is the 404 of
/// an unmatched route or the 400 of a rejected suffix.
///
/// # Errors
///
/// Returns the `RouteNotFound` error for the no-match outcome and the
/// `ValidationError` error for a suffix a route with `path_suffix_mode:
/// disabled` received.
pub fn failure_of(outcome: &MatchOutcome) -> Option<DomainError> {
    match outcome {
        MatchOutcome::Matched(_) => None,
        MatchOutcome::NoMatch => Some(DomainError::gateway(
            ErrorKind::RouteNotFound,
            "no route of the resolved upstream matches the request method and path",
        )),
        MatchOutcome::SuffixRejected => Some(DomainError::gateway(
            ErrorKind::ValidationError,
            "the request supplies a path suffix to a route that declares path_suffix_mode disabled",
        )),
    }
}

/// Whether a candidate's route is enabled, which is the route's own flag.
fn enabled(candidate: &RouteCandidate) -> bool {
    candidate.route.enabled != Some(false)
}

/// Whether the configured path addresses the request path.
///
/// A configured path matches itself and every path it prefixes at a segment
/// boundary, which is how the hierarchical feature reads the same table:
/// `/v1` addresses `/v1` and `/v1/chat`, and never `/v1chat`.
#[must_use]
fn path_matches(configured: &str, request: &str) -> bool {
    if configured == request {
        return true;
    }
    request.starts_with(configured) && request.as_bytes().get(configured.len()) == Some(&b'/')
}

/// The length of the longest configured prefix in the candidate set.
fn longest_prefix(candidates: &[&&RouteCandidate]) -> usize {
    candidates
        .iter()
        .filter_map(|candidate| {
            candidate
                .route
                .match_config
                .http
                .as_ref()
                .map(|http| http.path.len())
        })
        .max()
        .unwrap_or(0)
}

/// Joins the route path with the supplied suffix at a path boundary.
fn append_suffix(route_path: &str, suffix: &str) -> String {
    let trimmed = suffix.trim_matches('/');
    if trimmed.is_empty() {
        return route_path.to_owned();
    }
    format!("{}/{}", route_path.trim_end_matches('/'), trimmed)
}

/// The merged families of the selected route.
///
/// The merged result of the hierarchy walk is used when it resolved the same
/// route; a route the walk's selector did not resolve contributes its own
/// values, which is the route-layer answer for a longer-prefix route on the
/// same upstream the coarse selector skipped.
fn merged_families(
    selected: &RouteCandidate,
    merged: Option<&MergedRouteFamilies>,
) -> (
    Option<EffectiveRateLimit>,
    Option<EffectivePluginChain>,
    Option<EffectiveCors>,
) {
    match merged {
        Some((route_id, rate_limit, plugins, cors)) if route_id == &selected.route.id => {
            (rate_limit.clone(), plugins.clone(), cors.clone())
        }
        _ => (
            selected.route.rate_limit.clone().map(|rate_limit| {
                let mode = rate_limit.sharing.unwrap_or(SharingMode::Private);
                EffectiveRateLimit {
                    owner: selected.tenant_id,
                    mode,
                    rate_limit,
                }
            }),
            selected.route.plugins.clone().map(|plugins| {
                let mode = plugins.sharing.unwrap_or(SharingMode::Private);
                EffectivePluginChain {
                    owner: selected.tenant_id,
                    mode,
                    items: plugins.items,
                    contributors: vec![selected.tenant_id],
                }
            }),
            // A route the walk's selector did not resolve contributes its own
            // CORS object, with the mode its own `sharing` names, which is the
            // same route-layer answer the other two families give.
            selected.route.cors.clone().map(|cors| EffectiveCors {
                owner: selected.tenant_id,
                mode: cors.sharing.unwrap_or(SharingMode::Private),
                cors,
            }),
        ),
    }
}
