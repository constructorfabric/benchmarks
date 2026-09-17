//! Proxy routing: alias resolution, route matching and endpoint selection.
//!
//! This module holds the *decision* half of the data plane (DESIGN §3.5
//! "Alias Resolution", "Guard Rules", ADR-0001 Appendix A); the *execution*
//! half (header transformation, forwarding) lives in [`crate::domain::headers`]
//! and [`crate::domain::services::data_plane`].
//!
//! Three decisions are made here, in the order the proxy applies them:
//!
//! 1. **Alias resolution** walks the tenant chain from the calling tenant to
//!    the root and returns the closest **enabled** upstream with the requested
//!    alias — descendants shadow ancestors (DESIGN §3.3 "Tenant Scoping":
//!    ancestor resources are inherited at proxy time and invisible through the
//!    management API).
//! 2. **Route matching** walks the same chain again for routes bound to the
//!    selected upstream and picks the longest matching path prefix; its guards
//!    (method allowlist, path-suffix mode, query allowlist) then decide.
//! 3. **Endpoint selection** applies the `X-OAGW-Target-Host` behavior matrix
//!    of ADR-0001 Appendix A.
//!
//! # Route ordering and the `priority` deviation
//!
//! DESIGN §3.6 models a route as carrying an `Int priority`, and its
//! invariants are written against `(path_prefix, priority)`: no two enabled
//! routes under one upstream may share that pair for the same method, and
//! matching is "by `(upstream_id, method, longest path prefix, priority)`".
//! The wire schema the crate is bound to
//! (`docs/schemas/route.v1.schema.json`) is `additionalProperties: false` and
//! declares **no `priority` field**, so
//! [`RouteSpec`](crate::domain::types::RouteSpec) cannot carry one and the
//! documented `(path_prefix, priority)` uniqueness invariant degrades here to
//! **longest path prefix, then nearest tenant in the chain, then insertion
//! order**:
//!
//! 1. the longest `match.http.path` prefix that equals or extends the
//!    alias-stripped request path wins ([`match_route`]);
//! 2. on equal length, the route owned by the nearer tenant of the chain wins
//!    (descendants shadow ancestors, DESIGN §3.3 "Tenant Scoping") —
//!    [`route_candidates`] walks the chain nearest-first and feeds the matcher
//!    in that order;
//! 3. still tied, the route inserted first wins, which is exactly the order the
//!    route store lists them in (and the order the management API reports).
//!
//! Match-rule uniqueness within an upstream (`UNIQUE (upstream_id, match)` in
//! the route store) makes ties impossible for the *same* tenant, so the
//! insertion-order tie-break only decides between tenants of a chain, where the
//! chain order is already deterministic.

use std::sync::Arc;

use async_trait::async_trait;
use http::Method;
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::types::{
    Endpoint, HttpMatch, PathSuffixMode, Protocol, Route, RouteMethod, Upstream,
};
use crate::error::OagwError;

/// Header the caller uses to pin an endpoint of a multi-endpoint upstream.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

// ---------------------------------------------------------------------------
// Tenant hierarchy port
// ---------------------------------------------------------------------------

/// Port to the tenant hierarchy (DESIGN §3.3 "Tenant Scoping").
///
/// The production adapter wraps `tenant_resolver_sdk::TenantResolverClient`
/// (see [`crate::infra::tenant_hierarchy`]); tests inject a stub. The data
/// plane only ever needs the ancestor chain of the calling tenant.
#[async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// Ancestor chain of `tenant`, **nearest first and including `tenant`**.
    ///
    /// Implementations must degrade to `[tenant]` when the hierarchy cannot be
    /// traversed: a shorter chain can only *narrow* the set of tenants whose
    /// configuration is consulted — the request then fails with 404 instead of
    /// silently gaining access to another tenant's upstreams. A wider chain
    /// would be a tenant-isolation failure and is therefore never acceptable.
    async fn chain(&self, security: &SecurityContext, tenant: Uuid) -> Vec<Uuid>;
}

// ---------------------------------------------------------------------------
// Alias resolution
// ---------------------------------------------------------------------------

/// Normalize an alias as it arrives on the proxy path.
///
/// Aliases are stored normalized (DESIGN §3.5 "Alias Normalization": ASCII
/// lowercase, trailing dots stripped), so resolution is a case-insensitive
/// compare after the same normalization.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// The outcome of alias resolution (DESIGN §3.5 "Shadowing Behavior").
#[derive(Debug)]
pub enum AliasResolution {
    /// No tenant of the chain owns the alias (404).
    NotFound,
    /// The nearest owner disabled its upstream. The alias is *deliberately*
    /// unavailable, so it is reported instead of being served from an ancestor
    /// and instead of being reported as unknown (503,
    /// PRD §5.1 "Enable/Disable Semantics").
    Disabled(Arc<Upstream>),
    /// The nearest owner serves the alias.
    Found(Arc<Upstream>),
}

