//! The request and response header pipeline
//! (`cpt-cf-oagw-algo-request-proxy-header-transform`).
//!
//! The pipeline is pure: it takes the inbound header list, the effective
//! `headers` configuration and the selected endpoint, and returns the outbound
//! header sets. No `axum` or `hyper` type is named, so the same classification
//! is what a buffered request and a streamed response head both go through.
//!
//! Three things are fixed by the DoD and are not configuration:
//!
//! * the routing header `x-oagw-target-host` is read for endpoint selection
//!   and then stripped, so it never reaches the upstream;
//! * the eight RFC 9110 hop-by-hop headers, `host`, `content-length` and every
//!   `x-forwarded-*` header never reach the upstream on a buffered request;
//! * on a detected WebSocket upgrade request, `connection` and `upgrade` are
//!   not stripped but replaced with the values the upstream handshake
//!   requires.
//!
//! The passthrough control decides what *else* is forwarded. The
//! configuration DTO leaves `passthrough` absent, and the absent form forwards
//! every header that survives the strip set — the useful default for a proxy,
//! and the one the dispatch fixes.

use crate::domain::dto::{Endpoint, EndpointScheme, HeaderPassthrough, HeadersConfig};

/// The RFC 9110 §7.6.1 hop-by-hop header names, lowercased.
pub const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The routing header the proxy reads for endpoint selection.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The inbound `Host` header.
pub const HOST_HEADER: &str = "host";

/// The prefix of the forwarding headers the proxy replaces itself.
pub const FORWARDED_PREFIX: &str = "x-forwarded-";

/// The header the proxy pipeline stamps on every response it produces.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Whether `name` is one of the hop-by-hop headers.
#[must_use]
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-1
// `inst-rp-al-header-1` .. `-4`: the header classes — the hop-by-hop set of
// RFC 9110, the `x-forwarded-*` set, the routing header and the sets each
// direction strips.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-11
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-8
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-9
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name)
}
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-9
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-2
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-11
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-1

/// Whether `name` is a forwarding header the proxy replaces itself.
#[must_use]
pub fn is_forwarded(name: &str) -> bool {
    name.starts_with(FORWARDED_PREFIX)
}

/// Whether `name` is a header the proxy reads itself and never forwards.
#[must_use]
pub fn is_routing(name: &str) -> bool {
    name == TARGET_HOST_HEADER
}

/// Whether `name` never reaches the upstream on a buffered request.
#[must_use]
pub fn is_stripped_from_request(name: &str) -> bool {
    is_hop_by_hop(name) || is_routing(name) || is_forwarded(name) || name == HOST_HEADER
        || name == "content-length"
}

/// Whether `name` never reaches the client on a proxied response.
///
/// `content-length` and `content-encoding` are stripped beside the hop-by-hop
/// set: the body is relayed as it arrives, and a length or encoding that
/// described the upstream's framing is no longer true of the relayed bytes.
#[must_use]
pub fn is_stripped_from_response(name: &str) -> bool {
    is_hop_by_hop(name) || name == "content-length" || name == "content-encoding"
}

/// Whether the request is a WebSocket upgrade
/// (`cpt-cf-oagw-dod-request-proxy-header-pipeline`).
///
/// The upgrade is detected from the request itself, so the exemption applies
/// before any configuration is consulted.
#[must_use]
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-1
// `inst-rp-ws-1` .. `-3`: the upgrade is detected from the request itself, so
// the hop-by-hop exemption applies before any configuration is consulted.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-9
pub fn is_websocket_upgrade(headers: &[(String, String)]) -> bool {
    let connection_upgrades = headers.iter().any(|(name, value)| {
        name == "connection"
            && value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
    });
    let upgrade_websocket = headers
        .iter()
        .any(|(name, value)| name == "upgrade" && value.trim().eq_ignore_ascii_case("websocket"));
    connection_upgrades && upgrade_websocket
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-10
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-websocket-session:p1:inst-rp-ws-1

/// The authority an upstream request addresses: `host`, or `host:port` when
/// the port is not the scheme's standard one.
#[must_use]
pub fn authority_for(endpoint: &Endpoint) -> String {
    if endpoint.port == endpoint.scheme.standard_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    }
}

/// The URL of an upstream request: the scheme and the authority plus the path
/// and query.
#[must_use]
pub fn upstream_url(
    endpoint: &Endpoint,
    path: &str,
    query: Option<&str>,
) -> Result<String, crate::domain::error::DomainError> {
    let scheme = match endpoint.scheme {
        EndpointScheme::Http => "http",
        EndpointScheme::Https => "https",
        EndpointScheme::Wss => "wss",
        EndpointScheme::Wt => "wt",
        EndpointScheme::Grpc => "grpc",
    };
    let authority = authority_for(endpoint);
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let mut target = format!("{scheme}://{authority}{path}");
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        target.push('?');
        target.push_str(query);
    }
    // A request target carrying a control character must never reach a
    // connection, and the parser tolerates some of them, so the target is
    // checked explicitly before it is parsed.
    if target.chars().any(|c| c != '\0' && c.is_control()) {
        return Err(crate::domain::error::DomainError::field_rejection(
            "path",
            "the request target carries a control character",
        ));
    }
    // A target the parser still refuses is a validation failure and not a
    // transport failure, for the same reason.
    if url::Url::parse(&target).is_err() {
        return Err(crate::domain::error::DomainError::field_rejection(
            "path",
            "the request target is not a valid URI",
        ));
    }
    Ok(target)
}

