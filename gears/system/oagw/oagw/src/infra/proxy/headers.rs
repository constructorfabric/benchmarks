//! Header rewriting for the proxy hop.
//!
//! Covers the `X-OAGW-Target-Host` behaviour matrix of
//! `docs/ADR/0001-request-routing.md` and the request/response header rules of
//! the `headers` configuration block.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use http::HeaderMap;
use uuid::Uuid;

use crate::domain::model::{Endpoint, HeaderPassthrough, HeadersConfig, Upstream};
use crate::domain::plugin::{INVALID_TARGET_HOST, MISSING_TARGET_HOST, UNKNOWN_TARGET_HOST};

/// Client-supplied endpoint selector (`docs/ADR/0001`).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// `Host` header, rewritten to the selected endpoint.
pub const HOST_HEADER: &str = "host";
/// `Connection`, the only hop-by-hop header a WebSocket upgrade needs.
pub const CONNECTION: &str = "connection";
/// `Upgrade`, likewise.
pub const UPGRADE: &str = "upgrade";
/// `Vary`, always present on a CORS-evaluated response.
pub const VARY: &str = "vary";

/// Headers that must not travel across a proxy boundary (RFC 9110 §7.6.1).
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Whether `name` is hop-by-hop.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|hop| hop.eq_ignore_ascii_case(name))
}

/// Names a `Connection` header lists, which are hop-by-hop by reference.
fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Drop hop-by-hop headers, keeping the two a WebSocket upgrade needs.
///
/// A request carrying `Connection: upgrade` / `Upgrade: websocket` must keep
/// both, or hyper will not negotiate the upgrade on the outbound leg.
pub fn strip_request_hop_by_hop(headers: &mut HeaderMap, upgrade: bool) {
    let tokens = connection_tokens(headers);
    for token in tokens {
        if upgrade && (token == CONNECTION || token == UPGRADE) {
            continue;
        }
        if let Ok(name) = http::HeaderName::try_from(token.as_str()) {
            headers.remove(name);
        }
    }
    for name in HOP_BY_HOP {
        if upgrade && (*name == CONNECTION || *name == UPGRADE) {
            continue;
        }
        if let Ok(header_name) = http::HeaderName::try_from(*name) {
            headers.remove(header_name);
        }
    }
}

/// Drop hop-by-hop headers from an upstream response, keeping the two an
/// upgrade response needs.
pub fn strip_response_hop_by_hop(headers: &mut HeaderMap, upgrade: bool) {
    strip_request_hop_by_hop(headers, upgrade);
}

/// How the inbound request headers reach the upstream.
pub fn inbound_request_headers(
    upstream_headers: Option<&HeadersConfig>,
    inbound: &HeaderMap,
    upgrade: bool,
) -> HeaderMap {
    let mut forwarded = HeaderMap::new();
    let passthrough = upstream_headers
        .map(|headers| headers.request.passthrough)
        .unwrap_or_default();
    let allowlist: Vec<String> = upstream_headers
        .map(|headers| headers.request.passthrough_allowlist.clone())
        .unwrap_or_default();

    match passthrough {
        HeaderPassthrough::All => {
            for (name, value) in inbound.iter() {
                forwarded.insert(name.clone(), value.clone());
            }
        }
        HeaderPassthrough::Allowlist => {
            for name in &allowlist {
                let Ok(header_name) = http::HeaderName::try_from(name.as_str()) else {
                    continue;
                };
                for value in inbound.get_all(&header_name) {
                    forwarded.append(&header_name, value.clone());
                }
            }
        }
        HeaderPassthrough::None => {}
    }
    strip_request_hop_by_hop(&mut forwarded, upgrade);
    forwarded.remove(HOST_HEADER);
    forwarded
}

/// Apply `headers.request` to the outbound header set.
pub fn apply_request_headers(config: Option<&HeadersConfig>, headers: &mut HeaderMap) {
    let Some(config) = config else {
        return;
    };
    for setting in &config.request.remove {
        if let Ok(name) = http::HeaderName::try_from(setting.as_str()) {
            headers.remove(name);
        }
    }
    for setting in &config.request.set {
        set_header(headers, &setting.name, &setting.value);
    }
    for setting in &config.request.add {
        append_header(headers, &setting.name, &setting.value);
    }
}

/// Apply `headers.response` to the response header set.
pub fn apply_response_headers(config: Option<&HeadersConfig>, headers: &mut HeaderMap) {
    let Some(config) = config else {
        return;
    };
    for setting in &config.response.remove {
        if let Ok(name) = http::HeaderName::try_from(setting.as_str()) {
            headers.remove(name);
        }
    }
    for setting in &config.response.set {
        set_header(headers, &setting.name, &setting.value);
    }
    for setting in &config.response.add {
        append_header(headers, &setting.name, &setting.value);
    }
}

fn set_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = http::HeaderName::try_from(name)
        && let Ok(value) = http::HeaderValue::try_from(value)
    {
        headers.insert(name, value);
    }
}

fn append_header(headers: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = http::HeaderName::try_from(name)
        && let Ok(value) = http::HeaderValue::try_from(value)
    {
        headers.append(name, value);
    }
}