/// Resolve `alias` by walking `chain` from the calling tenant to the root.
///
/// The first tenant (nearest first) that **owns** the alias wins, so a
/// descendant shadows an ancestor. Ownership is decisive whatever the enabled
/// state: a disabled upstream of a nearer tenant stops the walk, because
/// silently serving the alias from a parent would defeat the operator who
/// disabled it (PRD §5.1 "Enable/Disable Semantics").
///
/// `lookup` resolves `(tenant, alias)` in the upstream store.
#[must_use]
pub fn resolve_alias<F>(chain: &[Uuid], alias: &str, mut lookup: F) -> AliasResolution
where
    F: FnMut(Uuid, &str) -> Option<Arc<Upstream>>,
{
    let alias = normalize_alias(alias);
    if alias.is_empty() {
        return AliasResolution::NotFound;
    }

    for tenant in chain {
        if let Some(upstream) = lookup(*tenant, &alias) {
            return match upstream.is_enabled() {
                true => AliasResolution::Found(upstream),
                false => AliasResolution::Disabled(upstream),
            };
        }
    }

    AliasResolution::NotFound
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// The route that matched a proxy request, plus the path to forward.
#[derive(Debug)]
pub struct MatchedRoute {
    /// The winning route (longest path prefix, nearest tenant first).
    pub route: Arc<Route>,
    /// Path forwarded to the upstream: `match.http.path` plus the request's
    /// path suffix when `path_suffix_mode` is `append`.
    pub forwarded_path: String,
}

/// Match `candidates` against the proxy request (DESIGN §3.5 "Guard Rules").
///
/// `candidates` must already be ordered by priority: nearest tenant of the
/// chain first (see [`route_candidates`]). A route matches when the request
/// method is in `match.http.methods` **and** the alias-stripped path equals or
/// starts with `match.http.path`; the longest such path wins, so a more
/// specific route always shadows a broader one.
///
/// `stripped_path` is the request path with the proxy prefix and the alias
/// removed — `/oagw/v1/proxy/api.openai.com/v1/chat` yields `/v1/chat`.
///
/// Disabled routes never match: the check is repeated here so that a caller
/// which forgets [`route_candidates`] still cannot route through a disabled
/// route (PRD §5.1 "Enable/Disable Semantics").
///
/// # Errors
/// * [`OagwErrorKind::RouteNotFound`](crate::error::OagwErrorKind) — no route
///   matches the method and path.
/// * [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) — the
///   winning route rejects the path suffix (`path_suffix_mode: disabled`) or a
///   query parameter outside `match.http.query_allowlist`.
pub fn match_route(
    candidates: &[Arc<Route>],
    method: &Method,
    stripped_path: &str,
    query_keys: &[String],
) -> Result<MatchedRoute, OagwError> {
    let (best, http) = best_candidate(candidates, method, stripped_path).ok_or_else(|| {
        OagwError::route_not_found(format!(
            "no route matches {method} for this upstream (path '{stripped_path}')"
        ))
    })?;

    let remainder = remainder_of(&http.path, stripped_path);
    let forwarded_path = forwarded_path(&http.path, remainder, http.path_suffix_mode)?;

    validate_query(&http.query_allowlist, query_keys)?;

    Ok(MatchedRoute {
        route: Arc::clone(best),
        forwarded_path,
    })
}

/// The part of `stripped_path` that extends `route_path`.
///
/// Empty when the request path *is* the route path. The alias root (`/`) is the
/// exception: it already matches every path below the alias, so the whole
/// stripped path is its remainder and is forwarded as-is.
fn remainder_of<'a>(route_path: &str, stripped_path: &'a str) -> &'a str {
    if route_path == "/" {
        return stripped_path;
    }
    if stripped_path.len() <= route_path.len() {
        return "";
    }
    &stripped_path[route_path.len()..]
}

/// The candidate with the longest matching path prefix, first one wins on a tie.
///
/// The HTTP match rule is returned *with* the route: only a candidate that has
/// one can match, so handing both out keeps [`match_route`] free of a
/// "cannot happen" unwrap on the way out (see the S2 review findings).
fn best_candidate<'a>(
    candidates: &'a [Arc<Route>],
    method: &Method,
    stripped_path: &str,
) -> Option<(&'a Arc<Route>, &'a HttpMatch)> {
    let mut best: Option<(&'a Arc<Route>, &'a HttpMatch)> = None;
    let mut best_len = 0_usize;

    for candidate in candidates {
        let Some(http) = &candidate.spec.match_rules.http else {
            continue;
        };
        if !candidate.is_enabled() {
            continue;
        }
        if !is_method_allowed(&http.methods, method) {
            continue;
        }
        if !path_matches(&http.path, stripped_path) {
            continue;
        }
        // Longest path prefix wins; on equal length the earlier candidate (a
        // nearer tenant, or an older route of the same tenant) is kept.
        if best.is_none() || http.path.len() > best_len {
            best = Some((candidate, http));
            best_len = http.path.len();
        }
    }

    best
}

/// Route methods are matched case-insensitively against the request method.
fn is_method_allowed(methods: &[RouteMethod], request: &Method) -> bool {
    methods
        .iter()
        .any(|allowed| allowed.as_str().eq_ignore_ascii_case(request.as_str()))
}

