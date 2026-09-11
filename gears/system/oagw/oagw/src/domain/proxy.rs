//! The pure proxy data-plane rules
//! (`cpt-cf-oagw-feature-proxy-pipeline`).
//!
//! The module holds the four decision cores the pipeline drives — route
//! matching, endpoint selection, the built-in CORS check and the proxy-context
//! state machine — together with the value objects those decisions produce and
//! with the header, body and scheme rules of `cpt-cf-oagw-dod-request-validation`.
//!
//! Everything here is pure: no I/O, no clock, no network. The inputs are the
//! `EffectiveConfig`, the route tier and the selected upstream the resolution
//! handed over, plus the request the API handler classified, and the outputs
//! are the decisions the pipeline renders. The pipeline in
//! `crate::infra::proxy::pipeline` is the caller that orders these decisions
//! and performs the outbound call; nothing in this module knows about it.
// @cpt-begin:cpt-cf-oagw-dod-request-validation:p1:inst-full
// The inbound-validation contract of `cpt-cf-oagw-dod-request-validation`:
// the method allowlist is part of the match key, a query parameter outside
// `query_allowlist` and a suffix offered to a `path_suffix_mode: disabled`
// route are 400 `ValidationError`, a body above the limit is 413
// `PayloadTooLarge` before it is buffered, a `Transfer-Encoding` other than
// `chunked`, a CR or LF in a header value and a `Content-Length` carried
// together with a `Transfer-Encoding` are 400 `ValidationError`, the SSRF
// posture runs behind `ssrf_policy.enabled` and a plaintext upstream
// connection is refused unless `allow_http_upstream` is `true`.
// @cpt-begin:cpt-cf-oagw-dod-endpoint-selection-contract:p1:inst-full
// The six-row `X-OAGW-Target-Host` Behavior Matrix of ADR 0001, the form
// validation that runs before every pool comparison, the per-pool round-robin
// cursor and the consumption of the routing header are all decided here, where
// no I/O can hide behind them.

use std::net::IpAddr;

use http::header::{HeaderMap, HeaderName, HeaderValue};
use uuid::Uuid;

use crate::domain::alias::compute_derived_alias;
use crate::domain::error::OagwError;
use crate::domain::model::{
    CORS_WILDCARD_ORIGIN, CorsConfig, DEFAULT_ENDPOINT_SCHEME, Endpoint, EndpointScheme, HttpMatch,
    PASSTHROUGH_ALLOWLIST, PASSTHROUGH_NONE, PATH_SUFFIX_DISABLED, PROTOCOL_HTTP, RequestHeaders,
    ResponseHeaders, Route, SCHEME_HTTP, SCHEME_HTTPS, SCHEME_WSS, Upstream, strip_trailing_dot,
};
use crate::domain::resolution::RouteTier;
use crate::domain::streaming::carve_out_keeps;

/// The proxy request the API handler classified (`inst-pf-01`), as the pure
/// input every decision of this module reads.
#[derive(Debug, Clone)]
pub struct ProxyRequest {
    /// The request method, as the routing layer classified it.
    pub method: String,
    /// The normalized alias the alias-resolution feature resolved.
    pub alias: String,
    /// The path the proxy URL carried beyond `/oagw/v1/proxy/{alias}`.
    pub path_suffix: String,
    /// The query parameters in inbound order, carried verbatim.
    pub query: Vec<QueryParam>,
    /// The inbound headers, as the request carried them.
    pub headers: HeaderMap,
    /// The size the inbound body reached, `0` when the request carried none.
    pub body_len: u64,
}

/// One inbound query parameter, carried byte for byte.
///
/// The proxy neither decodes nor re-encodes a query string: `name` is what the
/// `query_allowlist` compares against and `raw` is the `name[=value]` form the
/// outbound request re-emits, so a parameter the route allows reaches the
/// upstream exactly as the client wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryParam {
    /// The parameter name, as the request wrote it.
    pub name: String,
    /// The verbatim `name[=value]` pair.
    pub raw: String,
}

/// Splits a raw query string into its parameters in inbound order.
///
/// The split is the byte-level one the proxy performs: parameters are
/// separated by `&` and a name from its value by the first `=`. Nothing is
/// percent-decoded, so an encoded name never becomes a different allowlist key.
#[must_use]
pub fn parse_query(raw: &str) -> Vec<QueryParam> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| QueryParam {
            name: pair.split('=').next().unwrap_or(pair).to_owned(),
            raw: pair.to_owned(),
        })
        .collect()
}

/// The reason `cpt-cf-oagw-algo-route-matching` produced no matched route.
///
/// `NoRoute` is rendered 404 `RouteNotFound` and `Content` 400
/// `ValidationError`; `NotProxied` is rendered with the gateway
/// `RouteError`/`ProtocolError` semantics a non-proxied protocol or scheme
/// answers with (`inst-rm-03`, `inst-pf-30`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchRejection {
    /// The upstream protocol is not HTTP, so no HTTP match is attempted.
    NotProxied,
    /// No route of the tier matched, including a method in no allowlist.
    NoRoute,
    /// A route matched and rejected the request content.
    Content(&'static str),
}

/// The rule a matched route rejected the request content on.
pub const QUERY_ALLOWLIST_RULE: &str = "query_allowlist";
/// The rule a matched route rejected the request content on.
pub const PATH_SUFFIX_RULE: &str = "path_suffix_mode";

/// The matched route with the outbound path and query it composes
/// (`inst-rm-13`, `inst-rm-15`).
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The route that matched, as the resolution handed it over.
    pub route: Route,
    /// The outbound path: the base path with the suffix appended when
    /// `path_suffix_mode` is `append`, the base path alone when it is
    /// `disabled`.
    pub outbound_path: String,
    /// The outbound query string: the allowlisted subset of the inbound one, in
    /// inbound order.
    pub outbound_query: Vec<QueryParam>,
}

/// One candidate the walk recorded before the ordering step.
struct Candidate {
    /// The index of the tier the route came from, the selected upstream's own
    /// tier being `0`.
    tier_index: usize,
    /// The matched route.
    route: Route,
    /// The matched `match.http.path` prefix.
    base: String,
    /// The path beyond that prefix, appended when the mode allows it.
    remainder: String,
    /// Whether `path_suffix_mode` appends the suffix.
    appended: bool,
}

