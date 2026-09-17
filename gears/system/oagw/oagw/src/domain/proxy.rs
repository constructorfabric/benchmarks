//! Pure decision logic of the proxy data plane
//! ([DESIGN.md](../../../docs/DESIGN.md) "Headers Transformation",
//! [ADR-0001](../../../docs/ADR/0001-request-routing.md)
//! "X-OAGW-Target-Host Behavior Matrix",
//! [ADR-0007](../../../docs/ADR/0007-error-source-distinction.md)).
//!
//! Every decision the proxy takes on a request lives here, free of transport
//! types: which upstream an alias resolves to, which endpoint it is aimed at,
//! which headers survive the trip out and back, and how a failure maps onto the
//! wire. [`crate::api::rest::proxy`] drives the decisions in the request
//! pipeline and [`crate::infra::http_client`] carries out the dispatch.
//!
//! The decisions are pure functions over the resolved records, so they are
//! unit-testable without an upstream: [`select_endpoint`] only reads the
//! endpoint pool, [`request_header_plan`] only reads the header configuration
//! and [`match_route`] only reads the route table.
//!
//! ## Target-host matrix (ADR-0001)
//!
//! | Endpoints | Alias | `X-OAGW-Target-Host` | Behaviour |
//! |---|---|---|---|
//! | 1 | any | absent | The only endpoint. |
//! | 1 | any | present | Validated, then honoured. |
//! | 2+ | explicit | absent | Round-robin over the pool. |
//! | 2+ | explicit | present | The named endpoint, bypassing round-robin. |
//! | 2+ | common suffix | absent | 400 `~cf.oagw.routing.missing_target_host.v1`. |
//! | 2+ | common suffix | present | The named endpoint. |

use std::net::{IpAddr, Ipv4Addr};

use toolkit_canonical_errors::{CanonicalError, Http, TransportOverride, resource_error};
use toolkit_gts::gts_id;
use toolkit_security::constants::INTERNAL_TOKEN_HEADER;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::{
    OagwError, OagwInvalidTargetHost, OagwMissingTargetHost, OagwUnknownTargetHost,
};
use crate::domain::model::{
    HeaderOps, HeaderPassthrough, HttpMatch, RequestHeaderOps, Route, Upstream, UpstreamEndpoint,
};

// ---------------------------------------------------------------------------
// Wire constants
// ---------------------------------------------------------------------------

/// Hard limit on a proxied request body (100 MiB), shared with
/// `libs/toolkit-gateway/src/forward.rs`.
pub const MAX_PROXY_BODY_BYTES: u64 = 100 * 1024 * 1024;

/// Header a caller may set to pick the endpoint of a multi-endpoint upstream
/// (ADR-0001). Read during routing, then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header distinguishing a gateway error from a passthrough upstream error
/// (ADR-0007). Present on every proxied response.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Value of [`ERROR_SOURCE_HEADER`] for an error the gateway generated.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Value of [`ERROR_SOURCE_HEADER`] for an upstream error passed through.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// The well-known header describing the body, which survives every
/// `passthrough` policy: dropping it would turn every body the upstream reads
/// into an unparseable one.
const CONTENT_TYPE_HEADER: &str = "content-type";

/// Inbound headers that never reach the upstream, per DESIGN.md's
/// transformation table: the hop-by-hop set, the routing headers
/// (`X-OAGW-Target-Host`, `Host`) and the platform's internal token.
///
/// `content-length` is in the set because the outbound client recomputes it
/// from the streamed body.
fn is_stripped(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
            | "x-forwarded-for"
            | "x-forwarded-proto"
            | "x-forwarded-host"
            | TARGET_HOST_HEADER
    ) || name == INTERNAL_TOKEN_HEADER
}

/// `true` when a header name is in the hop-by-hop set and must be dropped
/// rather than forwarded, in either direction.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Normalizes an alias segment of the request path, so `API.Vendor.COM` and
/// `api.vendor.com.` resolve to the stored alias.
#[must_use]
pub fn normalize_alias(segment: &str) -> String {
    alias::normalize(segment)
}

// ---------------------------------------------------------------------------
// Errors of the proxy path
// ---------------------------------------------------------------------------

/// 400 `cf.oagw.ssrf.blocked.v1` — the SSRF policy blocked the target.
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.ssrf.blocked.v1~"))]
pub struct OagwSsrfBlocked;

/// 403 `cf.oagw.cors.origin_not_allowed.v1` — the origin of an actual
/// cross-origin request is not in the CORS policy of the upstream
/// ([ADR-0004](../../../docs/ADR/0004-cors.md) "Error Responses").
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1~"))]
pub struct OagwCorsOriginNotAllowed;

/// 403 `cf.oagw.cors.method_not_allowed.v1` — the method of an actual
/// cross-origin request is not in the CORS policy of the upstream.
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1~"))]
pub struct OagwCorsMethodNotAllowed;

/// The GTS instance ids of the rows the phase-6 rules name that
/// `toolkit-canonical-errors` cannot carry as a `context.resource_type`: the
/// canonical `ServiceUnavailable` category accepts no resource type, so the
/// disabled-resource 503s render as a bare canonical error. The ids stay here
/// as the traceability anchor of the row, as [`crate::domain::error`] does for
/// its own 503 rows.
pub const UPSTREAM_DISABLED_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.disabled.v1");
pub const ROUTE_DISABLED_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.disabled.v1");
pub const DOWNSTREAM_UNAVAILABLE_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.unavailable.v1");

