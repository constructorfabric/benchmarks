// Created: 2026-09-04 by Constructor Tech
//! Header transformation of the data plane.
//!
//! Implements the three header categories of `docs/DESIGN.md` §3.2
//! "Headers Transformation":
//!
//! 1. *routing headers* — consumed by OAGW and never forwarded
//!    ([`TARGET_HOST_HEADER`], `host`);
//! 2. *hop-by-hop headers* — stripped in both directions
//!    ([`strip_hop_by_hop`]);
//! 3. *passthrough headers* — forwarded according to the per-upstream
//!    `headers` rules ([`outbound_request_headers`],
//!    [`outbound_response_headers`]).

use std::borrow::Cow;

use form_urlencoded;
use http::{HeaderMap, HeaderName, HeaderValue};

use crate::domain::{HeaderPassthrough, RequestHeaderRules, ResponseHeaderRules};
use crate::error::OagwError;

/// `X-OAGW-Target-Host` — the routing header that pins the upstream endpoint
/// (`docs/DESIGN.md` §3.2, `docs/ADR/0001-request-routing.md`).
pub const TARGET_HOST_HEADER: HeaderName = HeaderName::from_static("x-oagw-target-host");

/// `X-Request-ID` — propagated by the built-in `request_id` transform plugin.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Hop-by-hop headers stripped in both directions (RFC 9110 §7.6.1).
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Header names the gateway consumes for routing and never forwards
/// (`docs/DESIGN.md` §3.2 "Routing Headers"). The second entry is
/// [`TARGET_HOST_HEADER`]; a test pins the two together.
const ROUTING_HEADERS: [&str; 2] = ["host", "x-oagw-target-host"];

/// Header names the gateway re-derives from the proxied exchange instead of
/// copying them: the body length is recomputed from the forwarded body, and
/// `content-type` travels through the passthrough filter.
const FRAMING_HEADERS: [&str; 1] = ["content-length"];

/// `true` when `name` is one of the hop-by-hop headers (case-insensitive).
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    HOP_BY_HOP.iter().any(|header| *header == lowered)
}

/// Header names listed in the `Connection` header, lowercased.
///
/// RFC 9110 §7.6.1: every header named in `Connection` is hop-by-hop and must
/// not be forwarded.
#[must_use]
pub fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    let mut tokens = Vec::new();
    for value in headers.get_all(http::header::CONNECTION) {
        for token in value.to_str().unwrap_or_default().split(',') {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                tokens.push(trimmed.to_ascii_lowercase());
            }
        }
    }
    tokens
}

/// Removes every hop-by-hop header — the fixed list plus anything named in
/// the `Connection` header of `headers`.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let mut doomed: Vec<HeaderName> = Vec::new();
    for token in connection_tokens(headers) {
        if let Ok(name) = HeaderName::try_from(token.as_str()) {
            doomed.push(name);
        }
    }
    for name in HOP_BY_HOP {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            doomed.push(parsed);
        }
    }
    for name in doomed {
        headers.remove(&name);
    }
}

/// Removes the routing headers OAGW consumed (`docs/DESIGN.md` §3.2) and the
/// framing headers the gateway re-derives from the proxied body.
pub fn strip_routing_headers(headers: &mut HeaderMap) {
    for name in ROUTING_HEADERS.iter().chain(FRAMING_HEADERS.iter()) {
        if let Ok(parsed) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(parsed);
        }
    }
}

/// Value of `X-OAGW-Target-Host`, trimmed, when the caller supplied one.
#[must_use]
pub fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// `true` when `raw` is a valid `X-OAGW-Target-Host` value: a bare hostname
/// or IP address, with no port, path or separator
/// (`docs/DESIGN.md` §3.3 "InvalidTargetHost").
#[must_use]
pub fn is_valid_target_host(raw: &str) -> bool {
    if raw.is_empty()
        || raw.len() > 253
        || raw.starts_with('-')
        || raw.ends_with('-')
        || raw.contains(['/', '\\', '?', '#', '@', ':', ' ', '\t'])
    {
        return false;
    }
    raw.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
            })
    })
}