/// `route.path` matches when the stripped path is equal to it or extends it.
///
/// The documented contract is a plain string prefix (DESIGN §3.5 "Guard
/// Rules": "equals or starts with"), without a path-segment boundary
/// requirement: `/v1` therefore also matches `/v1beta`. Routes that need a
/// segment boundary configure the longer path.
fn path_matches(route_path: &str, path_suffix: &str) -> bool {
    if route_path == "/" {
        // The alias root matches the alias itself and every path below it.
        return true;
    }
    path_suffix == route_path || path_suffix.starts_with(route_path)
}

// ---------------------------------------------------------------------------
// Proxy path suffix validation
// ---------------------------------------------------------------------------

/// Validate the path suffix of a proxy request before any routing or dialing.
///
/// `raw` is the suffix exactly as it was sent (still percent-encoded, taken
/// from the request line); `decoded` is what the transport handed over after
/// percent-decoding. The two must describe the **same** segment structure,
/// because a proxy forwards the decoded form while the upstream resolves
/// whatever the bytes it receives mean to *its* router:
///
/// * a dot segment (`.` / `..`) in the decoded form is a traversal attempt:
///   `/v1/public/../private` matches the `/v1/public` route on the way in and
///   resolves to `/v1/private` at the upstream, which is exactly the confusion
///   a path-scoped guard rule must not allow;
/// * an interior empty segment (`//`) re-normalizes into a different path at
///   some upstream routers, so it is rejected as well;
/// * an **encoded** path separator (`%2f`) in the raw form decodes into a
///   segment structure the router that matched this request never saw, so it is
///   rejected even when the decoded form looks benign.
///
/// Every check here runs *before* alias resolution, endpoint selection and
/// forwarding, so a rejected request never reaches an upstream.
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) — 400, with
/// the `field` extension naming the offending part of the request line.
pub fn validate_path_suffix(raw: &str, decoded: &str) -> Result<(), OagwError> {
    for encoded_separator in ["%2f", "%2F"] {
        if raw.contains(encoded_separator) {
            return Err(OagwError::validation(format!(
                "the proxied path contains an encoded path separator ('{encoded_separator}'); the \
                 request must use one segment per path element"
            ))
            .with_extension("field", json!("path")));
        }
    }

    validate_path_segments(decoded)
}

/// Validate a decoded path: no dot segments and no interior empty segments.
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) when the
/// path carries a segment that changes meaning under normalization.
pub fn validate_path_segments(decoded: &str) -> Result<(), OagwError> {
    for segment in decoded.split('/') {
        if segment == "." || segment == ".." {
            return Err(OagwError::validation(format!(
                "the proxied path contains the dot segment '{segment}'; dot segments are not \
                 forwarded"
            ))
            .with_extension("field", json!("path")));
        }
    }

    if has_empty_segment(decoded) {
        return Err(OagwError::validation(
            "the proxied path contains an empty segment ('//'); empty segments are not forwarded",
        )
        .with_extension("field", json!("path")));
    }

    Ok(())
}

/// `true` when `path` carries an interior empty segment (`//`).
///
/// A single leading or trailing separator is the ordinary path form, not a
/// segment of its own, and an empty path is the alias itself.
fn has_empty_segment(path: &str) -> bool {
    let body = path.strip_prefix('/').unwrap_or(path);
    let body = body.strip_suffix('/').unwrap_or(body);
    if body.is_empty() {
        return false;
    }

    body.split('/').any(str::is_empty)
}

/// Path forwarded to the upstream (DESIGN §3.5 "Transformation Rules").
///
/// `path_suffix_mode: disabled` rejects any suffix; `append` (the default)
/// appends it to `match.http.path`, collapsing a doubled separator so that a
/// route path of `/v1/` plus a suffix of `/chat` forwards `/v1/chat`.
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) when a
/// suffix is present and the route disables it.
pub fn forwarded_path(
    route_path: &str,
    path_suffix: &str,
    mode: PathSuffixMode,
) -> Result<String, OagwError> {
    if path_suffix.is_empty() {
        return Ok(route_path.to_owned());
    }

    if mode == PathSuffixMode::Disabled {
        return Err(OagwError::validation(format!(
            "this route does not accept a path suffix ('{path_suffix}'); `path_suffix_mode` is \
             `disabled`"
        ))
        .with_extension("field", json!("match.http.path_suffix_mode")));
    }

    let mut forwarded = route_path.to_owned();
    if forwarded.ends_with('/') && path_suffix.starts_with('/') {
        forwarded.pop();
    }
    forwarded.push_str(path_suffix);

    Ok(forwarded)
}

/// Validate the request query parameters against the route allowlist.
///
/// An empty allowlist allows **no** parameter (DESIGN §3.5 "Guard Rules").
///
/// # Errors
/// [`OagwErrorKind::ValidationError`](crate::error::OagwErrorKind) when a
/// parameter is not allowlisted.
pub fn validate_query(allowlist: &[String], query_keys: &[String]) -> Result<(), OagwError> {
    for key in query_keys {
        let allowed = allowlist.iter().any(|name| name == key);
        if !allowed {
            return Err(OagwError::validation(format!(
                "query parameter '{key}' is not allowed by this route (allowed: [{}])",
                allowlist.join(", ")
            ))
            .with_extension("field", json!("match.http.query_allowlist")));
        }
    }

    Ok(())
}