/// The GTS instance ids of the two ADR-0004 CORS rejections, which the proxy
/// reports as gateway errors (see [`ProxyError::CorsOriginNotAllowed`]).
pub const CORS_ORIGIN_NOT_ALLOWED_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
pub const CORS_METHOD_NOT_ALLOWED_GTS_ID: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");

/// 502 status override for the `Internal` mapping (defaults to 500).
const BAD_GATEWAY_STATUS: TransportOverride = Http::status_code(502);

/// An error the proxy path reports, with the wire identity of the phase-6 rows
/// of DESIGN.md's error table and the routing payloads ADR-0007 documents.
///
/// The target-host variants duplicate [`OagwError`]'s because ADR-0007 requires
/// them to carry `valid_hosts` / `invalid_value` on the wire, which the
/// management-API variants have nowhere to put.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxyError {
    /// 503 `cf.oagw.upstream.disabled.v1`.
    #[error("upstream '{alias}' is disabled")]
    UpstreamDisabled {
        /// Alias the request resolved through.
        alias: String,
    },
    /// 503 `cf.oagw.route.disabled.v1`.
    #[error("route '{route_id}' is disabled")]
    RouteDisabled {
        /// Id of the disabled route.
        route_id: Uuid,
    },
    /// 400 `cf.oagw.routing.missing_target_host.v1`.
    #[error("X-OAGW-Target-Host header is required: {message}")]
    MissingTargetHost {
        /// Human-readable, client-safe detail.
        message: String,
        /// Hosts the caller may name, as ADR-0007's `valid_hosts`.
        valid_hosts: Vec<String>,
    },
    /// 400 `cf.oagw.routing.invalid_target_host.v1`.
    #[error("invalid X-OAGW-Target-Host value: {message}")]
    InvalidTargetHost {
        /// Human-readable, client-safe detail.
        message: String,
        /// The rejected value, as ADR-0007's `invalid_value`.
        invalid_value: String,
    },
    /// 400 `cf.oagw.routing.unknown_target_host.v1`.
    #[error("unknown X-OAGW-Target-Host value: {message}")]
    UnknownTargetHost {
        /// Human-readable, client-safe detail.
        message: String,
        /// The rejected value, as ADR-0007's `invalid_value`.
        invalid_value: String,
        /// Hosts the caller may have meant, as ADR-0007's `valid_hosts`.
        valid_hosts: Vec<String>,
    },
    /// 502 `cf.oagw.downstream.unavailable.v1` — connect or I/O failure.
    #[error("the upstream could not be reached: {message}")]
    DownstreamUnavailable {
        /// Human-readable, client-safe description of the failure.
        message: String,
    },
    /// 400 `cf.oagw.ssrf.blocked.v1`.
    #[error("blocked by the SSRF policy: {message}")]
    SsrfBlocked {
        /// Human-readable, client-safe reason for the block.
        message: String,
    },
    /// 403 `cf.oagw.cors.origin_not_allowed.v1` — the origin of an actual
    /// cross-origin request is not in the merged CORS policy (ADR-0004).
    #[error("CORS origin '{origin}' is not allowed")]
    CorsOriginNotAllowed {
        /// The origin the request came from, as ADR-0007's `invalid_value`.
        origin: String,
    },
    /// 403 `cf.oagw.cors.method_not_allowed.v1` — the method of an actual
    /// cross-origin request is not in the merged CORS policy (ADR-0004).
    #[error("CORS method '{method}' is not allowed")]
    CorsMethodNotAllowed {
        /// The method the request asked for, as ADR-0007's `invalid_value`.
        method: String,
    },
    /// 429 `cf.oagw.rate_limit.exceeded.v1` with the response headers of the
    /// decision (ADR-0003): `Retry-After` always, `X-RateLimit-*` when the
    /// policy has them on. A plain `Domain` cannot carry them, and a 429
    /// without `Retry-After` is not the 429 the ADR describes.
    #[error("rate limit exceeded: {error}")]
    RateLimited {
        /// The rejection itself, rendered as the problem body.
        error: OagwError,
        /// The decision's headers, as `(name, value)` pairs.
        headers: Vec<(String, String)>,
    },
    /// Any other domain error: alias resolution, route resolution, rate
    /// limiting, plugin execution.
    #[error(transparent)]
    Domain(#[from] OagwError),
}

