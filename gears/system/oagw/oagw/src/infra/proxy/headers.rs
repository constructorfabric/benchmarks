//! Header transformation (DESIGN §3.2 "Headers Transformation").

use http::HeaderMap;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;
use crate::domain::model::{HeadersConfig, PassthroughMode};

/// Headers consumed by the gateway during routing; never forwarded.
const ROUTING_HEADERS: &[&str] = &[gts::HEADER_TARGET_HOST, gts::HEADER_ERROR_SOURCE];

/// Hop-by-hop headers stripped per HTTP semantics.
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

/// Headers the proxy always forwards regardless of `passthrough`, because the
/// request body cannot be framed or negotiated without them.
const STRUCTURAL_HEADERS: &[&str] = &["content-type", "content-length", "accept"];

/// The `X-OAGW-Target-Host` routing header, already validated.
///
/// # Errors
/// Returns [`crate::domain::error::DomainError::InvalidTargetHost`] when the
/// header is present but not a bare hostname or IP.
pub fn requested_target_host(headers: &HeaderMap) -> Result<Option<String>, DomainError> {
    match headers.get(crate::domain::gts_helpers::HEADER_TARGET_HOST) {
        Some(value) => {
            let raw = value.to_str().map_err(|_| {
                crate::domain::error::DomainError::InvalidTargetHost(
                    "the header value is not valid ASCII".to_owned(),
                )
            })?;
            parse_target_host(raw).map(Some)
        }
        None => Ok(None),
    }
}

/// Appends `headers` to `out`, ignoring values that are not valid on the wire.
pub fn insert_all(out: &mut HeaderMap, headers: &[(&'static str, String)]) {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(name, value);
        }
    }
}

/// Removes hop-by-hop and gateway routing headers from an inbound request.
#[must_use]
pub fn strip_gateway_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP_HEADERS.contains(&key.as_str()) || ROUTING_HEADERS.contains(&key.as_str()) {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Builds the outbound request headers from the inbound set plus the upstream's
/// `headers.request` rules.
#[must_use]
pub fn build_outbound_headers(
    inbound_stripped: &HeaderMap,
    rules: &crate::domain::model::RequestHeaderRules,
) -> HeaderMap {
    let mut out = match rules.passthrough {
        PassthroughMode::All => inbound_stripped.clone(),
        PassthroughMode::Allowlist => {
            let allow: Vec<String> = rules
                .passthrough_allowlist
                .iter()
                .map(|h| h.trim().to_ascii_lowercase())
                .collect();
            let allow: Vec<&str> = allow.iter().map(String::as_str).collect();
            filter_headers(inbound_stripped, &allow)
        }
        PassthroughMode::None => filter_headers(inbound_stripped, STRUCTURAL_HEADERS),
    };

    for name in &rules.remove {
        out.remove(name);
    }
    for (name, value) in &rules.set {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.insert(n, v);
        }
    }
    for (name, value) in &rules.add {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            out.append(n, v);
        }
    }
    out
}

fn filter_headers(source: &HeaderMap, allow: &[&str]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in source {
        let key = name.as_str().to_ascii_lowercase();
        if allow.contains(&key.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Applies the upstream's `headers.response` rules to the upstream response
/// headers in place.
pub fn apply_response_headers(headers: &mut HeaderMap, rules: &HeadersConfig) {
    for name in &rules.response.remove {
        headers.remove(name);
    }
    for (name, value) in &rules.response.set {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.insert(n, v);
        }
    }
    for (name, value) in &rules.response.add {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            http::HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
}

/// Parses an `X-OAGW-Target-Host` value: a bare hostname or IP, no port or path.
///
/// # Errors
/// Returns [`crate::domain::error::DomainError::InvalidTargetHost`] for an empty
/// value or one carrying a scheme, port, path or user info.
pub fn parse_target_host(value: &str) -> Result<String, crate::domain::error::DomainError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(crate::domain::error::DomainError::InvalidTargetHost(
            "the header value is empty".to_owned(),
        ));
    }
    let has_scheme = trimmed.contains("://");
    let has_port = trimmed
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let has_path = trimmed.contains('/') || trimmed.contains('?');
    if has_scheme || has_port || has_path || trimmed.contains('@') {
        return Err(crate::domain::error::DomainError::InvalidTargetHost(
            format!("`{trimmed}` must be a bare hostname or IP address"),
        ));
    }
    Ok(crate::domain::alias::normalize_host(trimmed))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod headers_tests;