/// Copies the inbound headers the upstream `headers` rules forward.
///
/// `None` rules mean [`HeaderPassthrough::None`]: no inbound header reaches
/// the upstream (`docs/schemas/upstream.v1.schema.json` `headers.request`).
#[must_use]
fn forward_passthrough(inbound: &HeaderMap, rules: Option<&RequestHeaderRules>) -> HeaderMap {
    let (passthrough, allowlist) = match rules {
        None => (HeaderPassthrough::None, Vec::new()),
        Some(rules) => (
            rules.passthrough,
            rules
                .passthrough_allowlist
                .iter()
                .map(|name| name.to_ascii_lowercase())
                .collect::<Vec<_>>(),
        ),
    };
    if passthrough == HeaderPassthrough::None {
        return HeaderMap::new();
    }
    let mut forwarded = HeaderMap::with_capacity(inbound.len());
    for (name, value) in inbound {
        if ROUTING_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if passthrough == HeaderPassthrough::Allowlist
            && !allowlist.iter().any(|allowed| allowed == name.as_str())
        {
            continue;
        }
        forwarded.append(name.clone(), value.clone());
    }
    forwarded
}

/// Builds the request headers sent to the upstream.
///
/// The pipeline is: passthrough filter → hop-by-hop strip → routing and
/// framing headers drop → `headers.request` rules → `host` set to
/// `authority` (`docs/DESIGN.md` §3.2 "Transformation Rules").
#[must_use]
pub fn outbound_request_headers(
    inbound: &HeaderMap,
    rules: Option<&RequestHeaderRules>,
    authority: &str,
) -> HeaderMap {
    let mut outbound = forward_passthrough(inbound, rules);
    strip_hop_by_hop(&mut outbound);
    strip_routing_headers(&mut outbound);
    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(parsed) = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()) {
                outbound.remove(parsed);
            }
        }
        set_and_add(&mut outbound, &rules.set, &rules.add);
    }
    if let Ok(host) = HeaderValue::from_str(authority) {
        outbound.insert(http::header::HOST, host);
    }
    outbound
}

/// Builds the response headers returned to the caller from the upstream
/// response headers.
#[must_use]
pub fn outbound_response_headers(
    upstream: &HeaderMap,
    rules: Option<&ResponseHeaderRules>,
) -> HeaderMap {
    let mut outbound = upstream.clone();
    strip_hop_by_hop(&mut outbound);
    outbound.remove(http::header::CONTENT_LENGTH);
    if let Some(rules) = rules {
        for name in &rules.remove {
            if let Ok(parsed) = HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()) {
                outbound.remove(parsed);
            }
        }
        set_and_add(&mut outbound, &rules.set, &rules.add);
    }
    outbound
}

/// Applies the `set` (overwrite) and `add` (append) header rules.
fn set_and_add(
    headers: &mut HeaderMap,
    set: &std::collections::BTreeMap<String, String>,
    add: &std::collections::BTreeMap<String, String>,
) {
    for (name, value) in set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
}

/// Filters the inbound query string against the matched route's
/// `query_allowlist`.
///
/// Unknown parameters are rejected with [`OagwError::Validation`]
/// (`docs/DESIGN.md` §3.2 "Guard Rules": query params validated against
/// `query_allowlist`); an empty allowlist allows no parameter at all.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when a query parameter is not
/// allowlisted.
pub fn filter_query(raw: Option<&str>, allowlist: &[String]) -> Result<Option<String>, OagwError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim_start_matches('?');
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    for (name, value) in form_urlencoded::parse(raw.as_bytes()) {
        let name = Cow::into_owned(name);
        if allowlist.contains(&name) {
            serializer.append_pair(&name, &value);
        } else {
            return Err(OagwError::Validation {
                detail: format!("query parameter '{name}' is not in the route allowlist"),
            });
        }
    }
    let filtered = serializer.finish();
    Ok((!filtered.is_empty()).then_some(filtered))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "headers_tests.rs"]
mod headers_tests;