impl ProxyError {
    /// HTTP status of this error, per DESIGN.md's error table.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::UpstreamDisabled { .. } | Self::RouteDisabled { .. } => 503,
            Self::DownstreamUnavailable { .. } => 502,
            Self::MissingTargetHost { .. }
            | Self::InvalidTargetHost { .. }
            | Self::UnknownTargetHost { .. }
            | Self::SsrfBlocked { .. } => 400,
            Self::CorsOriginNotAllowed { .. } | Self::CorsMethodNotAllowed { .. } => 403,
            Self::RateLimited { .. } => 429,
            Self::Domain(error) => error.http_status(),
        }
    }

    /// The canonical error the proxy renders as `application/problem+json`.
    ///
    /// The category is the canonical one of the OAGW row (house convention
    /// since phase 2: the wire `type` is the canonical category and the OAGW id
    /// travels in `context.resource_type`).
    #[must_use]
    pub fn canonical(&self) -> CanonicalError {
        match self {
            Self::UpstreamDisabled { alias } => CanonicalError::service_unavailable()
                .with_detail(format!("the upstream '{alias}' is disabled"))
                .create(),
            Self::RouteDisabled { route_id } => CanonicalError::service_unavailable()
                .with_detail(format!("the matched route '{route_id}' is disabled"))
                .create(),
            Self::MissingTargetHost {
                message,
                valid_hosts,
            } => OagwMissingTargetHost::invalid_argument()
                .with_format(message_with_hosts(message, valid_hosts))
                .create(),
            Self::InvalidTargetHost {
                message,
                invalid_value,
            } => OagwInvalidTargetHost::invalid_argument()
                .with_format(format!("{message} (invalid_value: '{invalid_value}')"))
                .create(),
            Self::UnknownTargetHost {
                message,
                invalid_value,
                ..
            } => OagwUnknownTargetHost::invalid_argument()
                .with_format(format!("{message} (invalid_value: '{invalid_value}')"))
                .create(),
            Self::DownstreamUnavailable { message } => CanonicalError::internal(message.clone())
                .with_override(BAD_GATEWAY_STATUS)
                .create(),
            Self::SsrfBlocked { message } => OagwSsrfBlocked::invalid_argument()
                .with_format(message.clone())
                .create(),
            // A CORS rejection is the gateway refusing the caller, which is a
            // permission denial. The client-safe reason the ADR-0004 examples
            // show travels in the `reason` of the canonical context, next to
            // the OAGW row id in `resource_type`.
            Self::CorsOriginNotAllowed { origin } => OagwCorsOriginNotAllowed::permission_denied()
                .with_reason(format!("Origin '{origin}' not in allowed origins list"))
                .create(),
            Self::CorsMethodNotAllowed { method } => OagwCorsMethodNotAllowed::permission_denied()
                .with_reason(format!("Method '{method}' not in allowed methods list"))
                .create(),
            Self::RateLimited { error, .. } => CanonicalError::from(error.clone()),
            Self::Domain(error) => CanonicalError::from(error.clone()),
        }
    }
}

/// `detail` of a target-host rejection, with the ADR-0007 host list appended.
fn message_with_hosts(message: &str, valid_hosts: &[String]) -> String {
    match valid_hosts {
        [] => message.to_owned(),
        hosts => format!("{message} valid_hosts: [{}]", hosts.join(", ")),
    }
}

// ---------------------------------------------------------------------------
// Endpoint selection
// ---------------------------------------------------------------------------

/// How the target endpoint of a proxied request was chosen
/// (`oagw_routing_endpoint_selected` label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMethod {
    /// `X-OAGW-Target-Host` named the endpoint.
    ExplicitHeader,
    /// The upstream declares one endpoint, so there was nothing to choose.
    Default,
    /// Round-robin over the endpoint pool.
    RoundRobin,
}

/// The endpoint a proxied request is aimed at, and how it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedEndpoint<'a> {
    /// Endpoint the request goes to.
    pub endpoint: &'a UpstreamEndpoint,
    /// How the endpoint was chosen.
    pub method: SelectionMethod,
}

/// `true` when the upstream's alias is the one its endpoints derive, i.e. the
/// ADR-0001 "common suffix" alias that needs `X-OAGW-Target-Host` to
/// disambiguate the pool.
#[must_use]
pub fn has_common_suffix_alias(upstream: &Upstream) -> bool {
    upstream.alias.as_ref().is_some_and(|alias| {
        alias::derive(&upstream.server.endpoints).is_some_and(|derived| derived == *alias)
    })
}

/// The hosts a caller may name with `X-OAGW-Target-Host`, normalized.
#[must_use]
pub fn valid_hosts(upstream: &Upstream) -> Vec<String> {
    upstream
        .server
        .endpoints
        .iter()
        .map(|endpoint| alias::normalize(&endpoint.host))
        .collect()
}

/// Parses a `X-OAGW-Target-Host` value: a bare hostname or IP literal, with no
/// port, path or separator.
fn parse_target_host(value: &str) -> Option<String> {
    let normalized = alias::normalize(value.trim());
    if normalized.is_empty()
        || normalized.contains([':', '/', '?', '#', '@', '%', '\\', ' '])
        || !alias::is_valid_hostname(&normalized)
    {
        return None;
    }
    Some(normalized)
}