/// Matches a route through `cpt-cf-oagw-algo-route-matching`.
///
/// The tier arrives in the order the resolution fixed at `inst-ef-08`: the
/// selected upstream's own routes first, then the inherited ancestor routes.
/// The walk is in memory, deterministic and free of repository reads, and the
/// same tier, method, suffix and query always produce the same route.
///
/// # Errors
/// Returns [`MatchRejection::NotProxied`] when the upstream protocol is not
/// HTTP, [`MatchRejection::NoRoute`] when no route of the tier matched and
/// [`MatchRejection::Content`] when a matched route rejected the request on its
/// `path_suffix_mode` or its `query_allowlist`.
pub fn match_route(
    protocol: &str,
    tier: &[RouteTier],
    method: &str,
    path_suffix: &str,
    query: &[QueryParam],
) -> Result<MatchedRoute, MatchRejection> {
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-01
    // Take the route tier, the method, the path suffix, the query string and
    // the upstream `protocol` as the inputs: the match strategy is selected by
    // the protocol, with `Content-Type` never used to select it.
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-01
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-02
    // IF the upstream `protocol` is not HTTP — `grpc` and `wt` being legal
    // configuration values — no HTTP match is attempted and no gRPC match key
    // is ever read.
    if protocol != PROTOCOL_HTTP {
        // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-03
        // Report the not-proxied outcome and stop: the caller answers with the
        // gateway `RouteError`/`ProtocolError` problem+json semantics, the
        // gRPC proxy code path being absent this release.
        return Err(MatchRejection::NotProxied);
        // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-03
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-02

    let mut candidates: Vec<Candidate> = Vec::new();
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-04
    // FOR EACH route of the tier in the fixed order, skipping a route whose
    // `enabled` flag is `false`: a disabled route is excluded from matching.
    for (tier_index, route_tier) in tier.iter().enumerate() {
        for route in &route_tier.routes {
            if !route.enabled {
                continue;
            }
            let Some(http) = route.match_config.as_ref().and_then(|m| m.http.as_ref()) else {
                continue;
            };
            match accept(route, http, tier_index, method, path_suffix, query) {
                Step::Skip => continue,
                Step::Record(candidate) => candidates.push(*candidate),
                Step::Reject(rule) => return Err(MatchRejection::Content(rule)),
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-04

    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-14
    // Order the recorded candidates by longest matching path prefix first, then
    // by the tier order the resolution fixed — a descendant route over an
    // inherited ancestor route at the same prefix — then by the greater
    // `priority` value, and take the first. The persist-time invariant that no
    // two enabled routes under the same upstream share `(path_prefix,
    // priority)` for the same method makes the order total.
    candidates.sort_by(|left, right| {
        let prefix = |candidate: &Candidate| candidate.base.len();
        prefix(right)
            .cmp(&prefix(left))
            .then(left.tier_index.cmp(&right.tier_index))
            .then(right.route.priority.cmp(&left.route.priority))
    });
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-14
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-17
    let Some(selected) = candidates.first() else {
        // RETURN the reason no route matched to the pipeline: no route of the
        // tier matched — including a method in no candidate's allowlist — so
        // the caller renders 404 `RouteNotFound`.
        return Err(MatchRejection::NoRoute);
    };
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-15
    // Compose the outbound path from the matched route: the base path with the
    // suffix appended when `path_suffix_mode` is `append`, the suffix alone
    // never being sent, and the outbound query string being the allowlisted
    // subset of the inbound one in inbound order — the walk having already
    // rejected any parameter outside the allowlist, the subset is the inbound
    // string itself.
    let http = selected
        .route
        .match_config
        .as_ref()
        .and_then(|m| m.http.as_ref());
    let outbound_path = if selected.appended {
        format!("{}{}", selected.base, selected.remainder)
    } else {
        selected.base.clone()
    };
    let allowlist = http.map_or(&[][..], |http| http.query_allowlist.as_slice());
    let outbound_query = query
        .iter()
        .filter(|param| allowlist.iter().any(|allowed| allowed == &param.name))
        .cloned()
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-15
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-16
    // The match is deterministic and in memory: the tier is a value the
    // resolution handed over, no repository read and no tenant walk happens
    // here, and the same tier, method, suffix and query always produce the same
    // route.
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-16
    let matched = MatchedRoute {
        route: selected.route.clone(),
        outbound_path,
        outbound_query,
    };
    // RETURN the matched route, its outbound path and its outbound query
    // string, or the reason — no route matched, or a route-level rejection — to
    // the pipeline.
    Ok(matched)
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-17
}

/// One route's verdict on the request, in the order the walk visits the rules.
enum Step {
    /// The route does not take the request; the walk continues.
    Skip,
    /// The route takes the request and becomes a candidate.
    Record(Box<Candidate>),
    /// The route takes the request and rejects its content, stopping the walk.
    Reject(&'static str),
}

/// Visits one route's rules in the order the walk fixes: the method allowlist,
/// the path prefix, the `path_suffix_mode` and the `query_allowlist`.
fn accept(
    route: &Route,
    http: &HttpMatch,
    tier_index: usize,
    method: &str,
    path_suffix: &str,
    query: &[QueryParam],
) -> Step {
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-05
    // IF the request method is not in the route's `match.http.methods` — the
    // allowlist the route schema constrains to GET, POST, PUT, DELETE and
    // PATCH — the method is part of the match key and not a post-match guard.
    if !http.methods.iter().any(|allowed| allowed == method) {
        // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-06
        // Skip the route and continue the walk: the request leaves no matched
        // route behind and is answered 404 `RouteNotFound`, not 400.
        return Step::Skip;
        // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-06
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-05
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-07
    // ELSE IF the route's `match.http.path` is not a prefix of the path suffix
    // the caller handed over, under the longest-prefix-wins rule.
    let base = http.path.as_deref().unwrap_or("");
    if !path_suffix.starts_with(base) {
        // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-08
        // Skip the route and continue the walk.
        return Step::Skip;
        // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-08
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-07
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-09
    // ELSE IF `path_suffix_mode` is `disabled` and the request carried a path
    // suffix beyond the matched prefix.
    if http.path_suffix_mode == PATH_SUFFIX_DISABLED && path_suffix.len() > base.len() {
        // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-10
        // Report the route-level rejection — 400 `ValidationError` — and stop
        // the walk: the suffix was offered to a route that forbids it.
        return Step::Reject(PATH_SUFFIX_RULE);
        // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-10
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-09
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-11
    // ELSE IF the request carries a query parameter whose name is not in
    // `query_allowlist`, an empty allowlist allowing none.
    if query.iter().any(|param| {
        !http
            .query_allowlist
            .iter()
            .any(|allowed| allowed == &param.name)
    }) {
        // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-12
        // Report the route-level rejection — 400 `ValidationError` — and stop
        // the walk, the unlisted parameter never reaching the outbound query
        // string.
        return Step::Reject(QUERY_ALLOWLIST_RULE);
        // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-12
    }
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-11
    // @cpt-begin:cpt-cf-oagw-algo-route-matching:p1:inst-rm-13
    // ELSE the route matches: record it, its `match.http.path` as the outbound
    // base path, and whether the suffix is appended (`append`, the schema
    // default) or the base path is used alone (`disabled`).
    Step::Record(Box::new(Candidate {
        tier_index,
        route: route.clone(),
        base: base.to_owned(),
        remainder: path_suffix[base.len()..].to_owned(),
        appended: http.path_suffix_mode != PATH_SUFFIX_DISABLED,
    }))
    // @cpt-end:cpt-cf-oagw-algo-route-matching:p1:inst-rm-13
}

/// The alias kind `cpt-cf-oagw-algo-endpoint-selection` reads: whether the
/// upstream's alias was derived from its endpoint pool, which is what gives it
/// a registrable common suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    /// The alias was supplied, so no common suffix is registrable from it.
    Explicit,
    /// The alias was derived from the pool, so it carries a common suffix.
    CommonSuffix,
}

impl AliasKind {
    /// Classifies the alias of a stored upstream, reading the alias kind the
    /// alias-resolution feature fixed at create time instead of re-deriving it.
    #[must_use]
    pub fn of_upstream(upstream: &Upstream) -> Self {
        // The pool is borrowed, not cloned: the classification reads it once
        // and no endpoint is ever copied for it.
        let derived = compute_derived_alias(endpoints(upstream));
        match derived {
            Ok(alias) if upstream.alias.as_deref() == Some(alias.as_str()) => Self::CommonSuffix,
            _ => Self::Explicit,
        }
    }
}

/// The endpoint pool of an upstream, empty when the upstream carries none.
fn endpoints(upstream: &Upstream) -> &[Endpoint] {
    upstream
        .server
        .as_ref()
        .map_or(&[][..], |server| server.endpoints.as_slice())
}

/// The `selection_method` the routing metrics name (`inst-es-17`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// The pool's single endpoint was taken with no header and no change.
    Default,
    /// The next endpoint of the pool was taken round-robin.
    RoundRobin,
    /// `X-OAGW-Target-Host` named the endpoint.
    ExplicitHeader,
}

impl SelectionMethod {
    /// The closed name of the method, the value the metrics label carries.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::RoundRobin => "round_robin",
            Self::ExplicitHeader => "explicit_header",
        }
    }
}

/// The typed failure of `cpt-cf-oagw-algo-endpoint-selection`, one row of the
/// closed mapping table each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionFailure {
    /// `X-OAGW-Target-Host` is required and absent.
    Missing,
    /// `X-OAGW-Target-Host` is present but malformed.
    Invalid,
    /// `X-OAGW-Target-Host` names no endpoint of this request's own pool.
    Unknown,
}

/// The selected endpoint together with the selection method it was taken by.
#[derive(Debug, Clone)]
pub struct SelectedEndpoint {
    /// The endpoint of the pool the request reaches.
    pub endpoint: Endpoint,
    /// How the endpoint was selected, for the routing metrics.
    pub method: SelectionMethod,
}

/// Selects the outbound endpoint through `cpt-cf-oagw-algo-endpoint-selection`.
///
/// The six rows of ADR 0001's matrix are covered by the three pool shapes each
/// carrying the header present or absent. The round-robin cursor is held by the
/// caller, one per pool, and is the only state the selection reads across
/// requests.
///
/// # Errors
/// Returns [`SelectionFailure::Missing`], [`SelectionFailure::Invalid`] or
/// [`SelectionFailure::Unknown`] for the caller to render as the
/// `MissingTargetHost`, `InvalidTargetHost` or `UnknownTargetHost` row.
pub fn select_endpoint(
    pool: &[Endpoint],
    kind: AliasKind,
    header: Option<&str>,
    cursor: &mut u64,
) -> Result<SelectedEndpoint, SelectionFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-01
    // Take the endpoint pool, the alias kind and the header value as the
    // inputs: the pool's homogeneity in `protocol`, `scheme` and `port` was
    // validated at persist time and is relied on here without being re-checked.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-01
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-02
    // Classify the request into one of the six rows of the behavior matrix:
    // single endpoint, multi-endpoint with an explicit alias, or multi-endpoint
    // with a common-suffix alias, each with the header present or absent.
    let selected = if pool.len() == 1 {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-03
        // IF the pool holds exactly one endpoint.
        match header {
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-04
                // The header is absent: select it, the row that routes to the
                // endpoint with no change.
                Ok(pool[0].clone())
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-04
            }
            Some(value) => {
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-05
                // The header is present: validate it and require it to name
                // that endpoint, the "optional but validated if present" row.
                named_endpoint(pool, value)
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-05
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-03
    } else if kind == AliasKind::Explicit {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-06
        // ELSE IF the pool holds several endpoints and the alias is explicit,
        // having no registrable common suffix.
        match header {
            None => {
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-07
                // The header is absent: select the next endpoint of the pool
                // round-robin and never reject for a missing header, the load
                // balancing being the pool's whole purpose.
                round_robin(pool, cursor)
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-07
            }
            Some(value) => {
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-08
                // The header is present: match it against the pool's endpoint
                // hosts and select the matching endpoint, bypassing load
                // balancing.
                named_endpoint(pool, value)
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-08
            }
        }
        // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-06
    } else {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-09
        // ELSE the pool holds several endpoints and the alias is a
        // common-suffix alias.
        match header {
            None => {
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-09
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-10
                // The header is absent: reject with 400 `MissingTargetHost`,
                // the header being required to disambiguate the target and no
                // default being invented.
                return Err(SelectionFailure::Missing);
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-10
            }
            Some(value) => {
                // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-11
                // The header is present: match it against the pool's endpoint
                // hosts and select the matching endpoint.
                named_endpoint(pool, value)
                // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-11
            }
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-02
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-15
    // The round-robin cursor is per-pool in-memory state of the data plane,
    // never persisted and never shared between pools, so its position after a
    // restart is unobservable and no two upstreams advance one another's
    // cursor.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-15
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-16
    // Consume the header: the value that routed this request is stripped by the
    // header stage and never reaches the upstream, and no header other than
    // `X-OAGW-Target-Host` influences the selection.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-16
    let method = if header.is_some() {
        SelectionMethod::ExplicitHeader
    } else if pool.len() == 1 {
        SelectionMethod::Default
    } else {
        SelectionMethod::RoundRobin
    };
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-17
    // RETURN the selected endpoint — host, port and scheme — with its selection
    // method, or the typed failure, to the pipeline.
    Ok(SelectedEndpoint {
        endpoint: selected?,
        method,
    })
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-17
}

/// Matches a present `X-OAGW-Target-Host` against the request's own pool.
fn named_endpoint(pool: &[Endpoint], value: &str) -> Result<Endpoint, SelectionFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-12
    // Validate the header's form whenever it is present and consumed: it must
    // be a hostname or an IP address, with no port, no path and no special
    // characters, and a malformed value is rejected before the pool comparison,
    // never after it.
    let Some(host) = target_host_name(value) else {
        return Err(SelectionFailure::Invalid);
    };
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-12
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-13
    // IF the header is present, well formed, and names no endpoint of this
    // pool.
    let found = pool.iter().find(|endpoint| {
        endpoint.host_stripped().is_some_and(|candidate| {
            // The pool side is read in the form the header value was
            // normalized to: an IPv6 endpoint host is canonicalized before the
            // comparison, so any spelling of the same address still names it.
            match ip_literal(candidate) {
                Some(ip) => ip.to_string() == host,
                None => candidate.eq_ignore_ascii_case(&host),
            }
        })
    });
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-13
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-14
    // Reject with 400 `UnknownTargetHost`: the value is matched against this
    // request's own pool and never against another upstream's.
    found.cloned().ok_or(SelectionFailure::Unknown)
    // @cpt-end:cpt-cf-oagw-algo-endpoint-selection:p1:inst-es-14
}

/// Takes the next endpoint of the pool round-robin, advancing the pool cursor.
fn round_robin(pool: &[Endpoint], cursor: &mut u64) -> Result<Endpoint, SelectionFailure> {
    let index = usize::try_from(*cursor % pool.len() as u64).unwrap_or(0);
    *cursor = cursor.wrapping_add(1);
    Ok(pool[index].clone())
}

/// The normalized host a well-formed `X-OAGW-Target-Host` value names.
///
/// An IP literal is accepted in either form and normalized to its canonical
/// text — `2001:db8::1` and `[2001:0db8:0000::1]` naming the same endpoint —
/// while a hostname keeps the label rules it always had and is lower-cased.
fn target_host_name(value: &str) -> Option<String> {
    let host = strip_trailing_dot(value);
    if let Some(ip) = ip_literal(host) {
        return Some(ip.to_string());
    }
    if host.is_empty() || host.contains('/') || host.contains(':') || host.len() > 253 {
        return None;
    }
    let well_formed = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    });
    well_formed.then(|| host.to_ascii_lowercase())
}

/// The IP address a bracketed or bare literal names, `None` for a hostname.
///
/// The brackets are the form a host is carried in when it is not to be confused
/// with a port, and they are removed before the value is judged. A dotted quad
/// must be exactly four decimal octets, each in range, which is the form an
/// address is written in and leaves `127.1` and `2130706433` out of it.
fn ip_literal(value: &str) -> Option<IpAddr> {
    let bare = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    if bare.contains(':') {
        return bare.parse::<IpAddr>().ok();
    }
    let octets = bare.split('.').map(octet_of).collect::<Option<Vec<_>>>();
    match octets {
        Some(octets) if octets.len() == 4 => bare.parse::<IpAddr>().ok(),
        _ => None,
    }
}

/// The decimal value of one octet spelling, `None` for an empty or lettered
/// label and for one out of range.
fn octet_of(label: &str) -> Option<u8> {
    if label.is_empty() || !label.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    label.parse::<u8>().ok()
}

/// The maximum age the permissive 204 carries (ADR 0004).
pub const PREFLIGHT_MAX_AGE: u64 = 86_400;
/// The `Vary` value of the preflight 204 (ADR 0004).
pub const VARY_PREFLIGHT: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
/// The `Vary` value an actual cross-origin response carries (ADR 0004).
pub const VARY_ORIGIN: &str = "Origin";

/// The CORS inputs of one request (`inst-cc-01`): the method, the `Origin`
/// header and the two preflight headers where present.
#[derive(Debug, Clone, Copy)]
pub struct CorsCheck<'a> {
    /// The request method.
    pub method: &'a str,
    /// The `Origin` header, absent on a same-origin request.
    pub origin: Option<&'a str>,
    /// The `Access-Control-Request-Method` header of a preflight.
    pub request_method: Option<&'a str>,
    /// The `Access-Control-Request-Headers` header of a preflight.
    pub request_headers: Option<&'a str>,
}

/// The headers the permissive 204 carries (`inst-cc-03`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightHeaders {
    /// `Access-Control-Allow-Origin`, echoing the requested origin.
    pub allow_origin: String,
    /// `Access-Control-Allow-Methods`, echoing the requested method.
    pub allow_methods: String,
    /// `Access-Control-Allow-Headers`, echoing the requested headers.
    pub allow_headers: String,
    /// `Access-Control-Max-Age`.
    pub max_age: u64,
    /// The `Vary` value.
    pub vary: &'static str,
}

