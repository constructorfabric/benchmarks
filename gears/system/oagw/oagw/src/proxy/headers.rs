//! Header transformation rules applied on the proxy path.

use http::HeaderMap;

use crate::domain::model::{HeaderRules, Passthrough, RequestHeaderRules, ResponseHeaderRules};

/// Headers stripped from every proxied request (RFC 9110 §7.6.1).
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

/// The gateway's own routing header, read then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Header distinguishing gateway from upstream errors.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Reads the `X-OAGW-Target-Host` header value, if present.
#[must_use]
pub fn target_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// Builds the outbound request headers.
///
/// The inbound set is reduced by the hop-by-hop headers and the gateway's own
/// routing headers, then filtered through the upstream's passthrough policy,
/// then the `set`/`add`/`remove` rules are applied.
#[must_use]
pub fn build_outbound_headers(
    inbound: &HeaderMap,
    rules: &RequestHeaderRules,
    host: &str,
) -> HeaderMap {
    let mut out = HeaderMap::new();

    let allowed = |name: &http::HeaderName| -> bool {
        if HOP_BY_HOP_HEADERS.contains(&name.as_str()) {
            return false;
        }
        if name == TARGET_HOST_HEADER || name == "host" || name == "content-length" {
            return false;
        }
        match rules.passthrough {
            Passthrough::None => false,
            Passthrough::All => true,
            Passthrough::Allowlist => rules
                .passthrough_allowlist
                .iter()
                .any(|h| h.eq_ignore_ascii_case(name.as_str())),
        }
    };

    for (name, value) in inbound {
        if allowed(name) {
            out.append(name, value.clone());
        }
    }

    for name in &rules.remove {
        out.remove(name.as_str());
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }

    if let Ok(value) = http::HeaderValue::from_str(host) {
        out.insert(http::header::HOST, value);
    }
    out
}

/// Applies the response header rules to an upstream response.
#[must_use]
pub fn apply_response_headers(headers: HeaderMap, rules: &ResponseHeaderRules) -> HeaderMap {
    let mut out = headers;
    for name in &rules.remove {
        out.remove(name.as_str());
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.append(name, value);
        }
    }
    out
}

/// Strips the hop-by-hop headers from a header set in place.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP_HEADERS {
        headers.remove(*name);
    }
}

/// Whether a header set carries the rules configured on the upstream.
#[must_use]
pub fn rules_are_empty(rules: &HeaderRules) -> bool {
    rules.request.is_empty() && rules.response.is_empty()
}