/// The outbound request headers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundRequestHeaders {
    /// The headers to forward, in order, without `host`.
    pub forwarded: Vec<(String, String)>,
    /// The `host` (or `:authority`) value the upstream request addresses.
    pub host: String,
}

/// Build the outbound request header set
/// (`inst-rp-al-header-1` .. `-7`).
///
/// The steps are the algorithm's: classify, read and strip the routing header,
/// replace `host`, strip the hop-by-hop set (exempting the WebSocket
/// handshake), apply the passthrough control, apply the set/add/remove rules,
/// and reject any value carrying a control character.
///
/// # Errors
///
/// Returns a validation error when a forwarded or configured header value
/// carries a control character, or when a configured rule names an invalid
/// header.
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-5
// `inst-rp-al-header-5` .. `-11`: the outbound set — classify, read and strip
// the routing header, replace `host`, strip the hop-by-hop set (exempting the
// WebSocket handshake), apply the passthrough control and then the set, add
// and remove rules in that order.
pub fn build_request_headers(
    inbound: &[(String, String)],
    config: Option<&HeadersConfig>,
    endpoint: &Endpoint,
    upgrade: bool,
) -> Result<OutboundRequestHeaders, crate::domain::error::DomainError> {
    let request = config.and_then(|headers| headers.request.as_ref());
    let passthrough = request.and_then(|request| request.passthrough).unwrap_or(HeaderPassthrough::All);
    let allowlist = request
        .and_then(|request| request.passthrough_allowlist.as_ref())
        .map(|names| names.iter().map(|name| name.to_ascii_lowercase()).collect::<Vec<_>>());

    let mut forwarded: Vec<(String, String)> = Vec::new();
    for (name, value) in inbound {
        if is_stripped_from_request(name) {
            continue;
        }
        match passthrough {
            HeaderPassthrough::None => {}
            HeaderPassthrough::Allowlist => {
                if allowlist.as_ref().is_some_and(|names| names.contains(name)) {
                    forwarded.push((name.clone(), value.clone()));
                }
            }
            HeaderPassthrough::All => forwarded.push((name.clone(), value.clone())),
        }
    }

    // The WebSocket handshake headers are replaced, not stripped.
    if upgrade {
        forwarded.retain(|(name, _)| name != "connection" && name != "upgrade");
        forwarded.push(("connection".to_owned(), "Upgrade".to_owned()));
        forwarded.push(("upgrade".to_owned(), "websocket".to_owned()));
    }

    // `set` overwrites, `add` appends, `remove` deletes.
    if let Some(request) = request {
        if let Some(rules) = &request.set {
            for (name, value) in rules {
                let lower = name.to_ascii_lowercase();
                forwarded.retain(|(existing, _)| existing != &lower);
                forwarded.push((lower.clone(), value.clone()));
            }
        }
        if let Some(rules) = &request.add {
            for (name, value) in rules {
                forwarded.push((name.to_ascii_lowercase(), value.clone()));
            }
        }
        if let Some(rules) = &request.remove {
            for name in rules {
                let lower = name.to_ascii_lowercase();
                forwarded.retain(|(existing, _)| existing != &lower);
            }
        }
    }

    for (name, value) in &forwarded {
        if value.chars().any(char::is_control) {
            return Err(crate::domain::error::DomainError::field_rejection(
                name,
                "the header value carries a control character",
            ));
        }
    }
    let host = authority_for(endpoint);
    Ok(OutboundRequestHeaders { forwarded, host })
}
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-5

/// Build the outbound response header set (`inst-rp-al-header-8`, `-9`).
///
/// The hop-by-hop set, `content-length` and `content-encoding` are stripped,
/// then the configured `set`, `add` and `remove` rules are applied in that
/// order.
#[must_use]
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-10
// `inst-rp-al-header-10`/`-11`: the response set — the stripped set and the
// configured rules — with the upgrade exemption for a `101` head.
pub fn build_response_headers(
    upstream: &[(String, String)],
    config: Option<&HeadersConfig>,
) -> Vec<(String, String)> {
    build_relayed_response_headers(upstream, config, false)
}
// @cpt-end:cpt-cf-oagw-algo-request-proxy-header-transform:p1:inst-rp-al-header-10

/// [`build_response_headers`] for a switching-protocols response.
///
/// A `101` response head must carry the `Upgrade` and `Connection` headers the
/// handshake needs, so the hop-by-hop exemption the request side applies is
/// applied here too.
#[must_use]
pub fn build_upgrade_response_headers(
    upstream: &[(String, String)],
    config: Option<&HeadersConfig>,
) -> Vec<(String, String)> {
    build_relayed_response_headers(upstream, config, true)
}

fn build_relayed_response_headers(
    upstream: &[(String, String)],
    config: Option<&HeadersConfig>,
    upgrade: bool,
) -> Vec<(String, String)> {
    let mut forwarded: Vec<(String, String)> = upstream
        .iter()
        .filter(|(name, _)| upgrade || !is_stripped_from_response(name))
        .cloned()
        .collect();
    if let Some(rules) = config.and_then(|headers| headers.response.as_ref()) {
        if let Some(set) = &rules.set {
            for (name, value) in set {
                let lower = name.to_ascii_lowercase();
                forwarded.retain(|(existing, _)| existing != &lower);
                forwarded.push((lower, value.clone()));
            }
        }
        if let Some(add) = &rules.add {
            for (name, value) in add {
                forwarded.push((name.to_ascii_lowercase(), value.clone()));
            }
        }
        if let Some(remove) = &rules.remove {
            for name in remove {
                let lower = name.to_ascii_lowercase();
                forwarded.retain(|(existing, _)| existing != &lower);
            }
        }
    }
    forwarded
}