/// Selects the endpoint of `upstream` the request goes to.
///
/// `target_host` is the already-read `X-OAGW-Target-Host` value, `round_robin`
/// the counter the caller increments per request (only its remainder modulo the
/// pool size is significant).
///
/// # Errors
/// [`ProxyError::InvalidTargetHost`] when the header value is not a hostname,
/// [`ProxyError::UnknownTargetHost`] when it names no endpoint of the upstream
/// and [`ProxyError::MissingTargetHost`] when a common-suffix pool cannot be
/// disambiguated without it.
pub fn select_endpoint<'a>(
    upstream: &'a Upstream,
    target_host: Option<&str>,
    round_robin: u64,
) -> Result<SelectedEndpoint<'a>, ProxyError> {
    let endpoints = &upstream.server.endpoints;
    let hosts = valid_hosts(upstream);
    if let Some(value) = target_host {
        let requested = parse_target_host(value).ok_or_else(|| ProxyError::InvalidTargetHost {
            message: "X-OAGW-Target-Host must be a hostname or IP address, without a port, path \
                      or special characters"
                .to_owned(),
            invalid_value: value.to_owned(),
        })?;
        return match endpoints
            .iter()
            .find(|endpoint| alias::normalize(&endpoint.host) == requested)
        {
            Some(endpoint) => Ok(SelectedEndpoint {
                endpoint,
                method: SelectionMethod::ExplicitHeader,
            }),
            None => Err(ProxyError::UnknownTargetHost {
                message: format!(
                    "X-OAGW-Target-Host '{requested}' does not match any configured endpoint"
                ),
                invalid_value: requested,
                valid_hosts: hosts,
            }),
        };
    }
    let endpoint = match endpoints.as_slice() {
        [only] => only,
        pool => {
            if has_common_suffix_alias(upstream) {
                return Err(ProxyError::MissingTargetHost {
                    message: "X-OAGW-Target-Host is required for a multi-endpoint upstream with \
                              a common-suffix alias"
                        .to_owned(),
                    valid_hosts: hosts,
                });
            }
            let index =
                usize::try_from(round_robin % u64::try_from(pool.len()).unwrap_or(1)).unwrap_or(0);
            &pool[index]
        }
    };
    Ok(SelectedEndpoint {
        method: SelectionMethod::Default,
        endpoint,
    })
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// `true` when `method` is allowed by the route's match rule.
///
/// `HEAD` is served by a route that declares `GET` — the two differ only in the
/// body, which the proxy strips. `OPTIONS` has no [`HttpMethod`] variant, so it
/// matches nothing and falls through to the 404.
#[must_use]
fn method_allowed(match_rule: &HttpMatch, method: &str) -> bool {
    let method = if method == "HEAD" { "GET" } else { method };
    match_rule
        .methods
        .iter()
        .any(|allowed| allowed.as_str() == method)
}

/// The upstream path of a request that matched `route_path` as a prefix, or
/// `None` when the request path does not extend it on a segment boundary.
fn joined_path(route_path: &str, request_path: &str) -> Option<String> {
    let remainder = if request_path == route_path {
        ""
    } else if route_path.ends_with('/') {
        request_path.strip_prefix(route_path)?
    } else {
        request_path
            .strip_prefix(route_path)
            .and_then(|rest| rest.strip_prefix('/'))?
    };
    Some(match remainder {
        "" => route_path.to_owned(),
        suffix => format!("{}/{}", route_path.trim_end_matches('/'), suffix),
    })
}

/// Resolves the route of a request among the `routes` of one upstream.
///
/// Returns the route and the upstream path it maps the request to: the route's
/// path, plus the matched suffix when the route allows one. The longest prefix
/// wins, so `/v1/chat` is preferred over `/v1`.
#[must_use]
pub fn match_route<'a>(
    routes: &'a [Route],
    method: &str,
    request_path: &str,
) -> Option<(&'a Route, String)> {
    let mut best: Option<(&'a Route, String, usize)> = None;
    for route in routes {
        let Some(http) = route.match_rule.http.as_ref() else {
            continue;
        };
        if !method_allowed(http, method) {
            continue;
        }
        let Some(path) = joined_path(&http.path, request_path) else {
            continue;
        };
        let exact = path == http.path;
        if !exact && !http.path_suffix_mode.allows_suffix() {
            continue;
        }
        let specificity = if exact { usize::MAX } else { http.path.len() };
        let takes_over = best
            .as_ref()
            .is_none_or(|(_, _, best_score)| specificity > *best_score);
        if takes_over {
            best = Some((route, path, specificity));
        }
    }
    best.map(|(route, path, _)| (route, path))
}

// ---------------------------------------------------------------------------
// Header plans
// ---------------------------------------------------------------------------

/// The header transformation the proxy applies on the way out
/// (DESIGN.md "Headers Transformation").
///
/// Plain data, so the api layer can apply it to a [`http::HeaderMap`] without
/// the domain layer knowing the transport type.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestHeaderPlan {
    /// Value of the outbound `Host` header: the endpoint host, plus `:port`
    /// when the endpoint does not use its scheme's standard port.
    pub host: String,
    /// Inbound headers forwarded to the upstream, in inbound order.
    pub forward: Vec<(String, String)>,
    /// `headers.request.set` of the upstream, then of the route.
    pub set: Vec<(String, String)>,
    /// `headers.request.add` of the upstream, then of the route.
    pub add: Vec<(String, String)>,
}

/// The header transformation the proxy applies to the response on the way back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResponseHeaderPlan {
    /// `headers.response.set` of the upstream, then of the route.
    pub set: Vec<(String, String)>,
    /// `headers.response.add` of the upstream, then of the route.
    pub add: Vec<(String, String)>,
    /// `headers.response.remove` of the upstream, then of the route.
    pub remove: Vec<String>,
}

/// The `host[:port]` authority of `endpoint`, for the outbound `Host` header.
#[must_use]
pub fn endpoint_authority(endpoint: &UpstreamEndpoint) -> String {
    let host = alias::normalize(&endpoint.host);
    if alias::is_standard_port(endpoint.scheme, endpoint.port) {
        host
    } else {
        format!("{host}:{}", endpoint.port)
    }
}