/// Round-robin cursor per upstream.
#[derive(Debug, Clone, Default)]
pub struct EndpointCursor {
    cursors: dashmap::DashMap<Uuid, Arc<AtomicU64>>,
}

impl EndpointCursor {
    /// An empty cursor table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn next_index(&self, upstream: &Uuid, len: usize) -> usize {
        let cursor = self
            .cursors
            .entry(*upstream)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .value()
            .clone();
        let previous = cursor.fetch_add(1, Ordering::Relaxed);
        if len == 0 {
            return 0;
        }
        (previous % len as u64) as usize
    }
}

/// Why an endpoint could not be chosen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    /// `X-OAGW-Target-Host` is required for a common-suffix alias.
    #[error(
        "X-OAGW-Target-Host is required: alias '{alias}' covers the endpoints of a common \
         suffix"
    )]
    Missing {
        /// The alias that needed disambiguation.
        alias: String,
    },
    /// `X-OAGW-Target-Host` is not a bare hostname.
    #[error("X-OAGW-Target-Host value is invalid: {0}")]
    Invalid(String),
    /// `X-OAGW-Target-Host` names no configured endpoint.
    #[error("X-OAGW-Target-Host '{0}' does not match any configured endpoint")]
    Unknown(String),
}

/// How an alias relates to its endpoint pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasShape {
    /// The alias is the caller's own name, unrelated to the hostnames.
    Explicit,
    /// The alias is the shared suffix of every pooled endpoint.
    CommonSuffix,
}

/// Classify an alias against its endpoint pool.
#[must_use]
pub fn alias_shape(alias: &str, endpoints: &[Endpoint]) -> AliasShape {
    let dotted = alias.contains('.');
    let shared = endpoints.len() > 1
        && dotted
        && endpoints.iter().all(|endpoint| {
            endpoint.host == alias || endpoint.host.ends_with(&format!(".{alias}"))
        });
    if shared {
        AliasShape::CommonSuffix
    } else {
        AliasShape::Explicit
    }
}

/// Whether a `X-OAGW-Target-Host` value is a bare hostname or IP literal.
///
/// A URL, a socket address or a header-injection attempt is not a hostname, and
/// the gateway refuses it rather than trying to dial it.
#[must_use]
pub fn is_bare_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && !value.contains(':')
        && !value.contains('/')
        && !value.contains('@')
        && !value.contains('?')
        && !value.contains('#')
        && !value.contains('%')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-')
}

/// Resolve `X-OAGW-Target-Host` to a pool position, applying the behaviour
/// matrix of `docs/ADR/0001`.
///
/// | Endpoints | Alias | Header | Behaviour |
/// |---|---|---|---|
/// | 1 | any | absent | the endpoint |
/// | 1 | any | present | validated, then the endpoint |
/// | 2+ | explicit | absent | round-robin |
/// | 2+ | explicit | present | the named endpoint |
/// | 2+ | common suffix | absent | `400 MissingTargetHost` |
/// | 2+ | common suffix | present | the named endpoint |
pub fn select_endpoint(
    alias: &str,
    upstream: &Upstream,
    target_host: Option<&str>,
    cursor: &EndpointCursor,
) -> Result<usize, TargetError> {
    let endpoints = &upstream.server.endpoints;
    let header = target_host.map(str::trim).filter(|value| !value.is_empty());

    if let Some(host) = header {
        if !is_bare_hostname(host) {
            return Err(TargetError::Invalid(format!(
                "must be a bare hostname or IP literal, got '{host}'"
            )));
        }
        if let Some(position) = endpoints
            .iter()
            .position(|endpoint| endpoint.host.eq_ignore_ascii_case(host))
        {
            return Ok(position);
        }
        return Err(TargetError::Unknown(host.to_owned()));
    }

    match endpoints.len() {
        0 => Err(TargetError::Invalid("upstream has no endpoints".to_owned())),
        1 => Ok(0),
        _ if alias_shape(alias, endpoints) == AliasShape::CommonSuffix => {
            Err(TargetError::Missing {
                alias: alias.to_owned(),
            })
        }
        _ => Ok(cursor.next_index(&upstream.id, endpoints.len())),
    }
}

/// Map an endpoint-selection failure onto the proxy problem catalogue.
pub fn target_failure(
    alias: &str,
    error: TargetError,
) -> crate::infra::proxy::failure::ProxyFailure {
    use crate::infra::proxy::failure::ProxyFailure as Failure;
    match error {
        TargetError::Missing { .. } => Failure::new(
            400,
            MISSING_TARGET_HOST,
            "Missing Target Host",
            format!(
                "X-OAGW-Target-Host is required: alias '{alias}' covers several endpoints \
                 sharing the alias as a common suffix"
            ),
        ),
        TargetError::Invalid(reason) => Failure::new(
            400,
            INVALID_TARGET_HOST,
            "Invalid Target Host",
            format!("X-OAGW-Target-Host value is invalid: {reason}"),
        ),
        TargetError::Unknown(host) => Failure::new(
            400,
            UNKNOWN_TARGET_HOST,
            "Unknown Target Host",
            format!("X-OAGW-Target-Host '{host}' does not match any configured endpoint"),
        ),
    }
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod tests;