/// The CORS response headers an allowed actual request carries
/// (`inst-cc-12`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedHeaders {
    /// `Access-Control-Allow-Origin`, the matched origin or the wildcard.
    pub allow_origin: String,
    /// `Access-Control-Expose-Headers`, from `expose_headers`.
    pub expose_headers: Option<String>,
    /// `Access-Control-Allow-Credentials`, present when `allow_credentials`.
    pub allow_credentials: bool,
}

/// The outcome of `cpt-cf-oagw-flow-cors-check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsOutcome {
    /// The preflight branch: the 204 the caller answers without resolving
    /// anything (`inst-cc-04`).
    Preflight(PreflightHeaders),
    /// No CORS applies: the request continues with no CORS work.
    NotApplicable,
    /// The actual request is allowed and continues (`inst-cc-12`).
    Allowed(AllowedHeaders),
    /// The origin is not in `allowed_origins` (`inst-cc-09`).
    OriginRejected,
    /// The method is not in `allowed_methods` (`inst-cc-11`).
    MethodRejected,
}

/// Reports whether the request is a CORS preflight (`inst-cc-02`).
#[must_use]
pub fn is_preflight(method: &str, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method.eq_ignore_ascii_case("OPTIONS") && origin.is_some() && request_method.is_some()
}

/// Runs `cpt-cf-oagw-flow-cors-check`.
///
/// The preflight branch is permissive: it echoes the requested origin, method
/// and headers and defers origin and method validation to the actual request.
/// The actual-request branch checks the origin first and the method second,
/// which is the order ADR 0004 fixes.
#[must_use]
pub fn cors_check(block: Option<&CorsConfig>, request: CorsCheck) -> CorsOutcome {
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-01
    // Receive the request with its method, its `Origin` header, its
    // `Access-Control-Request-Method` and `Access-Control-Request-Headers`
    // headers where present, and the CORS block the `EffectiveConfig` carries.
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-01
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-02
    // IF the request is a preflight — method `OPTIONS` with an `Origin` header
    // and an `Access-Control-Request-Method` header.
    if is_preflight(request.method, request.origin, request.request_method) {
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-03
        // Return 204 No Content echoing the request's `Origin`, the requested
        // method and the requested headers, with `Access-Control-Max-Age` and
        // `Vary`, performing no upstream resolution, no tenant-context lookup,
        // no route match and no per-request auth or plugin check.
        let preflight = PreflightHeaders {
            allow_origin: request.origin.unwrap_or_default().to_owned(),
            allow_methods: request.request_method.unwrap_or_default().to_owned(),
            allow_headers: request.request_headers.unwrap_or_default().to_owned(),
            max_age: PREFLIGHT_MAX_AGE,
            vary: VARY_PREFLIGHT,
        };
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-03
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-04
        // RETURN the 204 with `X-OAGW-Error-Source: gateway` to the caller,
        // which returns it without entering the pipeline further.
        return CorsOutcome::Preflight(preflight);
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-04
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-02
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-05
    // ELSE IF the request carries no `Origin` header, or the `EffectiveConfig`
    // carries no CORS block with `enabled: true`.
    let Some(block) = block else {
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-06
        // Report that no CORS applies: the request continues with no origin
        // check, no method check and no `Access-Control-*` response header,
        // `Vary: Origin` being added by the caller.
        return CorsOutcome::NotApplicable;
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-06
    };
    if block.enabled != Some(true) {
        return CorsOutcome::NotApplicable;
    }
    let Some(origin) = request.origin else {
        return CorsOutcome::NotApplicable;
    };
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-05
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-07
    // ELSE match the request's `Origin` against the CORS block's
    // `allowed_origins` by exact, port-sensitive and protocol-sensitive string
    // comparison, with no regex pattern anywhere in the comparison.
    if !origin_allowed(block, origin) {
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-08
        // IF the origin is not in the list.
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-08
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-09
        // Reject with 403 `CorsOriginNotAllowed`, `detail` naming the rejected
        // origin, through the existing row, with no plugin having run and no
        // forwarding having taken place.
        return CorsOutcome::OriginRejected;
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-09
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-07
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-10
    // ELSE IF the request method is not in the CORS block's
    // `allowed_methods`.
    if !block
        .allowed_methods
        .iter()
        .any(|allowed| allowed == request.method)
    {
        // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-11
        // Reject with 403 `CorsMethodNotAllowed`, `detail` naming the rejected
        // method, at the same point and with the same consequences.
        return CorsOutcome::MethodRejected;
        // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-11
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-10
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-12
    // ELSE the request continues: the caller attaches
    // `Access-Control-Allow-Origin` echoing the matched origin,
    // `Access-Control-Expose-Headers` from `expose_headers` and
    // `Access-Control-Allow-Credentials` when `allow_credentials` is `true`,
    // together with `Vary: Origin`.
    let outcome = CorsOutcome::Allowed(AllowedHeaders {
        allow_origin: allowed_origin_value(block, origin),
        expose_headers: comma_separated(&block.expose_headers),
        allow_credentials: block.allow_credentials,
    });
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-12
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-13
    // The credential restriction is a configuration invariant and not a
    // runtime check: `allow_credentials` with a wildcard origin is rejected
    // when the upstream or route is stored, so no CORS block this flow can ever
    // read carries that combination, and no branch exists here to handle it.
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-13
    // @cpt-begin:cpt-cf-oagw-flow-cors-check:p1:inst-cc-14
    // RETURN the outcome — continue, or one of the two 403 rows — to the
    // pipeline, which either drives the next stage or renders the rejection.
    outcome
    // @cpt-end:cpt-cf-oagw-flow-cors-check:p1:inst-cc-14
}

/// Reports whether an origin is in the allowed list.
///
/// The wildcard `*` matches any origin; every other entry is compared exactly,
/// so a differing port or protocol never matches.
fn origin_allowed(block: &CorsConfig, origin: &str) -> bool {
    block
        .allowed_origins
        .iter()
        .any(|allowed| allowed == CORS_WILDCARD_ORIGIN || allowed == origin)
}

/// The `Access-Control-Allow-Origin` value an allowed request carries.
fn allowed_origin_value(block: &CorsConfig, origin: &str) -> String {
    if block
        .allowed_origins
        .iter()
        .any(|allowed| allowed == CORS_WILDCARD_ORIGIN)
    {
        CORS_WILDCARD_ORIGIN.to_owned()
    } else {
        origin.to_owned()
    }
}

/// Joins a list the way a comma-separated header value writes it.
fn comma_separated(values: &[String]) -> Option<String> {
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

/// The inbound header names the gateway never forwards (`inst-pf-20`): the
/// hop-by-hop list of `cpt-cf-oagw-fr-header-transform`.
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The routing header the pipeline consumes and never forwards
/// (`inst-pf-20`).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The inbound header the gateway replaces with the selected endpoint's
/// authority.
pub const HOST_HEADER: &str = "host";

/// The inbound header the outbound bridge recomputes from the body it streams,
/// so a stale declared length can never contradict the forwarded one.
pub const CONTENT_LENGTH_HEADER: &str = "content-length";

/// The prefix whose carve-out belongs to `cpt-cf-oagw-feature-streaming-proxy`
/// and is referenced here without being implemented.
pub const SEC_WEBSOCKET_PREFIX: &str = "sec-websocket-";

/// Builds the outbound request header set (`inst-pf-20`).
///
/// The routing and hop-by-hop headers are consumed or stripped first, the
/// passthrough rule then selects which inbound headers form the forwarded base
/// set, `Host` is replaced with the selected endpoint's authority and the
/// `request.*` rules are applied over that set in the order set, add, remove.
/// Reading the passthrough step as the selection of the forwarded base keeps a
/// `remove` rule from being undone by the header it removes, which is the
/// reading that lets the four rules compose.
///
/// The form carries no upgrade exemption: this is the set every proxied
/// request but a WebSocket upgrade is built with.
#[must_use]
pub fn prepare_request_headers(
    inbound: &HeaderMap,
    authority: &str,
    rules: Option<&RequestHeaders>,
) -> HeaderMap {
    prepare_headers(inbound, authority, rules, false)
}

/// Builds the outbound request header set of a WebSocket upgrade request.
///
/// The set is built by the same stage in the same order as
/// [`prepare_request_headers`] — the passthrough selection, the `Host`
/// replacement and the `request.*` rules — with one difference: the carve-out
/// of `cpt-cf-oagw-algo-upgrade-header-carve-out` is applied so that `Upgrade`,
/// `Connection` and the `Sec-WebSocket-*` family survive the hop-by-hop strip
/// list. No other member of that list is exempted, and the exemption is a
/// request-side rule only: the 101's response headers are relayed as the RFC
/// 6455 exchange produced them and are never touched by this function.
#[must_use]
pub fn prepare_upgrade_request_headers(
    inbound: &HeaderMap,
    authority: &str,
    rules: Option<&RequestHeaders>,
) -> HeaderMap {
    prepare_headers(inbound, authority, rules, true)
}

/// The one builder both forms share, the `upgrade` flag being the only thing
/// that differs between them.
fn prepare_headers(
    inbound: &HeaderMap,
    authority: &str,
    rules: Option<&RequestHeaders>,
    upgrade: bool,
) -> HeaderMap {
    // @cpt-begin:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-03
    // Apply the carve-out through `cpt-cf-oagw-algo-upgrade-header-carve-out`
    // so that `Upgrade`, `Connection` and the `Sec-WebSocket-*` family survive
    // the hop-by-hop strip list of `inst-pf-20` and no other member of that
    // list is exempted: the exemption is read one header at a time by
    // `carve_out_keeps`, which the passthrough step below consults before it
    // consults the strip list itself.
    // @cpt-end:cpt-cf-oagw-flow-websocket-upgrade:p1:inst-ws-03
    let mut outbound = HeaderMap::new();
    // @cpt-begin:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-05
    // Keep the three exempt families in the outbound set whatever the
    // passthrough mode says: `Upgrade` so the upstream sees the upgrade token,
    // `Connection` so the upgrade's connection-level tokens survive and every
    // `Sec-WebSocket-*` header the RFC 6455 exchange needs. An upgrade that
    // forwarded none of them could never be proxied, which is the one reading
    // the exemption cannot allow, so the exemption rides ahead of the
    // passthrough selection below and the selection never appends them twice.
    if upgrade {
        for (name, value) in inbound.iter() {
            if carve_out_keeps(name.as_str(), true) {
                outbound.append(name, value.clone());
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-upgrade-header-carve-out:p1:inst-hc-05
    let mode = rules.map_or(PASSTHROUGH_NONE, |rules| rules.passthrough.as_str());
    match mode {
        PASSTHROUGH_ALLOWLIST => {
            let allowlist = rules.map_or(&[][..], |rules| rules.passthrough_allowlist.as_slice());
            for (name, value) in inbound.iter() {
                if !carve_out_keeps(name.as_str(), upgrade)
                    && forwardable(name.as_str(), upgrade)
                    && allowlist
                        .iter()
                        .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str()))
                {
                    outbound.append(name, value.clone());
                }
            }
        }
        PASSTHROUGH_NONE => {}
        // Every other value forwards the inbound set, the mode the schema
        // default and the `all` value both spell.
        _ => {
            for (name, value) in inbound.iter() {
                if !carve_out_keeps(name.as_str(), upgrade) && forwardable(name.as_str(), upgrade) {
                    outbound.append(name, value.clone());
                }
            }
        }
    }
    if let Ok(name) = HeaderName::from_bytes(HOST_HEADER.as_bytes())
        && let Ok(value) = HeaderValue::from_str(authority)
    {
        outbound.insert(name, value);
    }
    if let Some(rules) = rules {
        apply_request_rules(&mut outbound, rules);
    }
    outbound
}

/// Reports whether an inbound header is forwarded by the passthrough step.
///
/// The carve-out exemption is applied first, so a WebSocket upgrade keeps the
/// three families the exemption names and every other request is stripped
/// exactly as the stage has always stripped it.
fn forwardable(name: &str, upgrade: bool) -> bool {
    if carve_out_keeps(name, upgrade) {
        return true;
    }
    name != TARGET_HOST_HEADER
        && name != HOST_HEADER
        && name != CONTENT_LENGTH_HEADER
        && !HOP_BY_HOP_HEADERS.contains(&name)
        && !name.starts_with(SEC_WEBSOCKET_PREFIX)
}

/// Applies the `request.*` rules over the forwarded base, in the order set,
/// add, remove.
fn apply_request_rules(outbound: &mut HeaderMap, rules: &RequestHeaders) {
    for (name, value) in &rules.set {
        insert_header(outbound, name, value);
    }
    for (name, value) in &rules.add {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
            && let Ok(value) = HeaderValue::from_str(value)
        {
            outbound.append(name, value);
        }
    }
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            outbound.remove(name);
        }
    }
}

/// Sets one configured request header, skipping a name the header syntax
/// rejects — the persist-time validation of the domain-model feature having
/// already reported it.
fn insert_header(outbound: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
        && let Ok(value) = HeaderValue::from_str(value)
    {
        outbound.insert(name, value);
    }
}

/// Applies the `response.*` rules to the response the client receives
/// (`inst-pf-37`), in the order set, add, remove.
#[must_use]
pub fn apply_response_rules(head: &HeaderMap, rules: Option<&ResponseHeaders>) -> HeaderMap {
    let mut outbound = head.clone();
    if let Some(rules) = rules {
        for (name, value) in &rules.set {
            insert_header(&mut outbound, name, value);
        }
        for (name, value) in &rules.add {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes())
                && let Ok(value) = HeaderValue::from_str(value)
            {
                outbound.append(name, value);
            }
        }
        for name in &rules.remove {
            if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                outbound.remove(name);
            }
        }
    }
    outbound
}

/// Rejects an inbound request whose header values or body do not satisfy the
/// default validation rules of `cpt-cf-oagw-dod-request-validation`
/// (`inst-pf-21`).
///
/// The size limit is also enforced while the body is read, so a body above it
/// is never buffered: the `body_len` this function receives is the size the
/// read reported.
///
/// # Errors
/// Returns the `PayloadTooLarge` row for a body above `limit`, and the
/// `ValidationError` row for a header value carrying a control character, for a
/// `Content-Length` that is not a valid integer, that differs from another
/// occurrence of the header or that does not match the received size, for a
/// `Transfer-Encoding` other than `chunked` and for a `Content-Length` carried
/// together with a `Transfer-Encoding`.
pub fn validate_inbound(headers: &HeaderMap, body_len: u64, limit: u64) -> Result<(), OagwError> {
    for value in headers.values() {
        if header_value_allowed(value.as_bytes()) {
            continue;
        }
        return Err(OagwError::validation_error(
            "oagw.proxy: a request header value carries a control character",
        ));
    }
    let transfer_encoding = header_values(headers, "transfer-encoding");
    let mut declared: Option<u64> = None;
    // Every occurrence of the header is read, over its raw bytes: a request
    // that carries two, or one the `http` crate cannot even read as text, is a
    // request whose declared size is ambiguous, and ambiguity is not resolved
    // by taking whichever value came last.
    for raw in headers.get_all(CONTENT_LENGTH_HEADER) {
        let value = std::str::from_utf8(raw.as_bytes()).map_err(|_| {
            OagwError::validation_error(
                "oagw.proxy: the request carries a Content-Length that is not a valid integer",
            )
        })?;
        let parsed = value.parse::<u64>().map_err(|_| {
            OagwError::validation_error(
                "oagw.proxy: the request carries a Content-Length that is not a valid integer",
            )
        })?;
        if declared.is_some_and(|previous| previous != parsed) {
            return Err(OagwError::validation_error(
                "oagw.proxy: the request carries two Content-Length values that differ",
            ));
        }
        declared = Some(parsed);
    }
    if declared.is_some() && !transfer_encoding.is_empty() {
        return Err(OagwError::validation_error(
            "oagw.proxy: the request carries a Content-Length and a Transfer-Encoding",
        ));
    }
    for value in transfer_encoding {
        if !value.eq_ignore_ascii_case("chunked") {
            return Err(OagwError::validation_error(
                "oagw.proxy: the request carries a Transfer-Encoding other than chunked",
            ));
        }
    }
    if body_len > limit {
        return Err(OagwError::payload_too_large(
            "oagw.proxy: the request body exceeds the configured body limit",
        ));
    }
    if let Some(declared) = declared
        && declared != body_len
    {
        return Err(OagwError::validation_error(
            "oagw.proxy: the declared Content-Length does not match the received body size",
        ));
    }
    Ok(())
}

/// Reports whether a header value carries no control character, CR or LF
/// (`inst-pf-21`).
///
/// The `http` crate's [`HeaderValue`] already refuses to hold a CR or an LF, so
/// the rule is defence in depth for the values the pipeline is handed: it is
/// written over the raw bytes so an `obs-text` byte, which the header syntax
/// allows, still passes while a control character does not.
#[must_use]
pub fn header_value_allowed(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| *byte == b'\t' || (*byte >= 0x20 && *byte != 0x7f))
}