/// Builds the outbound header plan from the inbound headers and the header
/// configuration of the upstream and of the matched route.
///
/// The upstream's rules apply before the route's, then the plugin transforms
/// run on the result (`request_headers` on the request context). `passthrough`
/// governs which inbound headers survive; `content-type` always survives, since
/// it describes the body the upstream is about to read.
#[must_use]
pub fn request_header_plan(
    endpoint: &UpstreamEndpoint,
    inbound: &[(String, String)],
    upstream_headers: Option<&HeaderOps>,
    route_headers: Option<&HeaderOps>,
) -> RequestHeaderPlan {
    let request =
        upstream_headers.map_or_else(RequestHeaderOps::default, |ops| ops.request.clone());
    let mut forward = Vec::new();
    for (name, value) in inbound {
        let name = name.to_ascii_lowercase();
        if is_stripped(&name) {
            continue;
        }
        if name == CONTENT_TYPE_HEADER
            || match request.passthrough {
                HeaderPassthrough::None => false,
                HeaderPassthrough::All => true,
                HeaderPassthrough::Allowlist => request
                    .passthrough_allowlist
                    .iter()
                    .any(|allowed| allowed.to_ascii_lowercase() == name),
            }
        {
            forward.push((name, value.clone()));
        }
    }
    let mut set = Vec::new();
    let mut add = Vec::new();
    for ops in [upstream_headers, route_headers].into_iter().flatten() {
        set.extend(
            ops.request
                .set
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        add.extend(
            ops.request
                .add
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
    }
    RequestHeaderPlan {
        host: endpoint_authority(endpoint),
        forward,
        set,
        add,
    }
}

/// Builds the response header plan of the upstream and of the matched route.
///
/// Upstream rules apply before route rules, on the untouched upstream response.
#[must_use]
pub fn response_header_plan(
    upstream_headers: Option<&HeaderOps>,
    route_headers: Option<&HeaderOps>,
) -> ResponseHeaderPlan {
    let mut plan = ResponseHeaderPlan::default();
    for ops in [upstream_headers, route_headers].into_iter().flatten() {
        plan.set.extend(
            ops.response
                .set
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        plan.add.extend(
            ops.response
                .add
                .iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value.clone())),
        );
        plan.remove.extend(
            ops.response
                .remove
                .iter()
                .map(|name| name.to_ascii_lowercase()),
        );
    }
    plan
}

// ---------------------------------------------------------------------------
// Body shape validation
// ---------------------------------------------------------------------------

/// The body shape a proxied request must have for the proxy to stream it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyShape {
    /// No body: the request carries no `Content-Length` and no
    /// `Transfer-Encoding`.
    Empty,
    /// A body of the given length, declared by `Content-Length`.
    Sized(u64),
    /// A chunked body, declared by `Transfer-Encoding: chunked`.
    Chunked,
}

/// Validates the framing headers of a proxied request.
///
/// # Errors
/// [`ProxyError::Domain`] with [`OagwError::PayloadTooLarge`] when a declared
/// `Content-Length` exceeds [`MAX_PROXY_BODY_BYTES`], and with
/// [`OagwError::Validation`] when `Content-Length` is not a plain number or
/// `Transfer-Encoding` names anything but `chunked` (a smuggling vector).
pub fn validate_body_shape(
    content_length: Option<&str>,
    transfer_encoding: Option<&str>,
) -> Result<BodyShape, ProxyError> {
    if let Some(encoding) = transfer_encoding {
        let encoding = encoding.trim();
        if !encoding.eq_ignore_ascii_case("chunked") {
            return Err(OagwError::Validation {
                message: format!(
                    "unsupported Transfer-Encoding '{encoding}': only 'chunked' is proxied"
                ),
            }
            .into());
        }
        return Ok(BodyShape::Chunked);
    }
    let Some(declared) = content_length else {
        return Ok(BodyShape::Empty);
    };
    let declared = declared.trim();
    let Ok(length) = declared.parse::<u64>() else {
        return Err(OagwError::Validation {
            message: format!("invalid Content-Length '{declared}'"),
        }
        .into());
    };
    if length > MAX_PROXY_BODY_BYTES {
        return Err(OagwError::PayloadTooLarge {
            message: format!(
                "the request body exceeds the {} MiB proxy limit",
                MAX_PROXY_BODY_BYTES / (1024 * 1024)
            ),
        }
        .into());
    }
    Ok(BodyShape::Sized(length))
}

// ---------------------------------------------------------------------------
// SSRF policy
// ---------------------------------------------------------------------------

/// Blocks a target host when the SSRF policy is enabled
/// (DESIGN.md "Security": private, loopback, link-local and metadata targets).
///
/// Hostnames are only checked against the well-known metadata names: a
/// hostname that *resolves* to a private address is caught at connect time by
/// the connector, not here.
///
/// Returns the reason the host is blocked, or `None` when it may be dialed.
#[must_use]
pub fn ssrf_rejection(host: &str) -> Option<&'static str> {
    let host = alias::normalize(host);
    if matches!(host.as_str(), "metadata.google.internal" | "metadata") {
        return Some("cloud metadata service");
    }
    let address: IpAddr = host.parse().ok()?;
    match address {
        IpAddr::V4(ipv4) => blocked_ipv4(ipv4),
        IpAddr::V6(ipv6) => {
            if ipv6.is_loopback() || ipv6.is_unspecified() {
                return Some("IPv6 loopback or unspecified address");
            }
            let segments = ipv6.segments();
            // IPv4-mapped and IPv4-compatible addresses fall back to the IPv4
            // rules, so `::ffff:10.0.0.1` is as blocked as `10.0.0.1`.
            if let Some(ipv4) = ipv6.to_ipv4_mapped() {
                return blocked_ipv4(ipv4);
            }
            let unique_local = segments[0] & 0xfe00 == 0xfc00;
            let link_local = segments[0] & 0xffc0 == 0xfe80;
            (unique_local || link_local).then_some("IPv6 unique-local or link-local address")
        }
    }
}

