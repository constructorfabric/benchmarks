//! Outbound header rewriting.
//!
//! Three categories, per the design: routing headers the gateway consumed and drops,
//! hop-by-hop headers the HTTP specifications say not to forward, and everything else,
//! which passes through unless the upstream's `headers` configuration says otherwise.

use http::HeaderMap;

use crate::domain::upstream::{HeadersConfig, PassthroughMode, ResponseHeaders};

/// Headers consumed by the gateway for routing and never forwarded.
pub const ROUTING_HEADERS: [&str; 1] = ["x-oagw-target-host"];

/// Headers a single HTTP hop owns and must not relay.
pub const HOP_BY_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers the gateway always sets itself from the resolved target.
pub const MANAGED_HEADERS: [&str; 1] = ["host"];

/// Builds the outbound header set for a relayed request.
///
/// The caller's headers are filtered by the passthrough mode, the upstream's static rules
/// are applied on top, credential injections land after them so a plugin can overwrite a
/// static rule, and `host` is set last so nothing can override it.
#[must_use]
pub fn build_request_headers(
    inbound: &HeaderMap,
    config: &HeadersConfig,
    target_host: &str,
    injected: &[(String, String)],
) -> HeaderMap {
    let mut out = HeaderMap::new();
    let allowlist = allowlist_of(config);

    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if is_dropped(&lower) {
            continue;
        }
        if let Some(allow) = &allowlist {
            if !allow.iter().any(|n| n.eq_ignore_ascii_case(&lower)) {
                continue;
            }
        }
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(lower.as_bytes()),
            http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(name, value);
        }
    }

    for op in &config.request.add {
        // `add` only fills a header the caller left empty.
        if out.get(&op.name).is_none() {
            set(&mut out, &op.name, &op.value);
        }
    }
    for name in &config.request.remove {
        remove(&mut out, name);
    }
    for op in &config.request.set {
        set(&mut out, &op.name, &op.value);
    }

    for (name, value) in injected {
        set(&mut out, name, value);
    }

    set(&mut out, "host", target_host);
    out
}

/// Builds the outbound response headers, dropping hop-by-hop headers the upstream set.
#[must_use]
pub fn build_response_headers(
    inbound: &HeaderMap,
    config: &ResponseHeaders,
    injected: &[(String, String)],
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in inbound {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(lower.as_bytes()),
            http::HeaderValue::from_bytes(value.as_bytes()),
        ) {
            out.append(name, value);
        }
    }
    for op in &config.add {
        if out.get(&op.name).is_none() {
            set(&mut out, &op.name, &op.value);
        }
    }
    for name in &config.remove {
        remove(&mut out, name);
    }
    for op in &config.set {
        set(&mut out, &op.name, &op.value);
    }
    for (name, value) in injected {
        set(&mut out, name, value);
    }
    out
}

/// Whether an inbound header is dropped before forwarding.
#[must_use]
pub fn is_dropped(lower_name: &str) -> bool {
    ROUTING_HEADERS.contains(&lower_name)
        || HOP_BY_HOP_HEADERS.contains(&lower_name)
        || MANAGED_HEADERS.contains(&lower_name)
}

/// The effective caller-header allowlist for an upstream, if it restricts one.
#[must_use]
fn allowlist_of(config: &HeadersConfig) -> Option<Vec<String>> {
    match config.request.passthrough {
        PassthroughMode::None => Some(Vec::new()),
        PassthroughMode::Allowlist => Some(config.request.passthrough_allowlist.clone()),
        PassthroughMode::All => None,
    }
}

/// Sets a header, replacing any existing values; invalid names are ignored.
pub fn set(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        http::HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

/// Removes a header by name, ignoring invalid names.
pub fn remove(headers: &mut HeaderMap, name: &str) {
    if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) {
        headers.remove(name);
    }
}