/// The values of one inbound header, as strings.
fn header_values(headers: &HeaderMap, name: &str) -> Vec<String> {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect()
}

/// The outbound policy the gear configuration carries (`inst-pf-29`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemePolicy {
    /// Whether a plaintext upstream connection may be established.
    pub allow_http_upstream: bool,
    /// Whether the SSRF posture of `cpt-cf-oagw-nfr-ssrf-protection` is on.
    pub ssrf_enabled: bool,
}

/// Applies the outbound scheme policy and the SSRF posture to the selected
/// endpoint (`inst-pf-30`).
///
/// `https` and `wss` are allowed unconditionally, `http` only when
/// `allow_http_upstream` is `true`, and a `grpc` or `wt` endpoint is never
/// proxied: it is answered with the gateway `RouteError`/`ProtocolError`
/// semantics instead of being forwarded.
///
/// # Errors
/// Returns the `RouteError` row for a non-proxied scheme, the
/// `ValidationError` row for a plaintext connection the knob does not lift and
/// for an endpoint host in a segment the SSRF posture refuses.
pub fn check_endpoint_policy(
    scheme: EndpointScheme,
    host: &str,
    policy: SchemePolicy,
) -> Result<(), OagwError> {
    if !matches!(
        scheme,
        EndpointScheme::Http | EndpointScheme::Https | EndpointScheme::Wss
    ) {
        return Err(OagwError::route_error(
            "oagw.proxy: the selected endpoint carries a scheme the gateway does not proxy",
        ));
    }
    if scheme.as_str() == SCHEME_HTTP && !policy.allow_http_upstream {
        return Err(OagwError::validation_error(
            "oagw.proxy: a plaintext upstream connection requires allow_http_upstream",
        ));
    }
    if policy.ssrf_enabled && !segment_allowed(host) {
        return Err(OagwError::validation_error(
            "oagw.proxy: the resolved endpoint host is outside the allowed segments",
        ));
    }
    Ok(())
}

/// The allowed-segment match of the SSRF posture.
///
/// The endpoint host the resolved configuration names is the only address the
/// outbound connection may reach, and an IP literal in a segment that can never
/// be a legitimate upstream target is refused when the policy is enabled. The
/// DNS-resolution half of the posture is a separate concern and is not built
/// here; the RFC 1918 ranges stay allowed, an internal upstream being the
/// ordinary case for a gateway.
///
/// A host is read as an address only when its literal form is the canonical
/// one: a dotted quad of exactly four decimal octets, or an IPv6 literal. Any
/// other spelling that is still made of digits and dots — `127.1`,
/// `2130706433` — is an address the platform's resolver would accept in
/// another form, so it is refused rather than passed as a name, and a
/// `::ffff:`-mapped IPv6 address is judged by the IPv4 segment it reaches.
fn segment_allowed(host: &str) -> bool {
    let Some(ip) = ip_literal(host) else {
        return !numeric_literal(host);
    };
    match ip {
        IpAddr::V4(v4) => v4_segment_allowed(v4),
        // The mapped form is the IPv4 address it carries, on the wire and to
        // the resolver, so it is judged by that address' segment.
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4_segment_allowed(v4),
            None => v6_segment_allowed(v6),
        },
    }
}

/// The IPv4 segment test of the SSRF posture.
fn v4_segment_allowed(v4: std::net::Ipv4Addr) -> bool {
    !(v4.is_loopback()
        || v4.is_unspecified()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation())
}

/// The IPv6 segment test of the SSRF posture.
fn v6_segment_allowed(v6: std::net::Ipv6Addr) -> bool {
    !(v6.is_loopback() || v6.is_unspecified() || v6.is_unicast_link_local() || v6.is_unique_local())
}

/// Whether a host is a numeric spelling of an address the platform's resolver
/// would still accept — an all-numeric dotted form of another octet count, an
/// integer or a hexadecimal one. Such a host names no name, so it is refused
/// rather than passed as one.
fn numeric_literal(host: &str) -> bool {
    let numeric = |label: &str| {
        let digits = label
            .strip_prefix("0x")
            .or_else(|| label.strip_prefix("0X"))
            .unwrap_or(label);
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    !host.is_empty()
        && host.split('.').all(numeric)
        && host.bytes().any(|byte| byte.is_ascii_hexdigit())
}

/// The outbound target the IP-pinning rule fixes: the scheme, host, port and
/// authority of the resolved endpoint, with the path and query the matched
/// route composed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundTarget {
    /// The outbound scheme, from the selected endpoint.
    pub scheme: String,
    /// The outbound host, from the selected endpoint.
    pub host: String,
    /// The outbound port, from the selected endpoint.
    pub port: i64,
    /// The `Host` / `:authority` value the outbound request carries.
    pub authority: String,
    /// The absolute outbound URI.
    pub uri: String,
}

/// Composes the outbound target of a request from the selected endpoint.
///
/// The caller's headers and path can contribute the path and query only: the
/// scheme, the host and the port come from the endpoint, which is what the
/// pinning rule fixes, so no dot-segment in the suffix can change where the
/// outbound connection goes.
#[must_use]
pub fn outbound_target(endpoint: &Endpoint, path: &str, query: &[QueryParam]) -> OutboundTarget {
    let scheme = endpoint.scheme_enum().map_or_else(
        || DEFAULT_ENDPOINT_SCHEME.to_owned(),
        |scheme| scheme.as_str().to_owned(),
    );
    let host = endpoint.host_stripped().unwrap_or_default().to_owned();
    let authority = authority_of(&scheme, &host, endpoint.port);
    let query_string = query_string(query);
    let suffix = if query_string.is_empty() {
        String::new()
    } else {
        format!("?{query_string}")
    };
    OutboundTarget {
        // The URI carries the scheme an absolute URI can name an origin with:
        // `wss` and `ws` name a WebSocket, and the transport they ride is the
        // `https` and `http` one, which is what the outbound client dials. The
        // configured scheme itself stays on `OutboundTarget.scheme`.
        uri: format!("{}://{authority}{path}{suffix}", transport_scheme(&scheme)),
        scheme,
        host,
        port: endpoint.port,
        authority,
    }
}

/// The transport scheme a configured scheme rides, `https` for `wss` and `http`
/// for `ws`.
///
/// An absolute URI may name the transport only: `wss://` is not a scheme the
/// outbound client can dial, and the upgrade semantics of a WebSocket request
/// are carried by its headers, not by the scheme of the connection. `ws` is no
/// scheme of the closed set any endpoint can carry, but the transport it names
/// is the plaintext one and is normalised all the same.
fn transport_scheme(scheme: &str) -> &str {
    match scheme {
        SCHEME_WSS => SCHEME_HTTPS,
        "ws" => SCHEME_HTTP,
        other => other,
    }
}