/// The reason `address` is a blocked IPv4 target, if it is one.
fn blocked_ipv4(address: Ipv4Addr) -> Option<&'static str> {
    let octets = address.octets();
    let blocked: &[(&[u8], usize, &str)] = &[
        (&[127], 8, "IPv4 loopback address"),
        (&[10], 8, "private IPv4 address"),
        (&[192, 168], 16, "private IPv4 address"),
        (&[172, 16], 12, "private IPv4 address"),
        (&[169, 254], 16, "IPv4 link-local address"),
        (&[0], 8, "unspecified IPv4 address"),
        (&[100, 64], 10, "shared address space"),
        (&[198, 18], 15, "benchmarking address space"),
        (&[224], 4, "IPv4 multicast address"),
        (&[240], 4, "reserved IPv4 address"),
    ];
    blocked
        .iter()
        .find(|(prefix, bits, _)| {
            let bytes = usize::div_ceil(*bits, 8);
            let head = &octets[..bytes];
            let padding = bytes * 8 - bits;
            head[..bytes - 1] == prefix[..bytes - 1]
                && (head[bytes - 1] >> padding) == (prefix[bytes - 1] >> padding)
        })
        .map(|(_, _, reason)| *reason)
}

/// The upstream record a proxied request resolved to, and the decisions taken
/// about it: everything the pipeline needs in one place.
#[derive(Debug)]
pub struct ResolvedRequest {
    /// The upstream the request goes to.
    pub upstream: Upstream,
    /// The route that matched, when one did.
    pub route: Option<Route>,
    /// The path the upstream receives.
    pub upstream_path: String,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use crate::domain::model::{
        Alias, EndpointScheme, HeaderOps, HeaderPassthrough, HttpMatch, HttpMethod, PathSuffixMode,
        Protocol, RequestHeaderOps, ResponseHeaderOps, Route, RouteMatch, Upstream,
        UpstreamEndpoint, UpstreamServer,
    };