/// Collect the route candidates for `upstream_id` along the tenant chain.
///
/// The walk is nearest-tenant-first, so the caller can rely on the order for
/// the "descendants take priority" rule of DESIGN §3.3 "Tenant Scoping".
/// Disabled routes are excluded here: a disabled route is simply not matched
/// (PRD §5.1 "Enable/Disable Semantics").
///
/// `lookup` resolves `(tenant, upstream_id)` in the route store.
#[must_use]
pub fn route_candidates<F>(chain: &[Uuid], upstream_id: Uuid, mut lookup: F) -> Vec<Arc<Route>>
where
    F: FnMut(Uuid, Uuid) -> Vec<Arc<Route>>,
{
    let mut candidates = Vec::new();
    for tenant in chain {
        for route in lookup(*tenant, upstream_id) {
            if route.is_enabled() {
                candidates.push(route);
            }
        }
    }
    candidates
}

// ---------------------------------------------------------------------------
// Endpoint selection
// ---------------------------------------------------------------------------

/// Select the endpoint of `upstream` for a proxy request (ADR-0001 Appendix A).
///
/// * an explicit `X-OAGW-Target-Host` pins the endpoint — always validated, and
///   rejected when it matches no configured endpoint;
/// * without it, a single-endpoint upstream routes to that endpoint, a
///   common-suffix multi-endpoint upstream requires the header, and every
///   other multi-endpoint upstream round-robins.
///
/// # Errors
/// * [`OagwErrorKind::MissingTargetHost`](crate::error::OagwErrorKind) — the
///   alias names a family of hosts and the header is absent.
/// * [`OagwErrorKind::InvalidTargetHost`](crate::error::OagwErrorKind) — the
///   header value is not a hostname or IP literal.
/// * [`OagwErrorKind::UnknownTargetHost`](crate::error::OagwErrorKind) — the
///   header value matches no endpoint of the upstream.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
    round_robin: &mut dyn FnMut() -> usize,
) -> Result<&'a Endpoint, OagwError> {
    select_endpoint_with_policy(
        upstream,
        upstream.requires_target_host(),
        target_host,
        round_robin,
    )
}

/// [`select_endpoint`] with the "needs `X-OAGW-Target-Host`" decision supplied
/// by the caller.
///
/// Deriving that decision re-runs the public-suffix alias derivation over the
/// endpoint pool, which is work no request path should repeat: the data plane
/// memoizes it per upstream id (invalidated by the record's `updated_at`) and
/// hands the result in here. The value *must* be derived from the same upstream
/// record that is passed in — an out-of-date flag would round-robin a pool the
/// caller must pin, or pin one that may round-robin.
///
/// # Errors
/// As [`select_endpoint`].
pub fn select_endpoint_with_policy<'a>(
    upstream: &'a Upstream,
    requires_target_host: bool,
    target_host: Option<&str>,
    round_robin: &mut dyn FnMut() -> usize,
) -> Result<&'a Endpoint, OagwError> {
    let endpoints = upstream.endpoints();
    let Some(target_host) = target_host else {
        return match endpoints {
            [only] => Ok(only),
            _ if requires_target_host => Err(missing_target_host(upstream)),
            _ => {
                let index = round_robin() % endpoints.len();
                Ok(&endpoints[index])
            }
        };
    };

    // An explicit target host is validated even when it would not be required,
    // so a typo can never silently fall through to load balancing.
    let normalized = validate_target_host(target_host)?;
    endpoint_for_host(upstream, &normalized)
        .ok_or_else(|| unknown_target_host(upstream, target_host))
}

/// Validate the *shape* of an `X-OAGW-Target-Host` value.
///
/// The header must name a host — a hostname or an IP literal — with no port,
/// path or other syntax; `us.vendor.com:8443` is a 400, not a route.
///
/// # Errors
/// [`OagwErrorKind::InvalidTargetHost`](crate::error::OagwErrorKind) when the
/// value is not a bare host.
pub fn validate_target_host(value: &str) -> Result<String, OagwError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(
            OagwError::invalid_target_host("X-OAGW-Target-Host must not be empty")
                .with_extension("invalid_value", serde_json::json!(value)),
        );
    }

    crate::domain::types::validate_host(value).map_err(|error| {
        OagwError::invalid_target_host(
            "X-OAGW-Target-Host must be a valid hostname or IP address (no port, path, or \
             special characters)",
        )
        .with_extension("invalid_value", json!(value))
        .with_extension("detail", json!(error.detail().to_owned()))
    })
}

/// The endpoint of `upstream` whose host is `host`, if any.
///
/// Hosts are compared after the same normalization `validate_host` applies, so
/// `Us.Vendor.COM.` matches the stored `us.vendor.com`.
#[must_use]
pub fn endpoint_for_host<'a>(upstream: &'a Upstream, host: &str) -> Option<&'a Endpoint> {
    upstream
        .endpoints()
        .iter()
        .find(|endpoint| endpoint.host.eq_ignore_ascii_case(host))
}

fn missing_target_host(upstream: &Upstream) -> OagwError {
    OagwError::missing_target_host(format!(
        "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix \
         alias. Valid hosts: [{}]",
        host_list(upstream)
    ))
    .with_extension("upstream_id", json!(upstream.gts_id()))
    .with_extension("alias", json!(upstream.alias))
    .with_extension("valid_hosts", json!(valid_hosts(upstream)))
}