/// The `Host` value of an endpoint: the host, with the port only when it is not
/// the scheme's default, and an IPv6 host in the brackets an authority needs to
/// keep it apart from the port.
#[must_use]
pub fn authority_of(scheme: &str, host: &str, port: i64) -> String {
    // An IPv6 literal in an authority is bracketed: `2001:db8::1:443` is no
    // authority the URI grammar reads, `[2001:db8::1]:443` is.
    let host = unbracketed(host);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if port == default_port(scheme) {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// The host without the brackets an authority carries, when it carried any.
fn unbracketed(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
}

/// The default port of a scheme, `443` for every proxied scheme but `http`.
#[must_use]
pub fn default_port(scheme: &str) -> i64 {
    if scheme == SCHEME_HTTP { 80 } else { 443 }
}

/// Joins the allowlisted query parameters back into a query string.
fn query_string(query: &[QueryParam]) -> String {
    query
        .iter()
        .map(|param| param.raw.clone())
        .collect::<Vec<_>>()
        .join("&")
}

/// The per-request lifecycle of one proxied request
/// (`cpt-cf-oagw-state-proxy-context`).
///
/// The machine holds no state between requests, is never persisted and has no
/// cache behind it: every request opens a fresh `Classified` context, and the
/// only transitions out of `Responded` and `Failed` belong to the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyState {
    /// The request was classified and no decision has been taken yet.
    Classified,
    /// The resolution returned a configuration and a route matched.
    Routed,
    /// An endpoint was selected and the CORS check, where one applied, let the
    /// request continue.
    EndpointSelected,
    /// The header processing and the body and header validation passed.
    Validated,
    /// The request was written to an established connection.
    Dispatched,
    /// An upstream response was received and passed through.
    Responded,
    /// The request was refused, rejected or failed.
    Failed,
}

impl ProxyState {
    /// The name of the state, for diagnostics and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Classified => "Classified",
            Self::Routed => "Routed",
            Self::EndpointSelected => "EndpointSelected",
            Self::Validated => "Validated",
            Self::Dispatched => "Dispatched",
            Self::Responded => "Responded",
            Self::Failed => "Failed",
        }
    }

    /// Applies the closed transition set of FEATURE §4.
    ///
    /// Any transition not listed is invalid and leaves the state unchanged, so
    /// the context can never reach a state no stage produced.
    #[must_use]
    pub fn transition(self, to: Self) -> Option<Self> {
        match (self, to) {
            (Self::Classified, Self::Routed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-01
                // FROM Classified TO Routed: the config-resolution stage
                // returned an `EffectiveConfig` and route matching matched a
                // route that accepted the request's method, path suffix and
                // query string.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-01
                Some(to)
            }
            (Self::Classified, Self::Failed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-02
                // FROM Classified TO Failed: the resolution returned a
                // disposition, no route of the tier matched, or a matched route
                // rejected the request.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-02
                Some(to)
            }
            (Self::Routed, Self::EndpointSelected) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-03
                // FROM Routed TO EndpointSelected: endpoint selection returned
                // an endpoint and the actual-request CORS check, where one
                // applied, let the request continue.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-03
                Some(to)
            }
            (Self::Routed, Self::Failed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-04
                // FROM Routed TO Failed: the endpoint selection failed on a
                // missing, malformed or unknown `X-OAGW-Target-Host`, or the
                // CORS check rejected the origin or the method.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-04
                Some(to)
            }
            (Self::EndpointSelected, Self::Validated) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-05
                // FROM EndpointSelected TO Validated: the header processing and
                // the body and header validation both passed.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-05
                Some(to)
            }
            (Self::EndpointSelected, Self::Failed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-06
                // FROM EndpointSelected TO Failed: the header or body
                // validation failed.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-06
                Some(to)
            }
            (Self::Validated, Self::Dispatched) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-07
                // FROM Validated TO Dispatched: the auth phase, the rate-limit
                // check and the guard and transform request tiers all admitted
                // the request, the scheme and SSRF policy allowed the endpoint,
                // and the outbound call was issued.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-07
                Some(to)
            }
            (Self::Validated, Self::Failed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-08
                // FROM Validated TO Failed: a chain phase or the rate-limit
                // check refused, the scheme or SSRF policy refused the
                // endpoint, or the outbound call could not be established
                // before it was issued.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-08
                Some(to)
            }
            (Self::Dispatched, Self::Responded) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-09
                // FROM Dispatched TO Responded: an upstream response was
                // received and passed through with its header work applied.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-09
                Some(to)
            }
            (Self::Dispatched, Self::Failed) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-10
                // FROM Dispatched TO Failed: the exchange failed — a
                // connection, request or idle timeout, a protocol error, a
                // downstream error, an aborted stream, or an unavailable link.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-10
                Some(to)
            }
            (Self::Responded, Self::Classified) | (Self::Failed, Self::Classified) => {
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-11
                // FROM Responded TO Classified: the next proxied request
                // arrives, the machine being the per-request lifecycle of one
                // request, so a completed outcome is never carried into the
                // next one.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-11
                // @cpt-begin:cpt-cf-oagw-state-proxy-context:p1:inst-ct-12
                // FROM Failed TO Classified: the next proxied request arrives,
                // the failure having been rendered and the context discarded
                // with the request that produced it.
                // @cpt-end:cpt-cf-oagw-state-proxy-context:p1:inst-ct-12
                Some(to)
            }
            _ => None,
        }
    }
}

/// The per-request `ProxyContext` (`cpt-cf-oagw-state-proxy-context`).
///
/// The context is the only state the pipeline owns for a request: it records
/// the lifecycle state the stages drove the request through and the routing
/// facts the metrics count, and it is discarded with the request.
#[derive(Debug, Clone)]
pub struct ProxyContext {
    state: ProxyState,
    /// The normalized alias the request resolved.
    alias: String,
    /// The matched route, when one matched.
    route_id: Option<Uuid>,
    /// The selected endpoint host, when one was selected.
    endpoint_host: Option<String>,
    /// How the endpoint was selected, when one was.
    selection_method: Option<SelectionMethod>,
}

impl ProxyContext {
    /// Opens the context of one request in its `Classified` state.
    #[must_use]
    pub fn new(alias: impl Into<String>) -> Self {
        Self {
            state: ProxyState::Classified,
            alias: alias.into(),
            route_id: None,
            endpoint_host: None,
            selection_method: None,
        }
    }

    /// The state the request reached.
    #[must_use]
    pub const fn state(&self) -> ProxyState {
        self.state
    }

    /// The normalized alias the request resolved.
    #[must_use]
    pub const fn alias(&self) -> &String {
        &self.alias
    }

    /// The matched route, when one matched.
    #[must_use]
    pub const fn route_id(&self) -> Option<Uuid> {
        self.route_id
    }

    /// The selected endpoint host, when one was selected.
    #[must_use]
    pub fn endpoint_host(&self) -> Option<&str> {
        self.endpoint_host.as_deref()
    }

    /// How the endpoint was selected, when one was.
    #[must_use]
    pub const fn selection_method(&self) -> Option<SelectionMethod> {
        self.selection_method
    }

    /// Records the matched route.
    pub fn record_route(&mut self, route_id: Uuid) {
        self.route_id = Some(route_id);
    }

    /// Records the selected endpoint and the method it was selected by.
    pub fn record_endpoint(&mut self, host: &str, method: SelectionMethod) {
        self.endpoint_host = Some(host.to_owned());
        self.selection_method = Some(method);
    }

