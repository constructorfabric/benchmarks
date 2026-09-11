//! Request and response header transformation.
//!
//! Hop-by-hop stripping happens first, then the configured rules. The
//! [`HopHeader`] list is fixed by DESIGN.md and applied in both directions.

use crate::domain::error::DomainError;
use crate::domain::model::{
    HeadersConfig, PassthroughMode, RequestHeaderRules, ResponseHeaderRules,
};
use http::HeaderMap;

/// Headers removed from a proxied request and from the upstream response.
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

/// A hop-by-hop header name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopHeader {
    /// `Connection`.
    Connection,
    /// `Keep-Alive`.
    KeepAlive,
    /// `Proxy-Authenticate`.
    ProxyAuthenticate,
    /// `Proxy-Authorization`.
    ProxyAuthorization,
    /// `TE`.
    Te,
    /// `Trailer`.
    Trailer,
    /// `Transfer-Encoding`.
    TransferEncoding,
    /// `Upgrade`.
    Upgrade,
}

impl HopHeader {
    /// The wire name of this header.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::KeepAlive => "keep-alive",
            Self::ProxyAuthenticate => "proxy-authenticate",
            Self::ProxyAuthorization => "proxy-authorization",
            Self::Te => "te",
            Self::Trailer => "trailer",
            Self::TransferEncoding => "transfer-encoding",
            Self::Upgrade => "upgrade",
        }
    }
}

/// Whether a header name is one of the hop-by-hop set.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.contains(&name.to_ascii_lowercase().as_str())
}

/// Removes hop-by-hop headers and `host` from a header map.
pub fn strip_hop_by_hop(headers: &mut HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
    headers.remove(http::header::HOST);
}

/// Builds the outbound request headers for a proxied call.
///
/// The result carries the surviving passthrough headers, then the configured
/// `set`/`add`/`remove` rules, then the gateway's own headers (host, request id
/// and target host are the caller's business).
///
/// # Errors
///
/// Returns [`DomainError::Invalid`] when a configured name or value cannot be
/// represented on the wire.
pub fn build_request_headers(
    inbound: &HeaderMap,
    config: &HeadersConfig,
    rules: &RequestHeaderRules,
) -> Result<HeaderMap, DomainError> {
    let mut outbound = HeaderMap::new();
    match rules.passthrough {
        PassthroughMode::None => {}
        PassthroughMode::All => {
            for (name, value) in inbound {
                if !is_hop_by_hop(name.as_str()) && !is_reserved(name.as_str()) {
                    append(&mut outbound, name.as_str(), value)?;
                }
            }
        }
        PassthroughMode::Allowlist => {
            for (name, value) in inbound {
                let allowed = rules
                    .passthrough_allowlist
                    .iter()
                    .any(|entry| entry.eq_ignore_ascii_case(name.as_str()));
                if allowed && !is_reserved(name.as_str()) {
                    append(&mut outbound, name.as_str(), value)?;
                }
            }
        }
    }
    for name in &rules.remove {
        outbound.remove(name);
    }
    apply_rules(&mut outbound, &rules.set, &rules.add)?;
    let _ = config;
    Ok(outbound)
}

/// Applies the response-side rules to an upstream response's headers.
///
/// # Errors
///
/// Returns [`DomainError::Invalid`] when a configured name or value cannot be
/// represented on the wire.
pub fn build_response_headers(
    upstream: &HeaderMap,
    rules: &ResponseHeaderRules,
) -> Result<HeaderMap, DomainError> {
    let mut outbound = upstream.clone();
    for name in HOP_BY_HOP {
        outbound.remove(name);
    }
    outbound.remove(http::header::CONTENT_LENGTH);
    for name in &rules.remove {
        outbound.remove(name);
    }
    apply_rules(&mut outbound, &rules.set, &rules.add)?;
    Ok(outbound)
}

/// Headers the gateway owns and never copies from the caller.
fn is_reserved(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "content-length" | "x-oagw-target-host" | "x-oagw-error-source" | "x-request-id"
    )
}

fn apply_rules(
    headers: &mut HeaderMap,
    set: &std::collections::BTreeMap<String, String>,
    add: &std::collections::BTreeMap<String, String>,
) -> Result<(), DomainError> {
    for (name, value) in set {
        let parsed = header_name(name)?;
        headers.remove(parsed.clone());
        insert(headers, &parsed, value)?;
    }
    for (name, value) in add {
        let parsed = header_name(name)?;
        insert(headers, &parsed, value)?;
    }
    Ok(())
}

fn header_name(name: &str) -> Result<http::HeaderName, DomainError> {
    http::HeaderName::try_from(name)
        .map_err(|_| DomainError::Invalid(format!("'{name}' is not a valid header name")))
}

fn insert(
    headers: &mut HeaderMap,
    name: &http::HeaderName,
    value: &str,
) -> Result<(), DomainError> {
    let parsed = http::HeaderValue::try_from(value).map_err(|_| {
        DomainError::Invalid(format!(
            "value for header '{name}' is not a valid header value"
        ))
    })?;
    headers.append(name.clone(), parsed);
    Ok(())
}

fn append(
    headers: &mut HeaderMap,
    name: &str,
    value: &http::HeaderValue,
) -> Result<(), DomainError> {
    let parsed = header_name(name)?;
    headers.append(parsed, value.clone());
    Ok(())
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod tests;