    use super::{
        BodyShape, MAX_PROXY_BODY_BYTES, ProxyError, SelectionMethod, endpoint_authority,
        has_common_suffix_alias, joined_path, match_route, parse_target_host, request_header_plan,
        response_header_plan, select_endpoint, ssrf_rejection, validate_body_shape,
    };

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> UpstreamEndpoint {
        UpstreamEndpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(alias: Option<&str>, endpoints: &[UpstreamEndpoint]) -> Upstream {
        Upstream {
            id: Some(uuid::Uuid::now_v7()),
            enabled: true,
            alias: alias.map(|alias| Alias::try_new(alias).unwrap()),
            tags: vec![],
            server: UpstreamServer {
                endpoints: endpoints.to_vec(),
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    fn route(path: &str, methods: &[HttpMethod], suffix_mode: PathSuffixMode) -> Route {
        Route {
            id: Some(uuid::Uuid::now_v7()),
            tags: vec![],
            upstream_id: uuid::Uuid::now_v7(),
            match_rule: RouteMatch {
                http: Some(HttpMatch {
                    methods: methods.to_vec(),
                    path: path.to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: suffix_mode,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn the_strip_set_covers_hop_by_hop_and_routing_headers() {
        for name in [
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "host",
            "content-length",
            "x-oagw-target-host",
            "x-toolkit-internal-token",
        ] {
            assert!(
                super::is_stripped(name),
                "{name} must be stripped from the outbound request"
            );
        }
        assert!(!super::is_stripped("authorization"));
        assert!(!super::is_stripped("content-type"));
    }

    #[test]
    fn a_single_endpoint_is_selected_by_default() {
        let upstream = upstream(
            None,
            &[endpoint(EndpointScheme::Https, "api.openai.com", 443)],
        );
        let selected = select_endpoint(&upstream, None, 0).unwrap();
        assert_eq!(selected.method, SelectionMethod::Default);
        assert_eq!(selected.endpoint.host, "api.openai.com");

        // A header naming the endpoint is validated and honoured.
        let selected = select_endpoint(&upstream, Some("API.OpenAI.Com."), 0).unwrap();
        assert_eq!(selected.method, SelectionMethod::ExplicitHeader);

        // A header naming another endpoint is rejected with the invalid value.
        let error = select_endpoint(&upstream, Some("eu.openai.com"), 0).unwrap_err();
        assert_eq!(error.http_status(), 400);
        match error {
            ProxyError::UnknownTargetHost {
                invalid_value,
                valid_hosts,
                ..
            } => {
                assert_eq!(invalid_value, "eu.openai.com");
                assert_eq!(valid_hosts, vec!["api.openai.com"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn a_malformed_target_host_is_an_invalid_value() {
        let upstream = upstream(
            Some("my-service"),
            &[
                endpoint(EndpointScheme::Https, "server-a.example.com", 443),
                endpoint(EndpointScheme::Https, "server-b.example.com", 443),
            ],
        );
        for value in ["server-a.example.com:443", "/server-a", "a b", ""] {
            let error = select_endpoint(&upstream, Some(value), 0).unwrap_err();
            match error {
                ProxyError::InvalidTargetHost { invalid_value, .. } => {
                    assert_eq!(invalid_value, value, "{value}");
                }
                other => panic!("unexpected error for '{value}': {other:?}"),
            }
        }
        assert!(parse_target_host("server-a.example.com.").is_some());
    }

    #[test]
    fn an_explicit_alias_multi_endpoint_pool_load_balances() {
        let upstream = upstream(
            Some("my-service"),
            &[
                endpoint(EndpointScheme::Http, "server-a.example.com", 8080),
                endpoint(EndpointScheme::Http, "server-b.example.com", 8080),
                endpoint(EndpointScheme::Http, "server-c.example.com", 8080),
            ],
        );
        assert!(!has_common_suffix_alias(&upstream));

        let first = select_endpoint(&upstream, None, 0).unwrap();
        let second = select_endpoint(&upstream, None, 1).unwrap();
        let again = select_endpoint(&upstream, None, 3).unwrap();
        assert_eq!(first.method, SelectionMethod::Default);
        assert_eq!(first.endpoint.host, "server-a.example.com");
        assert_eq!(second.endpoint.host, "server-b.example.com");
        assert_eq!(again.endpoint.host, "server-a.example.com");

        // The header bypasses the round-robin.
        let named = select_endpoint(&upstream, Some("server-c.example.com"), 1).unwrap();
        assert_eq!(named.method, SelectionMethod::ExplicitHeader);
        assert_eq!(named.endpoint.host, "server-c.example.com");
    }

    #[test]
    fn a_common_suffix_alias_requires_the_target_host() {
        let upstream = upstream(
            Some("vendor.com"),
            &[
                endpoint(EndpointScheme::Https, "us.vendor.com", 443),
                endpoint(EndpointScheme::Https, "eu.vendor.com", 443),
            ],
        );
        assert!(has_common_suffix_alias(&upstream));

        let error = select_endpoint(&upstream, None, 0).unwrap_err();
        assert_eq!(error.http_status(), 400);
        match error {
            ProxyError::MissingTargetHost { valid_hosts, .. } => {
                assert_eq!(valid_hosts, vec!["us.vendor.com", "eu.vendor.com"]);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        let named = select_endpoint(&upstream, Some("eu.vendor.com"), 0).unwrap();
        assert_eq!(named.method, SelectionMethod::ExplicitHeader);
    }

    #[test]
    fn route_matching_prefers_the_longest_prefix() {
        let short = route("/v1", &[HttpMethod::Get], PathSuffixMode::Append);
        let long = route("/v1/chat", &[HttpMethod::Post], PathSuffixMode::Append);
        let exact = route("/v1/status", &[HttpMethod::Get], PathSuffixMode::Disabled);
        let routes = [short.clone(), long.clone(), exact.clone()];

        let (matched, path) = match_route(&routes, "GET", "/v1/chat/completions").unwrap();
        assert_eq!(matched.id, short.id);
        assert_eq!(path, "/v1/chat/completions");

        let (matched, path) = match_route(&routes, "POST", "/v1/chat").unwrap();
        assert_eq!(matched.id, long.id);
        assert_eq!(path, "/v1/chat");

        // A `Disabled` route only matches its own path: the longer `/v1`
        // prefix does not take the request away from it, but `/v1/status/now`
        // is not the route's own path and nothing else may match it.
        let (matched, path) = match_route(&routes, "GET", "/v1/status").unwrap();
        assert_eq!(matched.id, exact.id);
        assert_eq!(path, "/v1/status");

        let only_exact = [exact.clone()];
        assert!(match_route(&only_exact, "GET", "/v1/status/now").is_none());
        assert!(match_route(&only_exact, "GET", "/v1/status").is_some());

        // Method and path both have to match.
        assert!(match_route(&routes, "DELETE", "/v1/chat").is_none());
        assert!(match_route(&routes, "GET", "/other").is_none());
    }

    #[test]
    fn head_is_served_by_a_get_route() {
        let routes = [route("/v1", &[HttpMethod::Get], PathSuffixMode::Append)];
        assert!(match_route(&routes, "HEAD", "/v1/items").is_some());
    }

    #[test]
    fn prefixes_match_on_whole_segments_only() {
        assert_eq!(joined_path("/v1", "/v1chat"), None);
        assert_eq!(joined_path("/v1", "/v1/chat").as_deref(), Some("/v1/chat"));
        assert_eq!(joined_path("/", "/v1/chat").as_deref(), Some("/v1/chat"));
        assert_eq!(joined_path("/v1/", "/v1/chat").as_deref(), Some("/v1/chat"));
        assert_eq!(joined_path("/v1", "/v1").as_deref(), Some("/v1"));
    }

    #[test]
    fn the_outbound_host_is_the_endpoint_authority() {
        assert_eq!(
            endpoint_authority(&endpoint(EndpointScheme::Https, "api.openai.com", 443)),
            "api.openai.com"
        );
        assert_eq!(
            endpoint_authority(&endpoint(EndpointScheme::Http, "api.openai.com", 8080)),
            "api.openai.com:8080"
        );
        assert_eq!(
            endpoint_authority(&endpoint(EndpointScheme::Http, "api.openai.com", 80)),
            "api.openai.com"
        );
    }

    #[test]
    fn the_request_plan_strips_and_forwards_according_to_the_policy() {
        let endpoint = endpoint(EndpointScheme::Http, "us.vendor.com", 80);
        let inbound = [
            ("Host".to_owned(), "oagw.example.com".to_owned()),
            ("Connection".to_owned(), "close".to_owned()),
            ("X-OAGW-Target-Host".to_owned(), "us.vendor.com".to_owned()),
            ("Authorization".to_owned(), "Bearer token".to_owned()),
            ("X-Custom".to_owned(), "1".to_owned()),
            ("Content-Type".to_owned(), "application/json".to_owned()),
        ];

        // `passthrough: none` forwards nothing but the content type. Port 80
        // is the standard plaintext port, so it is omitted from the authority.
        let plan = request_header_plan(&endpoint, &inbound, None, None);
        assert_eq!(plan.host, "us.vendor.com");
        assert_eq!(
            plan.forward,
            vec![("content-type".to_owned(), "application/json".to_owned())]
        );

        // `all` forwards everything but the stripped set.
        let ops = HeaderOps {
            request: RequestHeaderOps {
                passthrough: HeaderPassthrough::All,
                ..Default::default()
            },
            response: ResponseHeaderOps::default(),
        };
        let plan = request_header_plan(&endpoint, &inbound, Some(&ops), None);
        assert_eq!(
            plan.forward,
            vec![
                ("authorization".to_owned(), "Bearer token".to_owned()),
                ("x-custom".to_owned(), "1".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]
        );

        // An allowlist forwards only the listed headers.
        let ops = HeaderOps {
            request: RequestHeaderOps {
                passthrough: HeaderPassthrough::Allowlist,
                passthrough_allowlist: vec!["X-Custom".to_owned()],
                ..Default::default()
            },
            response: ResponseHeaderOps::default(),
        };
        let plan = request_header_plan(&endpoint, &inbound, Some(&ops), None);
        // `content-type` describes the body and always survives the policy.
        assert_eq!(
            plan.forward,
            vec![
                ("x-custom".to_owned(), "1".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]
        );
    }

    #[test]
    fn header_rules_of_the_upstream_apply_before_the_route_ones() {
        let endpoint = endpoint(EndpointScheme::Https, "us.vendor.com", 443);
        let upstream = HeaderOps {
            request: RequestHeaderOps {
                set: [("x-shared".to_owned(), "upstream".to_owned())].into(),
                add: [("x-add".to_owned(), "upstream".to_owned())].into(),
                ..Default::default()
            },
            response: ResponseHeaderOps {
                remove: vec!["Server".to_owned()],
                ..Default::default()
            },
        };
        let route_ops = HeaderOps {
            request: RequestHeaderOps {
                set: [("x-shared".to_owned(), "route".to_owned())].into(),
                ..Default::default()
            },
            response: ResponseHeaderOps::default(),
        };

        let plan = request_header_plan(&endpoint, &[], Some(&upstream), Some(&route_ops));
        assert_eq!(
            plan.set,
            vec![
                ("x-shared".to_owned(), "upstream".to_owned()),
                ("x-shared".to_owned(), "route".to_owned()),
            ]
        );
        assert_eq!(plan.add, vec![("x-add".to_owned(), "upstream".to_owned())]);

        let plan = response_header_plan(Some(&upstream), Some(&route_ops));
        assert_eq!(plan.remove, vec!["server"]);
    }

    #[test]
    fn body_shape_is_taken_from_the_framing_headers() {
        assert_eq!(validate_body_shape(None, None).unwrap(), BodyShape::Empty);
        assert_eq!(
            validate_body_shape(Some(" 12 "), None).unwrap(),
            BodyShape::Sized(12)
        );
        assert_eq!(
            validate_body_shape(None, Some("chunked")).unwrap(),
            BodyShape::Chunked
        );

        let error = validate_body_shape(None, Some("gzip")).unwrap_err();
        assert_eq!(error.http_status(), 400);

        let error = validate_body_shape(Some("abc"), None).unwrap_err();
        assert_eq!(error.http_status(), 400);

        let error = validate_body_shape(Some("104857601"), None).unwrap_err();
        assert_eq!(error.http_status(), 413);
        assert_eq!(MAX_PROXY_BODY_BYTES, 100 * 1024 * 1024);
    }

    #[test]
    fn the_ssrf_policy_blocks_private_and_metadata_targets() {
        for host in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "172.16.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "metadata",
            "metadata.google.internal",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(ssrf_rejection(host).is_some(), "{host} must be blocked");
        }
        for host in ["api.openai.com", "8.8.8.8", "172.32.0.1", "2606:4700::1111"] {
            assert!(ssrf_rejection(host).is_none(), "{host} must be allowed");
        }
        // A hostname is never blocked here: it may resolve anywhere.
        assert!(ssrf_rejection("internal.corp").is_none());
    }

    #[test]
    fn every_proxy_error_maps_onto_its_wire_status() {
        let cases: &[(ProxyError, u16)] = &[
            (
                ProxyError::UpstreamDisabled {
                    alias: "vendor.com".to_owned(),
                },
                503,
            ),
            (
                ProxyError::RouteDisabled {
                    route_id: uuid::Uuid::now_v7(),
                },
                503,
            ),
            (
                ProxyError::DownstreamUnavailable {
                    message: "connection refused".to_owned(),
                },
                502,
            ),
            (
                ProxyError::SsrfBlocked {
                    message: "loopback".to_owned(),
                },
                400,
            ),
            (
                ProxyError::CorsOriginNotAllowed {
                    origin: "https://evil.example.com".to_owned(),
                },
                403,
            ),
            (
                ProxyError::CorsMethodNotAllowed {
                    method: "DELETE".to_owned(),
                },
                403,
            ),
        ];
        for (error, status) in cases {
            assert_eq!(error.http_status(), *status, "{error:?}");
            assert_eq!(error.canonical().status_code(), *status, "{error:?}");
        }
    }
}
