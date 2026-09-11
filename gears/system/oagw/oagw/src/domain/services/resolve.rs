//! Proxy-time configuration resolution.
//!
//! One tenant-chain walk answers three questions at once
//! (`cpt-cf-oagw-adr-state-management`): which upstream the alias names,
//! which route matches, and what the effective configuration is once
//! ancestor sharing modes have been applied.

use toolkit_security::SecurityContext;

use crate::domain::alias::normalize_alias;
use crate::domain::dto::ResolvedTarget;
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::{
    AuthConfig, CorsConfig, PluginBinding, RateLimitConfig, Route, SharingMode, Upstream,
};

use super::management::ControlPlane;

/// A candidate upstream found during the chain walk, with its distance from
/// the calling tenant (`0` = the caller's own).
struct Candidate {
    depth: usize,
    upstream: Upstream,
}

impl ControlPlane {
    /// Resolve `alias` plus `(method, path_suffix)` into the fully merged
    /// configuration for one proxy request.
    ///
    /// # Errors
    ///
    /// * `404 RouteNotFound` — no upstream carries the alias anywhere in the
    ///   chain, or no enabled route matches.
    /// * `503 LinkUnavailable` — the upstream, or an ancestor's upstream with
    ///   the same alias, is disabled. An ancestor's `enabled: false` is
    ///   binding on every descendant.
    pub async fn resolve_proxy_target(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &str,
        path_suffix: &str,
    ) -> OagwResult<ResolvedTarget> {
        let alias = normalize_alias(alias);
        let chain = self.tenants().chain(ctx, ctx.subject_tenant_id()).await;

        let mut candidates: Vec<Candidate> = Vec::new();
        for (depth, tenant_id) in chain.iter().enumerate() {
            if let Some(upstream) = self
                .upstream_repo()
                .find_by_alias(*tenant_id, &alias)
                .await?
            {
                candidates.push(Candidate { depth, upstream });
            }
        }
        if candidates.is_empty() {
            return Err(OagwError::route_not_found(format!(
                "no upstream is registered for alias {alias:?}"
            ))
            .with("alias", alias.clone()));
        }

        // An ancestor that disables the upstream disables it for every
        // descendant, so the whole chain is checked, not just the winner.
        if let Some(disabled) = candidates.iter().find(|c| !c.upstream.enabled) {
            return Err(OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream {alias:?} is disabled"),
            )
            .with("alias", alias.clone())
            .with(
                "upstream_id",
                crate::domain::gts_helpers::anonymous_id(
                    crate::domain::gts_helpers::UPSTREAM_TYPE,
                    disabled.upstream.id,
                ),
            ));
        }

        let selected = candidates[0].upstream.clone();
        let route = self
            .match_route(&candidates, method, path_suffix, &alias)
            .await?;

        // Ancestors are merged root-first so the nearest `enforce` has the
        // last word, and an `inherit` only fills a hole the descendant left.
        let ancestors: Vec<&Upstream> = candidates
            .iter()
            .filter(|c| c.depth > 0)
            .map(|c| &c.upstream)
            .rev()
            .collect();

        let auth = merge_auth(&selected, &ancestors);
        let rate_limit = merge_rate_limit(&selected, &route, &ancestors);
        let cors = merge_cors(&selected, &route, &ancestors);
        let plugin_chain = merge_plugin_chain(&selected, &route, &ancestors);
        let tags = merge_tags(&candidates, &route);

        Ok(ResolvedTarget {
            owner_tenant_id: selected.tenant_id,
            headers: selected.headers.clone().unwrap_or_default(),
            upstream: selected,
            route,
            auth,
            rate_limit,
            cors,
            plugin_chain,
            tags,
        })
    }

    /// Pick the route for a request: descendant routes first, then the
    /// longest matching path prefix, then the highest priority.
    async fn match_route(
        &self,
        candidates: &[Candidate],
        method: &str,
        path_suffix: &str,
        alias: &str,
    ) -> OagwResult<Route> {
        let request_path = normalize_request_path(path_suffix);
        let method = method.to_ascii_uppercase();
        let mut best: Option<(usize, usize, i32, u64, Route)> = None;

        for candidate in candidates {
            for route in self
                .route_repo()
                .list_by_upstream(candidate.upstream.id)
                .await?
            {
                if !route.enabled {
                    continue;
                }
                let Some(http) = &route.match_config.http else {
                    continue;
                };
                if !http.methods.contains(&method) {
                    continue;
                }
                if !path_matches_prefix(&request_path, &http.path) {
                    continue;
                }
                let key = (
                    candidate.depth,
                    usize::MAX - http.path.len(),
                    -route.priority,
                    route.seq,
                    route.clone(),
                );
                if best.as_ref().is_none_or(|current| {
                    (key.0, key.1, key.2, key.3) < (current.0, current.1, current.2, current.3)
                }) {
                    best = Some(key);
                }
            }
        }

        best.map(|(_, _, _, _, route)| route).ok_or_else(|| {
            OagwError::route_not_found(format!(
                "no enabled route on upstream {alias:?} matches {method} {request_path}"
            ))
            .with("alias", alias.to_owned())
            .with("path", request_path)
        })
    }
}

/// Turn a proxy-URL path suffix into an absolute path.
#[must_use]
pub fn normalize_request_path(path_suffix: &str) -> String {
    let trimmed = path_suffix.trim_start_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        format!("/{trimmed}")
    }
}