fn unknown_target_host(upstream: &Upstream, value: &str) -> OagwError {
    OagwError::unknown_target_host(format!(
        "X-OAGW-Target-Host '{value}' does not match any configured endpoint. Valid hosts: [{}]",
        host_list(upstream)
    ))
    .with_extension("upstream_id", json!(upstream.gts_id()))
    .with_extension("invalid_value", json!(value))
    .with_extension("valid_hosts", json!(valid_hosts(upstream)))
}

fn host_list(upstream: &Upstream) -> String {
    upstream
        .endpoints()
        .iter()
        .map(|endpoint| endpoint.host.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn valid_hosts(upstream: &Upstream) -> Vec<String> {
    upstream
        .endpoints()
        .iter()
        .map(|endpoint| endpoint.host.clone())
        .collect()
}

/// Whether an upstream can be proxied at all by this slice.
///
/// gRPC upstreams are a later phase (DESIGN §3.3 "Proxy API": no gRPC proxy
/// code path is currently implemented or reachable), so they are reported as
/// unmatchable rather than proxied with the wrong match strategy.
#[must_use]
pub const fn is_http_upstream(upstream: &Upstream) -> bool {
    matches!(upstream.spec.protocol, Protocol::Http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::{GrpcMatch, HttpMatch, RouteMatch, RouteSpec, Scheme, ServerConfig};

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: 443,
        }
    }

    fn make_upstream(alias: &str, hosts: &[&str]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            created_at: 0,
            updated_at: 0,
            spec: crate::domain::types::UpstreamSpec {
                alias: Some(alias.to_owned()),
                server: ServerConfig {
                    endpoints: hosts.iter().map(|host| endpoint(host)).collect(),
                },
                ..crate::domain::types::UpstreamSpec::default()
            },
        }
    }

    fn http_route(tenant: Uuid, upstream_id: Uuid, path: &str, methods: &[RouteMethod]) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id,
                match_rules: RouteMatch {
                    http: Some(HttpMatch {
                        methods: methods.to_vec(),
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        }
    }

    fn get(path: &str) -> Result<MatchedRoute, OagwError> {
        let candidates = Vec::new();
        match_route(&candidates, &Method::GET, path, &[])
    }

    /// Minimal percent-decoder, mirroring what the transport's `Path` extractor
    /// does to a captured path segment.
    fn percent_decode(raw: &str) -> String {
        let bytes = raw.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%'
                && index + 2 < bytes.len()
                && let Ok(byte) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("zz"),
                    16,
                )
            {
                decoded.push(byte);
                index += 3;
                continue;
            }
            decoded.push(bytes[index]);
            index += 1;
        }
        String::from_utf8_lossy(&decoded).into_owned()
    }

    #[test]
    fn alias_normalization_matches_the_stored_form() {
        assert_eq!(normalize_alias("Api.OpenAI.COM"), "api.openai.com");
        assert_eq!(normalize_alias("api.openai.com."), "api.openai.com");
        assert_eq!(normalize_alias("  Vendor.com:8443 "), "vendor.com:8443");
        assert_eq!(normalize_alias(""), "");
    }

    #[test]
    fn alias_resolution_walks_the_chain_nearest_first() {
        let root = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let chain = [leaf, root];
        let ancestor = make_upstream("api.openai.com", &["api.openai.com"]);
        let ancestor_id = ancestor.id;
        let mut root_upstream = ancestor;
        root_upstream.tenant_id = root;

        let found = match resolve_alias(&chain, "api.openai.com", |tenant, alias| {
            if tenant == root && alias == "api.openai.com" {
                Some(Arc::new(root_upstream.clone()))
            } else {
                None
            }
        }) {
            AliasResolution::Found(upstream) => upstream,
            other => panic!("the ancestor upstream is inherited, got {other:?}"),
        };

        assert_eq!(found.id, ancestor_id);
        assert_eq!(found.tenant_id, root);
    }

    #[test]
    fn alias_resolution_shadows_with_the_descendant() {
        let root = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let chain = [leaf, root];
        let mut descendant = make_upstream("api.openai.com", &["api.openai.com"]);
        descendant.tenant_id = leaf;
        let descendant_id = descendant.id;
        let mut ancestor = make_upstream("api.openai.com", &["api.openai.com"]);
        ancestor.tenant_id = root;

        let shadowed = [ancestor, descendant.clone()];
        let found = match resolve_alias(&chain, "api.openai.com", |tenant, _| {
            shadowed
                .iter()
                .position(|candidate| candidate.tenant_id == tenant)
                .map(|index| Arc::new(shadowed[index].clone()))
        }) {
            AliasResolution::Found(upstream) => upstream,
            other => panic!("the descendant shadows the ancestor, got {other:?}"),
        };

        assert_eq!(found.id, descendant_id);
        assert_eq!(found.tenant_id, leaf);
    }

    #[test]
    fn a_disabled_upstream_stops_the_walk() {
        let tenant = Uuid::new_v4();
        let mut disabled = make_upstream("api.openai.com", &["api.openai.com"]);
        disabled.spec.enabled = false;

        let resolution = resolve_alias(&[tenant], "api.openai.com", |_, _| {
            Some(Arc::new(disabled.clone()))
        });

        match resolution {
            AliasResolution::Disabled(found) => assert_eq!(found.alias, "api.openai.com"),
            other => panic!("a disabled upstream is reported as disabled, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_alias_is_reported_as_not_found() {
        let tenant = Uuid::new_v4();

        assert!(matches!(
            resolve_alias(&[tenant], "api.openai.com", |_, _| None),
            AliasResolution::NotFound
        ));
        assert!(matches!(
            resolve_alias(&[tenant], "", |_, _| panic!(
                "an empty alias is never looked up"
            )),
            AliasResolution::NotFound
        ));
    }

    #[test]
    fn alias_resolution_is_case_insensitive() {
        let tenant = Uuid::new_v4();
        let stored = make_upstream("api.openai.com", &["api.openai.com"]);

        assert!(matches!(
            resolve_alias(&[tenant], "Api.OpenAI.Com", |_, _| Some(Arc::new(
                stored.clone()
            ))),
            AliasResolution::Found(_)
        ));
    }

    #[test]
    fn a_disabled_descendant_does_not_reveal_the_ancestor() {
        let root = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let chain = [leaf, root];
        let mut descendant = make_upstream("api.openai.com", &["api.openai.com"]);
        descendant.tenant_id = leaf;
        descendant.spec.enabled = false;
        let mut ancestor = make_upstream("api.openai.com", &["api.openai.com"]);
        ancestor.tenant_id = root;

        let owned = [ancestor, descendant];
        let resolution = resolve_alias(&chain, "api.openai.com", |tenant, _| {
            owned
                .iter()
                .position(|candidate| candidate.tenant_id == tenant)
                .map(|index| Arc::new(owned[index].clone()))
        });

        assert!(
            matches!(resolution, AliasResolution::Disabled(_)),
            "the disabled descendant shadows the enabled ancestor"
        );
    }

    #[test]
    fn route_matching_picks_the_longest_prefix() {
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let broad = Arc::new(http_route(tenant, upstream_id, "/v1", &[RouteMethod::Get]));
        let specific = Arc::new(http_route(
            tenant,
            upstream_id,
            "/v1/chat",
            &[RouteMethod::Get],
        ));
        let candidates = vec![broad, specific];

        // `/oagw/v1/proxy/api.openai.com/v1/chat/completions` strips to
        // `/v1/chat/completions`.
        let matched = match_route(&candidates, &Method::GET, "/v1/chat/completions", &[])
            .expect("the specific route wins");

        assert_eq!(
            matched
                .route
                .spec
                .match_rules
                .http
                .as_ref()
                .expect("http")
                .path,
            "/v1/chat"
        );
        assert_eq!(
            matched.forwarded_path, "/v1/chat/completions",
            "the suffix is appended to the route path"
        );
    }

    #[test]
    fn route_matching_rejects_a_method_outside_the_allowlist() {
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let candidates = vec![Arc::new(http_route(
            tenant,
            upstream_id,
            "/v1",
            &[RouteMethod::Get],
        ))];

        let err =
            match_route(&candidates, &Method::POST, "/v1", &[]).expect_err("POST is not allowed");
        assert_eq!(err.status().as_u16(), 404);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::RouteNotFound);
    }

    #[test]
    fn route_matching_is_order_stable_for_equal_prefixes() {
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let first = Arc::new(http_route(tenant, upstream_id, "/v1", &[RouteMethod::Get]));
        let second = Arc::new(http_route(tenant, upstream_id, "/v1", &[RouteMethod::Get]));
        let candidates = vec![Arc::clone(&first), Arc::clone(&second)];

        let matched = match_route(&candidates, &Method::GET, "/v1/x", &[]).expect("matched");

        assert_eq!(
            matched.route.id, first.id,
            "the nearest tenant / oldest route wins a tie"
        );
    }

    #[test]
    fn route_matching_ignores_disabled_and_non_http_routes() {
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let mut disabled = http_route(tenant, upstream_id, "/v1", &[RouteMethod::Get]);
        disabled.spec.enabled = false;
        let grpc_only = Route {
            id: Uuid::new_v4(),
            tenant_id: tenant,
            upstream_id,
            created_at: 0,
            updated_at: 0,
            spec: RouteSpec {
                upstream_id,
                match_rules: RouteMatch {
                    http: None,
                    grpc: Some(GrpcMatch {
                        service: "pkg.Service".to_owned(),
                        method: "Call".to_owned(),
                    }),
                },
                enabled: true,
                tags: Vec::new(),
                plugins: None,
                rate_limit: None,
            },
        };

        assert!(get("/v1").is_err(), "nothing matched");

        let candidates = vec![Arc::new(disabled), Arc::new(grpc_only)];
        assert!(
            match_route(&candidates, &Method::GET, "/v1", &[]).is_err(),
            "disabled routes and gRPC matches never match an HTTP upstream"
        );
    }

    #[test]
    fn route_candidates_walk_the_chain_and_drop_disabled_routes() {
        let root = Uuid::new_v4();
        let leaf = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let mut ancestor = http_route(root, upstream_id, "/ancestor", &[RouteMethod::Get]);
        ancestor.id = Uuid::new_v4();
        let mut disabled = http_route(leaf, upstream_id, "/leaf-disabled", &[RouteMethod::Get]);
        disabled.spec.enabled = false;
        let mut descendant = http_route(leaf, upstream_id, "/leaf", &[RouteMethod::Get]);
        descendant.id = Uuid::new_v4();

        let per_tenant: Vec<(Uuid, Vec<Arc<Route>>)> = vec![
            (leaf, vec![Arc::new(disabled), Arc::new(descendant.clone())]),
            (root, vec![Arc::new(ancestor.clone())]),
        ];

        let candidates = route_candidates(&[leaf, root], upstream_id, |tenant, _| {
            per_tenant
                .iter()
                .find(|(owner, _)| *owner == tenant)
                .map(|(_, routes)| routes.clone())
                .unwrap_or_default()
        });

        let ids: Vec<Uuid> = candidates.iter().map(|route| route.id).collect();
        assert_eq!(
            ids,
            vec![descendant.id, ancestor.id],
            "nearest tenant first, disabled routes excluded"
        );
    }

    #[test]
    fn path_suffix_disabled_rejects_any_suffix() {
        let err = forwarded_path("/v1/status", "/extra", PathSuffixMode::Disabled)
            .expect_err("suffix rejected");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::ValidationError);

        assert_eq!(
            forwarded_path("/v1/status", "", PathSuffixMode::Disabled).expect("no suffix"),
            "/v1/status"
        );
    }

    #[test]
    fn an_alias_root_route_forwards_the_whole_stripped_path() {
        let tenant = Uuid::new_v4();
        let upstream_id = Uuid::new_v4();
        let candidates = vec![Arc::new(http_route(
            tenant,
            upstream_id,
            "/",
            &[RouteMethod::Get],
        ))];

        let matched = match_route(&candidates, &Method::GET, "/v1/chat/completions", &[])
            .expect("the alias root matches every path below the alias");
        assert_eq!(matched.forwarded_path, "/v1/chat/completions");

        let matched = match_route(&candidates, &Method::GET, "/", &[]).expect("the alias itself");
        assert_eq!(matched.forwarded_path, "/");

        let matched = match_route(&candidates, &Method::GET, "", &[]).expect("the alias alone");
        assert_eq!(matched.forwarded_path, "/");
    }

    #[test]
    fn path_suffix_append_joins_without_doubling_the_separator() {
        assert_eq!(
            forwarded_path("/v1", "/chat", PathSuffixMode::Append).expect("append"),
            "/v1/chat"
        );
        assert_eq!(
            forwarded_path("/v1/", "/chat", PathSuffixMode::Append).expect("append"),
            "/v1/chat"
        );
        assert_eq!(
            forwarded_path("", "/chat", PathSuffixMode::Append).expect("append"),
            "/chat"
        );
    }

    #[test]
    fn a_dot_segment_in_the_path_suffix_is_rejected_before_routing() {
        for raw in [
            "v1/public/../private",
            "v1/%2e%2e/private",
            "v1/public/%2E%2E/x",
        ] {
            let decoded = percent_decode(raw);
            let err = validate_path_suffix(raw, &decoded).expect_err(raw);
            assert_eq!(err.status().as_u16(), 400, "{raw}");
            assert_eq!(
                err.kind(),
                crate::error::OagwErrorKind::ValidationError,
                "{raw}"
            );
            assert_eq!(
                err.extensions()
                    .get("field")
                    .and_then(|value| value.as_str()),
                Some("path"),
                "the offending part of the request line is named: {raw}"
            );
        }

        assert!(
            validate_path_segments("/v1/public/../private").is_err(),
            "the decoded form is validated, not just the raw one"
        );
    }

    #[test]
    fn an_encoded_path_separator_is_rejected_even_when_the_decoded_form_is_benign() {
        let err = validate_path_suffix("v1/public%2fprivate", "/v1/public/private")
            .expect_err("encoded separator");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::ValidationError);
        assert!(err.detail().contains("%2f"), "{}", err.detail());

        let err = validate_path_suffix("v1/public%2Fprivate", "/v1/public/private")
            .expect_err("encoded separator, uppercase");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn an_interior_empty_segment_is_rejected_but_a_trailing_separator_is_not() {
        assert!(validate_path_segments("/v1//private").is_err());
        assert!(validate_path_segments("/v1/private").is_ok());
        assert!(validate_path_segments("/v1/private/").is_ok(), "trailing /");
        assert!(validate_path_segments("/").is_ok(), "the alias root");
        assert!(validate_path_segments("").is_ok(), "the alias alone");
    }

    #[test]
    fn endpoint_selection_honours_a_memoized_target_host_policy() {
        // A pool that needs the header, with the flag supplied by a caller that
        // memoized the derivation.
        let pool = make_upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);
        let err = select_endpoint_with_policy(&pool, true, None, &mut || 0)
            .expect_err("the memoized flag decides");
        assert_eq!(err.kind(), crate::error::OagwErrorKind::MissingTargetHost);

        // The same pool with a stale/derived-false flag round-robins: the flag
        // is trusted, which is why it must come from the same record.
        let selected = select_endpoint_with_policy(&pool, false, None, &mut || 0)
            .expect("the flag is the caller's decision");
        assert_eq!(selected.host, "us.vendor.com");

        // A single-endpoint upstream ignores the flag: one endpoint is never
        // ambiguous.
        let single = make_upstream("api.vendor.com", &["api.vendor.com"]);
        let selected = select_endpoint_with_policy(&single, true, None, &mut || 0)
            .expect("a single endpoint needs no header");
        assert_eq!(selected.host, "api.vendor.com");

        // An explicit target host is still validated whatever the flag says.
        let err = select_endpoint_with_policy(&single, true, Some("nope.example.com"), &mut || 0)
            .expect_err("unknown host");
        assert_eq!(err.kind(), crate::error::OagwErrorKind::UnknownTargetHost);
    }

    #[test]
    fn query_allowlist_rejects_unknown_parameters() {
        let err = validate_query(&["api-version".to_owned()], &["debug".to_owned()])
            .expect_err("unknown parameter");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::ValidationError);
        assert!(err.detail().contains("debug"));
    }

    #[test]
    fn query_allowlist_is_empty_means_none_allowed() {
        let err = validate_query(&[], &["api-version".to_owned()]).expect_err("none allowed");
        assert_eq!(err.status().as_u16(), 400);

        assert!(validate_query(&[], &[]).is_ok(), "no parameters is fine");
        assert!(validate_query(&["api-version".to_owned()], &["api-version".to_owned()]).is_ok());
    }

    #[test]
    fn endpoint_selection_routes_a_single_endpoint_without_a_header() {
        let upstream = make_upstream("api.openai.com", &["api.openai.com"]);
        let mut calls = 0;

        let selected = select_endpoint(&upstream, None, &mut || {
            calls += 1;
            0
        })
        .expect("single endpoint");

        assert_eq!(selected.host, "api.openai.com");
        assert_eq!(calls, 0, "no round-robin for a single endpoint");
    }

    #[test]
    fn endpoint_selection_validates_an_explicit_header_on_any_upstream() {
        let upstream = make_upstream("api.openai.com", &["api.openai.com"]);

        let selected = select_endpoint(&upstream, Some("Api.OpenAI.COM."), &mut || 0)
            .expect("the header is validated, not required");
        assert_eq!(selected.host, "api.openai.com");

        let err = select_endpoint(&upstream, Some("other.example.com"), &mut || 0)
            .expect_err("unknown host");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::UnknownTargetHost);
        assert_eq!(
            err.extensions()
                .get("invalid_value")
                .and_then(|v| v.as_str()),
            Some("other.example.com")
        );
    }

    #[test]
    fn endpoint_selection_requires_the_header_for_common_suffix_pools() {
        let upstream = make_upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);

        let err = select_endpoint(&upstream, None, &mut || 0).expect_err("header required");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), crate::error::OagwErrorKind::MissingTargetHost);
        assert_eq!(
            err.extensions()
                .get("valid_hosts")
                .and_then(|value| value.as_array())
                .map(Vec::len),
            Some(2)
        );

        let selected = select_endpoint(&upstream, Some("eu.vendor.com"), &mut || 0)
            .expect("the header disambiguates");
        assert_eq!(selected.host, "eu.vendor.com");
    }

    #[test]
    fn endpoint_selection_round_robins_explicit_multi_endpoint_pools() {
        let upstream = make_upstream("my-service", &["10.0.1.1", "10.0.1.2"]);

        for (turn, expected) in ["10.0.1.1", "10.0.1.2", "10.0.1.1"].into_iter().enumerate() {
            let selected = select_endpoint(&upstream, None, &mut || turn).expect("round robin");
            assert_eq!(selected.host, expected);
        }
    }

    #[test]
    fn endpoint_selection_rejects_a_malformed_target_host() {
        let upstream = make_upstream("vendor.com", &["us.vendor.com", "eu.vendor.com"]);

        for value in ["us.vendor.com:8443", "us.vendor.com/path", "not a host", ""] {
            let err = select_endpoint(&upstream, Some(value), &mut || 0).expect_err(value);
            assert_eq!(err.status().as_u16(), 400, "{value}");
            assert_eq!(
                err.kind(),
                crate::error::OagwErrorKind::InvalidTargetHost,
                "{value}"
            );
            assert_eq!(
                err.extensions()
                    .get("invalid_value")
                    .and_then(|v| v.as_str()),
                Some(value),
                "the rejected value is reported"
            );
        }

        // IP literals are legitimate target hosts.
        let ips = make_upstream("my-service", &["10.0.1.1", "10.0.1.2"]);
        let selected = select_endpoint(&ips, Some("10.0.1.2"), &mut || 0).expect("ip literal");
        assert_eq!(selected.host, "10.0.1.2");
    }

    #[test]
    fn grpc_upstreams_are_reported_unmatchable() {
        let mut grpc = make_upstream("svc.local", &["svc.local"]);
        grpc.spec.protocol = crate::domain::types::Protocol::Grpc;

        assert!(!is_http_upstream(&grpc));
        assert!(is_http_upstream(&make_upstream(
            "api.openai.com",
            &["api.openai.com"]
        )));
    }
}
