//! Header cleaning and transformation for the data plane.
//!
//! Implements the DESIGN "Headers Transformation" rules: routing headers are
//! consumed (never forwarded), hop-by-hop headers are stripped, then the
//! upstream `headers` rules (set/add/remove/passthrough) are applied to the
//! outbound request and the upstream response.

use http::HeaderMap;
use http::header::{CONNECTION, CONTENT_TYPE, HOST, TE, TRAILER, TRANSFER_ENCODING, UPGRADE};

use crate::domain::models::{HeaderTransforms, PassthroughMode, RequestHeaderRules, ResponseHeaderRules};

/// OAGW routing headers consumed by the data plane (never forwarded).
pub const ROUTING_HEADERS: [&str; 4] = [
    // "x-oagw-target-host" is read by the proxy handler and stripped here.
    "x-oagw-target-host",
    "x-oagw-error-source",
    "x-forwarded-for",
    "x-forwarded-proto",
];

/// Strip hop-by-hop and OAGW-routing headers from an outbound header map.
///
/// `target_authority` becomes the new `Host` value (matching the forwarded
/// endpoint); `content_length`/`content_type` are left intact for the caller
/// to manage when the body is re-buffered.
pub fn clean_outbound(headers: &mut HeaderMap, target_authority: &str) {
    let hop_by_hop = [
        CONNECTION.as_str(),
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        TE.as_str(),
        TRAILER.as_str(),
        TRANSFER_ENCODING.as_str(),
        UPGRADE.as_str(),
    ];
    for name in hop_by_hop {
        headers.remove(name);
    }
    for name in ROUTING_HEADERS {
        headers.remove(name);
    }
    // Replace Host with the target endpoint authority.
    if let Ok(value) = http::HeaderValue::from_str(target_authority) {
        headers.insert(HOST, value);
    }
}

/// Apply request header rules from `upstream.headers` on top of the cleaned
/// outbound map.
pub fn apply_request_rules(headers: &mut HeaderMap, rules: &RequestHeaderRules) {
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for name in &rules.remove {
        if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// Apply response header rules from `upstream.headers` to an upstream
/// response.
pub fn apply_response_rules(headers: &mut HeaderMap, rules: &ResponseHeaderRules) {
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    for name in &rules.remove {
        if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

/// Decide which inbound headers survive to the outbound request.
///
/// Returns the outbound map to hand to the transport: restricted per
/// `passthrough` mode (none → nothing, allowlist → listed, all → everything
/// except hop-by-hop/routing headers), the `Host` replaced with the target
/// authority, then the `set`/`add`/`remove` rules applied on top.
pub fn plan_request_headers(
    inbound: &HeaderMap,
    config: &HeaderTransforms,
    target_authority: &str,
) -> HeaderMap {
    let mut outbound = match config.request.passthrough {
        PassthroughMode::None => HeaderMap::new(),
        PassthroughMode::Allowlist => {
            let mut next = HeaderMap::new();
            for (name, value) in inbound {
                let n = name.as_str().to_ascii_lowercase();
                if config
                    .request
                    .passthrough_allowlist
                    .iter()
                    .any(|a| a == &n)
                {
                    next.insert(name.clone(), value.clone());
                }
            }
            next
        }
        PassthroughMode::All => inbound.clone(),
    };
    clean_outbound(&mut outbound, target_authority);
    apply_request_rules(&mut outbound, &config.request);
    outbound
}

/// Whether any inbound body-disposition header must be preserved: a
/// `Content-Type` that the caller explicitly set via rules, or a
/// `Content-Length` matching the re-buffered body (recomputed by the caller).
#[must_use]
pub fn has_content_type(outbound: &HeaderMap) -> bool {
    outbound.contains_key(CONTENT_TYPE)
}