/// Whether `path` sits under the route's `prefix`, on a segment boundary so
/// `/v1/chatty` does not match a route registered at `/v1/chat`.
#[must_use]
pub fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    if path == prefix {
        return true;
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// The remainder of `path` after the route prefix, if any.
#[must_use]
pub fn path_remainder(path: &str, prefix: &str) -> String {
    if prefix == "/" {
        return path.trim_start_matches('/').to_owned();
    }
    let prefix = prefix.trim_end_matches('/');
    path.strip_prefix(prefix)
        .unwrap_or("")
        .trim_start_matches('/')
        .to_owned()
}

/// Effective auth: the nearest enforcing ancestor wins outright, otherwise
/// the descendant's own binding, otherwise the nearest `inherit`.
fn merge_auth(selected: &Upstream, ancestors: &[&Upstream]) -> Option<AuthConfig> {
    let mut effective = selected.auth.clone();
    for ancestor in ancestors {
        let Some(auth) = &ancestor.auth else { continue };
        match auth.sharing {
            SharingMode::Enforce => effective = Some(auth.clone()),
            SharingMode::Inherit => {
                if effective.is_none() {
                    effective = Some(auth.clone());
                }
            }
            SharingMode::Private => {}
        }
    }
    effective
}

/// Effective rate limit: the strictest of the selected upstream, the route,
/// and every ancestor that shares its limit.
fn merge_rate_limit(
    selected: &Upstream,
    route: &Route,
    ancestors: &[&Upstream],
) -> Option<RateLimitConfig> {
    let mut effective: Option<RateLimitConfig> = None;
    let mut apply = |candidate: Option<&RateLimitConfig>| {
        if let Some(candidate) = candidate {
            effective = Some(match effective.take() {
                Some(current) => current.tighten(candidate),
                None => candidate.clone(),
            });
        }
    };
    for ancestor in ancestors {
        if let Some(limit) = &ancestor.rate_limit
            && limit.sharing.visible_to_descendants()
        {
            apply(Some(limit));
        }
    }
    apply(selected.rate_limit.as_ref());
    apply(route.rate_limit.as_ref());
    effective
}

/// Effective CORS: ancestors merge per their sharing mode, then the route
/// overrides the upstream.
fn merge_cors(selected: &Upstream, route: &Route, ancestors: &[&Upstream]) -> Option<CorsConfig> {
    let mut effective = selected.cors.clone();
    for ancestor in ancestors {
        let Some(cors) = &ancestor.cors else { continue };
        if !cors.sharing.visible_to_descendants() {
            continue;
        }
        effective = Some(match effective.take() {
            Some(descendant) => CorsConfig::merge_descendant(cors, &descendant),
            None => cors.clone(),
        });
    }
    if let Some(route_cors) = &route.cors {
        effective = Some(match effective.take() {
            Some(upstream_cors) if upstream_cors.sharing == SharingMode::Enforce => upstream_cors,
            Some(upstream_cors) => CorsConfig::merge_descendant(&upstream_cors, route_cors),
            None => route_cors.clone(),
        });
    }
    effective
}

/// Effective plugin chain: visible ancestor chains root-first, then the
/// selected upstream's, then the route's.
fn merge_plugin_chain(
    selected: &Upstream,
    route: &Route,
    ancestors: &[&Upstream],
) -> Vec<PluginBinding> {
    let mut chain = Vec::new();
    for ancestor in ancestors {
        if let Some(plugins) = &ancestor.plugins
            && plugins.sharing.visible_to_descendants()
        {
            chain.extend(plugins.items.iter().cloned());
        }
    }
    if let Some(plugins) = &selected.plugins {
        chain.extend(plugins.items.iter().cloned());
    }
    if let Some(plugins) = &route.plugins {
        chain.extend(plugins.items.iter().cloned());
    }
    chain
}

/// Tags are add-only: the union of everything in the chain, plus the route's.
fn merge_tags(candidates: &[Candidate], route: &Route) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for candidate in candidates.iter().rev() {
        for tag in &candidate.upstream.tags {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    for tag in &route.tags {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_paths_are_absolute() {
        assert_eq!(normalize_request_path(""), "/");
        assert_eq!(normalize_request_path("/"), "/");
        assert_eq!(normalize_request_path("v1/chat"), "/v1/chat");
        assert_eq!(normalize_request_path("/v1/chat"), "/v1/chat");
    }

    #[test]
    fn prefix_matching_respects_segment_boundaries() {
        assert!(path_matches_prefix("/v1/chat", "/v1/chat"));
        assert!(path_matches_prefix("/v1/chat/completions", "/v1/chat"));
        assert!(!path_matches_prefix("/v1/chatty", "/v1/chat"));
        assert!(path_matches_prefix("/anything", "/"));
        assert!(!path_matches_prefix("/v2/chat", "/v1"));
    }

    #[test]
    fn remainder_is_what_append_mode_appends() {
        assert_eq!(
            path_remainder("/v1/chat/completions", "/v1/chat"),
            "completions"
        );
        assert_eq!(path_remainder("/v1/chat", "/v1/chat"), "");
        assert_eq!(path_remainder("/v1/chat", "/"), "v1/chat");
    }
}