    /// Applies a transition, leaving an invalid transition unapplied.
    ///
    /// # Errors
    /// Returns the state the context was in when the transition was not one of
    /// the twelve declared ones; the context is left unchanged.
    pub fn advance(&mut self, to: ProxyState) -> Result<(), ProxyState> {
        match self.state.transition(to) {
            Some(next) => {
                self.state = next;
                Ok(())
            }
            None => Err(self.state),
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-endpoint-selection-contract:p1:inst-full
// @cpt-end:cpt-cf-oagw-dod-request-validation:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HttpMatch, MatchConfig, PATH_SUFFIX_APPEND, PATH_SUFFIX_DISABLED};
    use tenant_resolver_sdk::TenantId;

    const TENANT: Uuid = Uuid::from_u128(0x90da_0001);

    /// An HTTP endpoint on a routable documentation host.
    fn endpoint(host: &str) -> Endpoint {
        Endpoint::new("https", host, 443)
    }

    /// A route matching `path` for `methods`.
    fn route(path: &str, methods: &[&str], priority: i64, enabled: bool) -> Route {
        Route {
            id: Some(Uuid::from_u128(0x1000 + priority as u128)),
            enabled,
            priority,
            match_config: Some(MatchConfig {
                http: Some(HttpMatch {
                    methods: methods.iter().map(ToString::to_string).collect(),
                    path: Some(path.to_owned()),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PATH_SUFFIX_APPEND.to_owned(),
                }),
                grpc: None,
            }),
            ..Route::default()
        }
    }

    fn tier(routes: Vec<Route>) -> Vec<RouteTier> {
        vec![RouteTier {
            tenant_id: TenantId(TENANT),
            upstream_id: Uuid::from_u128(0x2000),
            routes,
        }]
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<QueryParam> {
        pairs
            .iter()
            .map(|(name, value)| QueryParam {
                name: (*name).to_owned(),
                raw: format!("{name}={value}"),
            })
            .collect()
    }

    /// The matching call every route-match test makes.
    fn matched(tier_routes: Vec<RouteTier>, path: &str, query: &[QueryParam]) -> MatchedRoute {
        match_route(PROTOCOL_HTTP, &tier_routes, "GET", path, query)
            .unwrap_or_else(|rejection| panic!("the request must match, got {rejection:?}"))
    }

    #[test]
    fn a_longer_path_prefix_wins_over_a_shorter_one() {
        let routes = tier(vec![
            route("/api", &["GET"], 0, true),
            route("/api/v1", &["GET"], 0, true),
        ]);

        let matched = matched(routes, "/api/v1/users", &[]);

        assert_eq!(matched.route.priority, 0);
        assert_eq!(matched.outbound_path, "/api/v1/users");
    }

    #[test]
    fn a_descendant_tier_route_wins_at_the_same_prefix() {
        let selected = tier(vec![route("/api", &["GET"], 0, true)]);
        let ancestor = vec![RouteTier {
            tenant_id: TenantId(TENANT),
            upstream_id: Uuid::from_u128(0x2001),
            routes: vec![route("/api", &["GET"], 99, true)],
        }];

        let matched = matched([selected, ancestor].concat(), "/api/users", &[]);

        assert_eq!(matched.route.id, Some(Uuid::from_u128(0x1000)));
    }

    #[test]
    fn the_greater_priority_wins_at_the_same_prefix_in_one_tier() {
        let routes = tier(vec![
            route("/api", &["GET"], 0, true),
            route("/api", &["GET"], 7, true),
        ]);

        let matched = matched(routes, "/api/users", &[]);

        assert_eq!(matched.route.priority, 7);
    }

    #[test]
    fn a_disabled_route_is_never_matched() {
        let routes = tier(vec![
            route("/api/v1", &["GET"], 9, false),
            route("/api", &["GET"], 0, true),
        ]);

        let matched = matched(routes, "/api/v1/users", &[]);

        assert_eq!(matched.route.priority, 0);
        assert_eq!(matched.outbound_path, "/api/v1/users");
    }

    #[test]
    fn a_method_in_no_candidate_allowlist_is_a_route_not_found() {
        let routes = tier(vec![route("/api", &["POST"], 0, true)]);

        let rejection = match_route(PROTOCOL_HTTP, &routes, "GET", "/api/users", &[])
            .expect_err("the method is in no allowlist");

        assert_eq!(rejection, MatchRejection::NoRoute);
    }

    #[test]
    fn a_suffix_offered_to_a_disabled_suffix_route_is_a_validation_error() {
        let mut forbidden = route("/api", &["GET"], 9, true);
        if let Some(http) = forbidden
            .match_config
            .as_mut()
            .and_then(|m| m.http.as_mut())
        {
            http.path_suffix_mode = PATH_SUFFIX_DISABLED.to_owned();
        }
        let routes = tier(vec![forbidden]);

        let rejection = match_route(PROTOCOL_HTTP, &routes, "GET", "/api/users", &[])
            .expect_err("the suffix is forbidden");

        assert_eq!(rejection, MatchRejection::Content(PATH_SUFFIX_RULE));
    }

    #[test]
    fn a_suffix_matching_the_whole_prefix_is_accepted_when_the_mode_disables_it() {
        let mut exact = route("/api/users", &["GET"], 0, true);
        if let Some(http) = exact.match_config.as_mut().and_then(|m| m.http.as_mut()) {
            http.path_suffix_mode = PATH_SUFFIX_DISABLED.to_owned();
        }
        let routes = tier(vec![exact]);

        let matched = matched(routes, "/api/users", &[]);

        assert_eq!(matched.outbound_path, "/api/users");
    }

    #[test]
    fn an_unlisted_query_parameter_is_a_validation_error() {
        let mut allowlisted = route("/api", &["GET"], 0, true);
        if let Some(http) = allowlisted
            .match_config
            .as_mut()
            .and_then(|m| m.http.as_mut())
        {
            http.query_allowlist = vec!["page".to_owned()];
        }
        let routes = tier(vec![allowlisted]);

        let rejection = match_route(
            PROTOCOL_HTTP,
            &routes,
            "GET",
            "/api/users",
            &params(&[("filter", "x")]),
        )
        .expect_err("the parameter is unlisted");

        assert_eq!(rejection, MatchRejection::Content(QUERY_ALLOWLIST_RULE));
    }

    #[test]
    fn an_empty_allowlist_allows_none() {
        let routes = tier(vec![route("/api", &["GET"], 0, true)]);

        let rejection = match_route(
            PROTOCOL_HTTP,
            &routes,
            "GET",
            "/api/users",
            &params(&[("page", "1")]),
        )
        .expect_err("the empty allowlist allows none");

        assert_eq!(rejection, MatchRejection::Content(QUERY_ALLOWLIST_RULE));
    }

    #[test]
    fn the_outbound_query_is_the_allowlisted_subset_in_inbound_order() {
        let mut allowlisted = route("/api", &["GET"], 0, true);
        if let Some(http) = allowlisted
            .match_config
            .as_mut()
            .and_then(|m| m.http.as_mut())
        {
            http.query_allowlist = vec!["a".to_owned(), "b".to_owned()];
        }
        let routes = tier(vec![allowlisted]);

        let matched = matched(
            routes,
            "/api/users",
            &params(&[("b", "2"), ("a", "1"), ("a", "3")]),
        );

        let raws: Vec<&str> = matched
            .outbound_query
            .iter()
            .map(|p| p.raw.as_str())
            .collect();
        assert_eq!(raws, vec!["b=2", "a=1", "a=3"]);
    }

    #[test]
    fn the_outbound_path_is_the_base_with_the_beyond_prefix_suffix() {
        let routes = tier(vec![route("/api/v1", &["GET"], 0, true)]);

        let matched = matched(routes, "/api/v1/accounts/42", &[]);

        assert_eq!(matched.outbound_path, "/api/v1/accounts/42");
    }

    #[test]
    fn a_non_http_protocol_is_never_matched_as_http() {
        let routes = tier(vec![route("/api", &["GET"], 0, true)]);

        let rejection = match_route(
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1",
            &routes,
            "GET",
            "/api",
            &[],
        )
        .expect_err("a non-HTTP protocol is never matched as HTTP");

        assert_eq!(rejection, MatchRejection::NotProxied);
    }

    #[test]
    fn the_match_is_deterministic() {
        let routes = tier(vec![route("/api", &["GET"], 0, true)]);

        let first = matched(routes.clone(), "/api/users", &[]);
        let second = matched(routes, "/api/users", &[]);

        assert_eq!(first.outbound_path, second.outbound_path);
        assert_eq!(first.route.id, second.route.id);
    }

    fn pool(hosts: &[&str]) -> Vec<Endpoint> {
        hosts.iter().map(|host| endpoint(host)).collect()
    }

    #[test]
    fn a_single_endpoint_pool_routes_with_the_header_absent() {
        let mut cursor = 0_u64;

        let selected = select_endpoint(
            &pool(&["api.example.com"]),
            AliasKind::CommonSuffix,
            None,
            &mut cursor,
        )
        .expect("the single endpoint routes with no header");

        assert_eq!(selected.endpoint.host_stripped(), Some("api.example.com"));
        assert_eq!(selected.method, SelectionMethod::Default);
    }

    #[test]
    fn a_single_endpoint_pool_validates_a_present_header() {
        let endpoints = pool(&["api.example.com"]);

        let invalid = select_endpoint(
            &endpoints,
            AliasKind::CommonSuffix,
            Some("api.example.com:443"),
            &mut 0,
        )
        .expect_err("a value carrying a port is malformed");
        assert_eq!(invalid, SelectionFailure::Invalid);

        let unknown = select_endpoint(
            &endpoints,
            AliasKind::CommonSuffix,
            Some("other.example.com"),
            &mut 0,
        )
        .expect_err("a well-formed value naming no endpoint is unknown");
        assert_eq!(unknown, SelectionFailure::Unknown);

        let selected = select_endpoint(
            &endpoints,
            AliasKind::CommonSuffix,
            Some("API.Example.COM."),
            &mut 0,
        )
        .expect("the header is optional but validated when present");
        assert_eq!(selected.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn a_multi_endpoint_explicit_alias_pool_round_robins_when_the_header_is_absent() {
        let endpoints = pool(&["a.example.com", "b.example.com", "c.example.com"]);
        let mut cursor = 0_u64;

        for expected in [
            "a.example.com",
            "b.example.com",
            "c.example.com",
            "a.example.com",
        ] {
            let selected = select_endpoint(&endpoints, AliasKind::Explicit, None, &mut cursor)
                .expect("a multi-endpoint pool never rejects for a missing header");
            assert_eq!(selected.endpoint.host_stripped(), Some(expected));
            assert_eq!(selected.method, SelectionMethod::RoundRobin);
        }
    }

    #[test]
    fn a_multi_endpoint_pool_routes_to_the_named_endpoint() {
        let endpoints = pool(&["a.example.com", "b.example.com"]);

        let selected = select_endpoint(
            &endpoints,
            AliasKind::Explicit,
            Some("b.example.com"),
            &mut 0,
        )
        .expect("the header names an endpoint of the pool");

        assert_eq!(selected.endpoint.host_stripped(), Some("b.example.com"));
        assert_eq!(selected.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn a_multi_endpoint_common_suffix_alias_requires_the_header() {
        let endpoints = pool(&["a.example.com", "b.example.com"]);

        let failure = select_endpoint(&endpoints, AliasKind::CommonSuffix, None, &mut 0)
            .expect_err("the header is required to disambiguate the target");

        assert_eq!(failure, SelectionFailure::Missing);
    }

    #[test]
    fn an_invalid_target_host_form_is_rejected_before_any_pool_comparison() {
        let endpoints = pool(&["a.example.com"]);

        for value in [
            "a.example.com:443",
            "a.example.com/api",
            "a b.example.com",
            "a..example.com",
            "-a.example.com",
            "a!.example.com",
            "",
        ] {
            let failure = select_endpoint(&endpoints, AliasKind::Explicit, Some(value), &mut 0)
                .err()
                .unwrap_or_else(|| panic!("'{value}' must be rejected as malformed"));
            assert_eq!(failure, SelectionFailure::Invalid, "{value}");
        }
    }

    #[test]
    fn an_ip_literal_target_host_is_a_valid_form() {
        let endpoints = pool(&["10.0.0.5"]);

        let selected = select_endpoint(&endpoints, AliasKind::Explicit, Some("10.0.0.5"), &mut 0)
            .expect("an IP literal is a hostname form the header may carry");

        assert_eq!(selected.endpoint.host_stripped(), Some("10.0.0.5"));
    }

    /// An IPv6 endpoint is nameable: the header may carry it bare or bracketed,
    /// any spelling of the same address naming it, while a value a colon makes
    /// ambiguous in any other way — a port — stays malformed.
    #[test]
    fn an_ipv6_endpoint_is_nameable_by_the_target_host_header() {
        let endpoints = pool(&["2001:db8::1", "10.0.0.5"]);

        for value in ["2001:db8::1", "[2001:db8::1]", "[2001:0db8:0000::1]"] {
            let selected = select_endpoint(&endpoints, AliasKind::Explicit, Some(value), &mut 0)
                .unwrap_or_else(|failure| panic!("'{value}' names an endpoint: {failure:?}"));
            assert_eq!(selected.method, SelectionMethod::ExplicitHeader, "{value}");
            assert_eq!(
                selected.endpoint.host_stripped(),
                Some("2001:db8::1"),
                "{value}"
            );
        }

        let unknown = select_endpoint(&endpoints, AliasKind::Explicit, Some("2001:db8::2"), &mut 0)
            .expect_err("a well-formed IPv6 address naming no endpoint is unknown");
        assert_eq!(unknown, SelectionFailure::Unknown);

        let invalid = select_endpoint(
            &endpoints,
            AliasKind::Explicit,
            Some("[2001:db8::1]:443"),
            &mut 0,
        )
        .expect_err("a value carrying a port is malformed");
        assert_eq!(invalid, SelectionFailure::Invalid);
    }

    #[test]
    fn the_round_robin_cursor_is_per_pool() {
        let first = pool(&["a.example.com", "b.example.com"]);
        let second = pool(&["x.example.com", "y.example.com"]);
        let mut first_cursor = 1_u64;
        let mut second_cursor = 0_u64;

        let advanced =
            select_endpoint(&first, AliasKind::Explicit, None, &mut first_cursor).unwrap();
        let untouched =
            select_endpoint(&second, AliasKind::Explicit, None, &mut second_cursor).unwrap();

        assert_eq!(advanced.endpoint.host_stripped(), Some("b.example.com"));
        assert_eq!(untouched.endpoint.host_stripped(), Some("x.example.com"));
    }

    fn cors_config(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            enabled: Some(true),
            allowed_origins: origins.iter().map(ToString::to_string).collect(),
            allowed_methods: methods.iter().map(ToString::to_string).collect(),
            ..CorsConfig::default()
        }
    }

    fn check<'a>(method: &'a str, origin: Option<&'a str>) -> CorsCheck<'a> {
        CorsCheck {
            method,
            origin,
            request_method: None,
            request_headers: None,
        }
    }

    #[test]
    fn a_preflight_is_answered_with_the_permissive_204_headers() {
        let block = cors_config(&["https://app.example.com"], &["GET", "POST"]);

        let outcome = cors_check(
            Some(&block),
            CorsCheck {
                method: "OPTIONS",
                origin: Some("https://evil.example.com"),
                request_method: Some("PUT"),
                request_headers: Some("X-Custom"),
            },
        );

        let CorsOutcome::Preflight(preflight) = outcome else {
            panic!("a preflight is answered permissively, got {outcome:?}");
        };
        assert_eq!(preflight.allow_origin, "https://evil.example.com");
        assert_eq!(preflight.allow_methods, "PUT");
        assert_eq!(preflight.allow_headers, "X-Custom");
        assert_eq!(preflight.max_age, 86_400);
        assert_eq!(preflight.vary, VARY_PREFLIGHT);
    }

    #[test]
    fn a_request_with_no_origin_performs_no_cors_work() {
        let block = cors_config(&["https://app.example.com"], &["GET"]);

        let outcome = cors_check(Some(&block), check("GET", None));

        assert_eq!(outcome, CorsOutcome::NotApplicable);
    }

    #[test]
    fn no_enabled_cors_block_performs_no_cors_work() {
        let mut block = cors_config(&["https://app.example.com"], &["GET"]);
        block.enabled = Some(false);
        let outcome = cors_check(Some(&block), check("GET", Some("https://evil.example.com")));
        assert_eq!(outcome, CorsOutcome::NotApplicable);

        let outcome = cors_check(None, check("GET", Some("https://app.example.com")));
        assert_eq!(outcome, CorsOutcome::NotApplicable);
    }

    #[test]
    fn an_origin_outside_the_list_is_rejected() {
        let block = cors_config(&["https://app.example.com"], &["GET"]);

        for origin in [
            "https://evil.com",
            "https://app.example.com:8080",
            "http://app.example.com",
            "https://app.example.com.evil.com",
        ] {
            let outcome = cors_check(Some(&block), check("GET", Some(origin)));
            assert_eq!(outcome, CorsOutcome::OriginRejected, "{origin}");
        }

        let allowed = cors_check(Some(&block), check("GET", Some("https://app.example.com")));
        assert_eq!(
            allowed,
            CorsOutcome::Allowed(AllowedHeaders {
                allow_origin: "https://app.example.com".to_owned(),
                expose_headers: None,
                allow_credentials: false,
            })
        );
    }

    #[test]
    fn a_method_outside_the_list_is_rejected_after_the_origin() {
        let block = cors_config(&["https://app.example.com"], &["GET", "POST"]);

        let outcome = cors_check(
            Some(&block),
            check("DELETE", Some("https://app.example.com")),
        );

        assert_eq!(outcome, CorsOutcome::MethodRejected);
    }

    #[test]
    fn a_wildcard_origin_matches_any_origin() {
        let block = cors_config(&["*"], &["GET"]);

        let outcome = cors_check(Some(&block), check("GET", Some("https://anywhere.example")));

        let CorsOutcome::Allowed(headers) = outcome else {
            panic!("the wildcard matches any origin, got {outcome:?}");
        };
        assert_eq!(headers.allow_origin, "*");
    }

    #[test]
    fn a_preflight_is_detected_by_method_origin_and_requested_method() {
        assert!(is_preflight(
            "OPTIONS",
            Some("https://a.example"),
            Some("PUT")
        ));
        assert!(!is_preflight("OPTIONS", Some("https://a.example"), None));
        assert!(!is_preflight("OPTIONS", None, Some("PUT")));
        assert!(!is_preflight("GET", Some("https://a.example"), Some("PUT")));
    }

    #[test]
    fn an_upgrade_request_keeps_only_the_three_exempt_families() {
        let rules = RequestHeaders {
            passthrough: "all".to_owned(),
            ..RequestHeaders::default()
        };
        let mut inbound = request_headers();
        inbound.insert(
            HeaderName::from_static("sec-websocket-key"),
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );
        inbound.insert(
            HeaderName::from_static("sec-websocket-version"),
            HeaderValue::from_static("13"),
        );
        inbound.insert(
            HeaderName::from_static("sec-websocket-protocol"),
            HeaderValue::from_static("chat, superchat"),
        );

        let outbound = prepare_upgrade_request_headers(&inbound, "ws.example.com", Some(&rules));

        for name in [
            "upgrade",
            "connection",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
        ] {
            assert_eq!(outbound.get(name), inbound.get(name), "{name} must survive");
        }
        for name in [
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
        ] {
            assert!(!outbound.contains_key(name), "{name} must be stripped");
        }
        assert_eq!(
            outbound.get(HOST_HEADER),
            Some(&HeaderValue::from_static("ws.example.com"))
        );
        assert!(!outbound.contains_key(TARGET_HOST_HEADER));
        assert_eq!(
            outbound.get("x-tenant"),
            Some(&HeaderValue::from_static("acme"))
        );
    }

    #[test]
    fn an_upgrade_request_forwards_the_exempt_families_in_every_passthrough_mode() {
        let mut inbound = request_headers();
        inbound.insert(
            HeaderName::from_static("upgrade"),
            HeaderValue::from_static("websocket"),
        );
        inbound.insert(
            HeaderName::from_static("connection"),
            HeaderValue::from_static("Upgrade"),
        );
        inbound.insert(
            HeaderName::from_static("sec-websocket-key"),
            HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
        );

        for rules in [
            None,
            Some(RequestHeaders::default()),
            Some(RequestHeaders {
                passthrough: "all".to_owned(),
                ..RequestHeaders::default()
            }),
        ] {
            let outbound =
                prepare_upgrade_request_headers(&inbound, "ws.example.com", rules.as_ref());
            for name in ["upgrade", "connection", "sec-websocket-key"] {
                assert_eq!(
                    outbound.get(name),
                    inbound.get(name),
                    "{name} must survive the {mode:?} passthrough mode",
                    mode = rules
                        .as_ref()
                        .map_or("absent", |rules| rules.passthrough.as_str())
                );
            }
        }

        // The exemption widens nothing: in the schema-default mode the outbound
        // set is the three families plus `Host`, and nothing else.
        let outbound = prepare_upgrade_request_headers(&inbound, "ws.example.com", None);
        let names: Vec<_> = outbound.keys().map(|name| name.as_str()).collect();
        assert_eq!(names.len(), 4, "exactly the exempt families plus Host");
        for name in ["upgrade", "connection", "sec-websocket-key", "host"] {
            assert!(
                names.contains(&name),
                "{name} must be in the carved upgrade set, got {names:?}"
            );
        }
    }

    #[test]
    fn an_upgrade_request_is_carved_by_no_other_rule() {
        let rules = RequestHeaders {
            passthrough: "all".to_owned(),
            ..RequestHeaders::default()
        };
        let upgrade =
            prepare_upgrade_request_headers(&request_headers(), "ws.example.com", Some(&rules));
        let plain = prepare_request_headers(&request_headers(), "ws.example.com", Some(&rules));

        for (name, value) in plain.iter() {
            assert_eq!(upgrade.get(name), Some(value), "{name} must survive");
        }
        for name in upgrade.keys() {
            assert!(
                plain.contains_key(name)
                    || name == "upgrade"
                    || name == "connection"
                    || name.as_str().starts_with(SEC_WEBSOCKET_PREFIX),
                "{name} must not be exempted"
            );
        }
    }

    #[test]
    fn the_request_rules_apply_over_the_carved_upgrade_set() {
        let mut rules = RequestHeaders::default();
        rules.set.insert("upgrade".to_owned(), "h2c".to_owned());
        rules.remove.push("sec-websocket-version".to_owned());
        rules.passthrough = "all".to_owned();

        let outbound =
            prepare_upgrade_request_headers(&request_headers(), "ws.example.com", Some(&rules));

        assert_eq!(
            outbound.get("upgrade"),
            Some(&HeaderValue::from_static("h2c"))
        );
        assert!(!outbound.contains_key("sec-websocket-version"));
        assert_eq!(
            outbound.get("connection"),
            Some(&HeaderValue::from_static("close"))
        );
    }

    fn request_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("x-oagw-target-host"),
            HeaderValue::from_static("api.example.com"),
        );
        headers.insert(
            HeaderName::from_static("connection"),
            HeaderValue::from_static("close"),
        );
        headers.insert(
            HeaderName::from_static("keep-alive"),
            HeaderValue::from_static("1"),
        );
        headers.insert(
            HeaderName::from_static("proxy-authorization"),
            HeaderValue::from_static("Basic abc"),
        );
        headers.insert(
            HeaderName::from_static("te"),
            HeaderValue::from_static("trailers"),
        );
        headers.insert(
            HeaderName::from_static("trailer"),
            HeaderValue::from_static("x"),
        );
        headers.insert(
            HeaderName::from_static("transfer-encoding"),
            HeaderValue::from_static("chunked"),
        );
        headers.insert(
            HeaderName::from_static("upgrade"),
            HeaderValue::from_static("websocket"),
        );
        headers.insert(
            HeaderName::from_static("x-tenant"),
            HeaderValue::from_static("acme"),
        );
        headers
    }

    #[test]
    fn the_hop_by_hop_and_routing_headers_are_absent_from_the_outbound_request() {
        let rules = RequestHeaders {
            passthrough: "all".to_owned(),
            ..RequestHeaders::default()
        };

        let outbound = prepare_request_headers(&request_headers(), "api.example.com", Some(&rules));

        for name in HOP_BY_HOP_HEADERS {
            assert!(!outbound.contains_key(*name), "{name} must be stripped");
        }
        assert!(!outbound.contains_key(TARGET_HOST_HEADER));
        assert_eq!(
            outbound.get("x-tenant"),
            Some(&HeaderValue::from_static("acme"))
        );
    }

    #[test]
    fn a_request_with_no_header_rules_forwards_no_inbound_header() {
        let outbound = prepare_request_headers(&request_headers(), "api.example.com", None);

        assert_eq!(outbound.get("x-tenant"), None);
        assert_eq!(outbound.get("content-type"), None);
        assert_eq!(
            outbound.get(HOST_HEADER),
            Some(&HeaderValue::from_static("api.example.com"))
        );
    }

    #[test]
    fn host_is_replaced_with_the_selected_endpoint_authority() {
        let outbound =
            prepare_request_headers(&request_headers(), "upstream.example.com:8443", None);

        assert_eq!(
            outbound.get(HOST_HEADER),
            Some(&HeaderValue::from_static("upstream.example.com:8443"))
        );
    }

    #[test]
    fn the_request_rules_apply_set_add_remove_and_passthrough() {
        let mut rules = RequestHeaders::default();
        rules.set.insert("x-set".to_owned(), "one".to_owned());
        rules
            .set
            .insert("content-type".to_owned(), "text/plain".to_owned());
        rules.add.insert("x-added".to_owned(), "two".to_owned());
        rules.remove.push("x-tenant".to_owned());
        rules.passthrough = "allowlist".to_owned();
        rules.passthrough_allowlist = vec!["x-forwarded-for".to_owned()];
        let mut inbound = request_headers();
        inbound.insert(
            HeaderName::from_static("x-forwarded-for"),
            HeaderValue::from_static("203.0.113.7"),
        );

        let outbound = prepare_request_headers(&inbound, "upstream.example.com", Some(&rules));

        assert_eq!(
            outbound.get("x-set"),
            Some(&HeaderValue::from_static("one"))
        );
        assert_eq!(
            outbound.get("content-type"),
            Some(&HeaderValue::from_static("text/plain"))
        );
        assert_eq!(
            outbound.get("x-added"),
            Some(&HeaderValue::from_static("two"))
        );
        assert_eq!(outbound.get("x-tenant"), None);
        assert_eq!(
            outbound.get("x-forwarded-for"),
            Some(&HeaderValue::from_static("203.0.113.7"))
        );
        assert_eq!(outbound.get("x-oagw-target-host"), None);
    }

    #[test]
    fn a_passthrough_mode_of_none_forwards_no_inbound_header() {
        let rules = RequestHeaders {
            passthrough: "none".to_owned(),
            ..RequestHeaders::default()
        };

        let outbound =
            prepare_request_headers(&request_headers(), "upstream.example.com", Some(&rules));

        assert_eq!(outbound.get("x-tenant"), None);
        assert_eq!(
            outbound.get(HOST_HEADER),
            Some(&HeaderValue::from_static("upstream.example.com"))
        );
    }

    fn inbound_headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_content_length_that_is_not_an_integer_is_a_validation_error() {
        let headers = inbound_headers(&[("content-length", "abc")]);

        let error = validate_inbound(&headers, 0, 100).expect_err("not an integer");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn a_content_length_mismatch_is_a_validation_error() {
        let headers = inbound_headers(&[("content-length", "5")]);

        let error = validate_inbound(&headers, 4, 100).expect_err("a mismatch");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn a_matching_content_length_is_accepted() {
        let headers = inbound_headers(&[("content-length", "4")]);

        assert!(validate_inbound(&headers, 4, 100).is_ok());
    }

    #[test]
    fn two_content_length_values_that_differ_are_a_validation_error() {
        let mut headers = HeaderMap::new();
        for value in ["4", "5"] {
            headers.append(
                HeaderName::from_static(CONTENT_LENGTH_HEADER),
                HeaderValue::from_static(value),
            );
        }

        let error = validate_inbound(&headers, 5, 100).expect_err("two differing lengths");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn two_equal_content_length_values_are_accepted() {
        let mut headers = HeaderMap::new();
        for _ in 0..2 {
            headers.append(
                HeaderName::from_static(CONTENT_LENGTH_HEADER),
                HeaderValue::from_static("4"),
            );
        }

        assert!(validate_inbound(&headers, 4, 100).is_ok());
    }

    #[test]
    fn a_content_length_that_is_not_text_is_a_validation_error() {
        let mut headers = HeaderMap::new();
        headers.append(
            HeaderName::from_static(CONTENT_LENGTH_HEADER),
            HeaderValue::from_bytes(b"5\xff").unwrap(),
        );

        let error = validate_inbound(&headers, 5, 100).expect_err("not text");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn a_body_over_the_limit_is_payload_too_large() {
        let headers = inbound_headers(&[("content-length", "101")]);

        let error = validate_inbound(&headers, 101, 100).expect_err("above the limit");

        assert_eq!(error.mapping().variant, "PayloadTooLarge");
    }

    #[test]
    fn a_transfer_encoding_other_than_chunked_is_a_validation_error() {
        let headers = inbound_headers(&[("transfer-encoding", "gzip")]);

        let error = validate_inbound(&headers, 0, 100).expect_err("not chunked");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn content_length_with_transfer_encoding_is_a_validation_error() {
        let headers = inbound_headers(&[("content-length", "4"), ("transfer-encoding", "chunked")]);

        let error = validate_inbound(&headers, 4, 100).expect_err("both carriers");

        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn a_header_value_carrying_a_control_character_is_refused() {
        assert!(!header_value_allowed(b"text\r\ninjected: host: x"));
        assert!(!header_value_allowed(b"text\ninjected"));
        assert!(!header_value_allowed(b"a\x00b"));
        assert!(!header_value_allowed(b"a\x7fb"));
        assert!(header_value_allowed(b"application/json"));
        assert!(header_value_allowed(b"na\xc3\xafve")); // obs-text is allowed
        assert!(header_value_allowed(b"tab\tseparated"));
    }

    #[test]
    fn an_obs_text_header_value_passes_the_inbound_validation() {
        let value = HeaderValue::from_bytes(b"obs\xc2\xa0text").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(HeaderName::from_static("x-bad"), value);

        assert!(validate_inbound(&headers, 0, 100).is_ok());
    }

    fn policy(allow_http: bool, ssrf: bool) -> SchemePolicy {
        SchemePolicy {
            allow_http_upstream: allow_http,
            ssrf_enabled: ssrf,
        }
    }

    #[test]
    fn an_http_endpoint_is_proxied_only_when_the_knob_allows_it() {
        assert!(
            check_endpoint_policy(EndpointScheme::Http, "api.example.com", policy(true, true))
                .is_ok()
        );

        let error =
            check_endpoint_policy(EndpointScheme::Http, "api.example.com", policy(false, true))
                .expect_err("the https-only posture holds without the knob");
        assert_eq!(error.mapping().variant, "ValidationError");
    }

    #[test]
    fn https_and_wss_are_proxied_regardless_of_the_knob() {
        for scheme in [EndpointScheme::Https, EndpointScheme::Wss] {
            assert!(check_endpoint_policy(scheme, "api.example.com", policy(false, true)).is_ok());
        }
    }

    #[test]
    fn a_non_proxied_scheme_is_answered_with_the_route_error_row() {
        for scheme in [EndpointScheme::Grpc, EndpointScheme::Wt] {
            let error = check_endpoint_policy(scheme, "api.example.com", policy(true, true))
                .expect_err("a non-proxied scheme is never forwarded");
            assert_eq!(error.mapping().variant, "RouteError");
            assert_eq!(error.mapping().status, 400);
        }
    }

    #[test]
    fn a_loopback_endpoint_is_refused_when_the_ssrf_policy_is_enabled() {
        assert!(
            check_endpoint_policy(EndpointScheme::Https, "127.0.0.1", policy(true, true)).is_err(),
            "the loopback segment is outside the allowed segments"
        );
        assert!(
            check_endpoint_policy(EndpointScheme::Https, "127.0.0.1", policy(true, false)).is_ok()
        );
        assert!(
            check_endpoint_policy(EndpointScheme::Https, "api.example.com", policy(true, true))
                .is_ok()
        );
    }

    /// Every spelling of the loopback address the platform's resolver would
    /// still accept is refused: the non-canonical dotted quad, the integer form
    /// and the hexadecimal one. A host that is read as no address at all and
    /// that carries a name is not one of them.
    #[test]
    fn an_address_in_a_non_canonical_literal_form_is_refused_by_the_ssrf_posture() {
        for host in [
            "127.1",
            "127.0.1",
            "2130706433",
            "0177.0.0.1",
            "0x7f.0.0.1",
            "0x7f000001",
            "[::ffff:127.0.0.1]",
            "[::1]",
        ] {
            let error = check_endpoint_policy(EndpointScheme::Https, host, policy(true, true))
                .expect_err(host);
            assert_eq!(error.mapping().variant, "ValidationError", "{host}");
        }
    }

    /// A name is never an address: a host whose spelling carries a name is read
    /// as one and allowed, and the RFC 1918 segments stay allowed.
    #[test]
    fn a_named_endpoint_is_never_refused_by_the_ssrf_posture() {
        for host in [
            "api.example.com",
            "x.com",
            "internal.example.internal",
            "10.0.0.7",
            "192.168.1.1",
            "[2001:db8::1]",
        ] {
            assert!(
                check_endpoint_policy(EndpointScheme::Https, host, policy(true, true)).is_ok(),
                "{host} is a name or an allowed segment"
            );
        }
    }

    #[test]
    fn the_outbound_target_is_pinned_to_the_resolved_endpoint() {
        let target = outbound_target(&endpoint("api.example.com"), "/api/v1/accounts/42", &[]);

        assert_eq!(target.scheme, "https");
        assert_eq!(target.authority, "api.example.com");
        assert_eq!(target.uri, "https://api.example.com/api/v1/accounts/42");
    }

    #[test]
    fn the_outbound_target_keeps_a_non_default_port_and_the_allowlisted_query() {
        let mut https = endpoint("api.example.com");
        https.port = 8443;

        let target = outbound_target(&https, "/api/v1", &params(&[("page", "2")]));

        assert_eq!(target.authority, "api.example.com:8443");
        assert_eq!(target.uri, "https://api.example.com:8443/api/v1?page=2");

        let plaintext = Endpoint::new("http", "api.example.com", 80);
        let target = outbound_target(&plaintext, "/api/v1", &[]);
        assert_eq!(target.authority, "api.example.com");
        assert_eq!(target.uri, "http://api.example.com/api/v1");
    }

    /// The URI an absolute URI can be dialed with carries the transport scheme:
    /// a WebSocket endpoint is dialed over its `https` transport, the upgrade
    /// semantics riding the request headers, while `OutboundTarget.scheme`
    /// keeps the scheme the endpoint was configured with.
    #[test]
    fn the_outbound_uri_carries_the_transport_scheme_of_a_websocket_endpoint() {
        let mut wss = endpoint("api.example.com");
        wss.scheme = crate::domain::model::SCHEME_WSS.to_owned();

        let target = outbound_target(&wss, "/ws", &[]);

        assert_eq!(target.scheme, "wss", "the configured scheme is kept");
        assert_eq!(target.uri, "https://api.example.com/ws");
        // `ws` is no scheme of the closed set, so no endpoint can carry it and
        // `outbound_target` never reads it; the normalisation it names is the
        // plaintext transport all the same.
        assert_eq!(transport_scheme("ws"), SCHEME_HTTP);
        assert_eq!(transport_scheme("wss"), SCHEME_HTTPS);
    }

    /// An IPv6 host is bracketed in the authority, the brackets keeping the
    /// address apart from the port an authority may carry.
    #[test]
    fn the_outbound_authority_brackets_an_ipv6_host() {
        let v6 = Endpoint::new("https", "2001:db8::1", 443);
        let target = outbound_target(&v6, "/api/v1", &[]);
        assert_eq!(target.authority, "[2001:db8::1]");
        assert_eq!(target.uri, "https://[2001:db8::1]/api/v1");

        let mut v6_port = v6;
        v6_port.port = 8443;
        let target = outbound_target(&v6_port, "/api/v1", &[]);
        assert_eq!(target.authority, "[2001:db8::1]:8443");
        assert_eq!(target.uri, "https://[2001:db8::1]:8443/api/v1");
    }

    /// An authority is never bracketed twice, whatever form the host was
    /// configured in, and a name keeps the form it had.
    #[test]
    fn the_outbound_authority_keeps_a_name_untouched() {
        assert_eq!(
            authority_of("https", "api.example.com", 443),
            "api.example.com"
        );
        assert_eq!(authority_of("https", "[2001:db8::1]", 443), "[2001:db8::1]");
        assert_eq!(
            authority_of("https", "[2001:db8::1]", 8443),
            "[2001:db8::1]:8443"
        );
    }

    #[test]
    fn parse_query_keeps_the_parameters_verbatim_and_in_order() {
        let parsed = parse_query("b=2&a=1&flag");

        let names: Vec<&str> = parsed.iter().map(|param| param.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a", "flag"]);
        assert_eq!(parsed[0].raw, "b=2");
        assert_eq!(parsed[2].raw, "flag");
    }

    #[test]
    fn the_twelve_declared_transitions_are_the_only_ones() {
        let declared = [
            (ProxyState::Classified, ProxyState::Routed),
            (ProxyState::Classified, ProxyState::Failed),
            (ProxyState::Routed, ProxyState::EndpointSelected),
            (ProxyState::Routed, ProxyState::Failed),
            (ProxyState::EndpointSelected, ProxyState::Validated),
            (ProxyState::EndpointSelected, ProxyState::Failed),
            (ProxyState::Validated, ProxyState::Dispatched),
            (ProxyState::Validated, ProxyState::Failed),
            (ProxyState::Dispatched, ProxyState::Responded),
            (ProxyState::Dispatched, ProxyState::Failed),
            (ProxyState::Responded, ProxyState::Classified),
            (ProxyState::Failed, ProxyState::Classified),
        ];
        let states = [
            ProxyState::Classified,
            ProxyState::Routed,
            ProxyState::EndpointSelected,
            ProxyState::Validated,
            ProxyState::Dispatched,
            ProxyState::Responded,
            ProxyState::Failed,
        ];
        assert_eq!(declared.len(), 12);

        for from in states {
            for to in states {
                let declared = declared.contains(&(from, to));
                assert_eq!(
                    from.transition(to).is_some(),
                    declared,
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
            }
        }
    }

    #[test]
    fn a_request_walks_the_lifecycle_in_order_and_an_invalid_transition_is_refused() {
        let mut context = ProxyContext::new("payments");
        assert_eq!(context.state(), ProxyState::Classified);
        assert!(context.advance(ProxyState::Dispatched).is_err());
        assert_eq!(context.state(), ProxyState::Classified);

        context.advance(ProxyState::Routed).unwrap();
        context.record_route(Uuid::from_u128(0x3000));
        context.advance(ProxyState::EndpointSelected).unwrap();
        context.record_endpoint("api.example.com", SelectionMethod::ExplicitHeader);
        context.advance(ProxyState::Validated).unwrap();
        context.advance(ProxyState::Dispatched).unwrap();
        context.advance(ProxyState::Responded).unwrap();

        assert_eq!(context.route_id(), Some(Uuid::from_u128(0x3000)));
        assert_eq!(context.endpoint_host(), Some("api.example.com"));
        assert_eq!(
            context.selection_method(),
            Some(SelectionMethod::ExplicitHeader)
        );
        assert_eq!(context.alias(), "payments");
        assert_eq!(context.state().as_str(), "Responded");
    }

    #[test]
    fn the_selection_method_names_the_value_the_metrics_carry() {
        assert_eq!(SelectionMethod::Default.as_str(), "default");
        assert_eq!(SelectionMethod::RoundRobin.as_str(), "round_robin");
        assert_eq!(SelectionMethod::ExplicitHeader.as_str(), "explicit_header");
    }

    #[test]
    fn the_response_rules_apply_set_add_remove() {
        let mut head = HeaderMap::new();
        head.insert(
            HeaderName::from_static("content-type"),
            HeaderValue::from_static("text/plain"),
        );
        head.insert(
            HeaderName::from_static("server"),
            HeaderValue::from_static("upstream"),
        );
        let mut rules = ResponseHeaders::default();
        rules.set.insert("x-set".to_owned(), "one".to_owned());
        rules.add.insert("x-added".to_owned(), "two".to_owned());
        rules.remove.push("server".to_owned());

        let outbound = apply_response_rules(&head, Some(&rules));

        assert_eq!(
            outbound.get("x-set"),
            Some(&HeaderValue::from_static("one"))
        );
        assert_eq!(
            outbound.get("x-added"),
            Some(&HeaderValue::from_static("two"))
        );
        assert_eq!(outbound.get("server"), None);
        assert_eq!(
            outbound.get("content-type"),
            Some(&HeaderValue::from_static("text/plain"))
        );

        let untouched = apply_response_rules(&head, None);
        assert_eq!(
            untouched.get("server"),
            Some(&HeaderValue::from_static("upstream"))
        );
    }
}
