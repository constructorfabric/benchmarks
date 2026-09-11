//! Validation of the domain model against the wire contract.
//!
//! Everything a management request can get wrong is checked here: endpoint
//! homogeneity, host validity, scheme acceptance, route match rules, tag
//! patterns and the body-level rules the proxy enforces. The scheme set itself
//! is exhaustive on [`crate::domain::model::EndpointScheme`], so it needs no
//! validator.

use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, HeadersConfig, HttpMatch, HttpMethod, MatchConfig, Protocol, Route, ServerConfig,
    Upstream,
};
use crate::domain::{alias, merge};
use http::HeaderMap;

/// Maximum length of an RFC 1123 hostname.
pub const MAX_HOSTNAME_LENGTH: usize = 253;
/// Maximum length of a single hostname label.
pub const MAX_LABEL_LENGTH: usize = 63;
/// Hard body limit: 100 MB.
pub const BODY_HARD_LIMIT_BYTES: u64 = 100_000_000;

/// Validates a hostname per RFC 1123, or accepts an IPv4/IPv6 literal.
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.len() > MAX_HOSTNAME_LENGTH {
        return false;
    }
    if trimmed.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    trimmed.split('.').all(|label| {
        if label.is_empty() || label.len() > MAX_LABEL_LENGTH {
            return false;
        }
        if label.starts_with('-') || label.ends_with('-') {
            return false;
        }
        label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

/// Validates one endpoint.
///
/// # Errors
///
/// Returns a message when the host or the scheme is invalid.
pub fn validate_endpoint(endpoint: &Endpoint) -> Result<(), DomainError> {
    if !is_valid_host(&endpoint.host) {
        return Err(DomainError::Invalid(format!(
            "endpoint host '{}' is not a valid RFC 1123 hostname or IP literal",
            endpoint.host
        )));
    }
    Ok(())
}

/// Validates the `server` section of an upstream.
///
/// # Errors
///
/// Returns a message when the pool is empty, an endpoint is invalid, or the
/// pool is not homogeneous in scheme and port.
pub fn validate_server(server: &ServerConfig) -> Result<(), DomainError> {
    if server.endpoints.is_empty() {
        return Err(DomainError::Invalid(
            "at least one endpoint is required".to_owned(),
        ));
    }
    for endpoint in &server.endpoints {
        validate_endpoint(endpoint)?;
    }
    let (scheme, port) = (server.endpoints[0].scheme, server.endpoints[0].port);
    let homogeneous = server
        .endpoints
        .iter()
        .all(|endpoint| endpoint.scheme == scheme && endpoint.port == port);
    if !homogeneous {
        return Err(DomainError::Invalid(
            "all endpoints must share the same scheme and port".to_owned(),
        ));
    }
    Ok(())
}

/// Validates the header rules of an upstream.
///
/// # Errors
///
/// Returns a message when a header name or value cannot be represented on the
/// wire.
pub fn validate_headers(headers: &HeadersConfig) -> Result<(), DomainError> {
    for (name, value) in headers.request.set.iter().chain(headers.request.add.iter()) {
        validate_header_pair(name, value)?;
    }
    for (name, value) in headers
        .response
        .set
        .iter()
        .chain(headers.response.add.iter())
    {
        validate_header_pair(name, value)?;
    }
    Ok(())
}

fn validate_header_pair(name: &str, value: &str) -> Result<(), DomainError> {
    if http::HeaderName::try_from(name).is_err() {
        return Err(DomainError::Invalid(format!(
            "'{name}' is not a valid header name"
        )));
    }
    if http::HeaderValue::try_from(value).is_err() {
        return Err(DomainError::Invalid(format!(
            "value for header '{name}' is not a valid header value"
        )));
    }
    Ok(())
}

/// Validates a tag against `^[a-z0-9_-]+$`.
#[must_use]
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        if !is_valid_tag(tag) {
            return Err(DomainError::Invalid(format!(
                "tag '{tag}' must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

/// Validates an upstream body.
///
/// # Errors
///
/// Returns a message naming the first contract violation.
pub fn validate_upstream(upstream: &Upstream) -> Result<(), DomainError> {
    validate_server(&upstream.server)?;
    validate_headers(&upstream.headers)?;
    validate_tags(&upstream.tags)?;
    if let Some(cors) = &upstream.cors {
        merge::validate_cors(cors)?;
    }
    if let Some(rate_limit) = &upstream.rate_limit {
        merge::validate_rate_limit(rate_limit)?;
    }
    if upstream
        .auth
        .as_ref()
        .is_some_and(|auth| auth.plugin_type.is_empty())
    {
        return Err(DomainError::Invalid(
            "auth.type must name an auth plugin".to_owned(),
        ));
    }
    if !alias::is_valid(&upstream.alias) {
        return Err(DomainError::Invalid(format!(
            "alias '{}' is not a valid alias",
            upstream.alias
        )));
    }
    Ok(())
}

/// Validates the match section of a route.
///
/// # Errors
///
/// Returns a message when the match is absent, ambiguous, or violates the
/// HTTP or gRPC shape.
pub fn validate_match(match_config: &MatchConfig) -> Result<(), DomainError> {
    match (&match_config.http, &match_config.grpc) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(_grpc)) => Ok(()),
        (Some(_), Some(_)) => Err(DomainError::Invalid(
            "exactly one of match.http or match.grpc must be set".to_owned(),
        )),
        (None, None) => Err(DomainError::Invalid(
            "one of match.http or match.grpc is required".to_owned(),
        )),
    }
}

fn validate_http_match(match_config: &HttpMatch) -> Result<(), DomainError> {
    if match_config.methods.is_empty() {
        return Err(DomainError::Invalid(
            "at least one HTTP method is required".to_owned(),
        ));
    }
    if match_config.path.is_empty() {
        return Err(DomainError::Invalid("path must not be empty".to_owned()));
    }
    Ok(())
}

/// Validates a route body.
///
/// # Errors
///
/// Returns a message naming the first contract violation.
pub fn validate_route(route: &Route) -> Result<(), DomainError> {
    if route.upstream_id.is_empty() {
        return Err(DomainError::Invalid(
            "upstream_id must name an upstream of the calling tenant".to_owned(),
        ));
    }
    validate_match(&route.match_config)?;
    validate_tags(&route.tags)?;
    if let Some(cors) = &route.cors {
        merge::validate_cors(cors)?;
    }
    if let Some(rate_limit) = &route.rate_limit {
        merge::validate_rate_limit(rate_limit)?;
    }
    Ok(())
}

/// Validates a body against the declared `Content-Length` and the hard limit.
///
/// # Errors
///
/// Returns [`crate::domain::error::OagwError`] via [`BodyViolation`] when the
/// declared length disagrees with the actual size, the transfer encoding is
/// unsupported, or the body exceeds the 100 MB limit.
pub fn validate_body(headers: &HeaderMap, actual_len: usize) -> Result<(), BodyViolation> {
    if let Some(encoding) = headers.get(http::header::TRANSFER_ENCODING) {
        let value = encoding.to_str().unwrap_or_default().to_ascii_lowercase();
        if !value.contains("chunked") {
            return Err(BodyViolation::UnsupportedEncoding(value));
        }
    }
    if let Some(length) = headers.get(http::header::CONTENT_LENGTH) {
        let text = length.to_str().unwrap_or_default().trim();
        let declared: u64 = text
            .parse()
            .map_err(|_| BodyViolation::InvalidLength(text.to_owned()))?;
        if u64::try_from(actual_len).unwrap_or(u64::MAX) != declared {
            return Err(BodyViolation::LengthMismatch {
                declared,
                actual: actual_len,
            });
        }
    }
    if u64::try_from(actual_len).unwrap_or(u64::MAX) > BODY_HARD_LIMIT_BYTES {
        return Err(BodyViolation::TooLarge);
    }
    Ok(())
}

/// The ways a request body can violate its contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BodyViolation {
    /// `Content-Length` is present but not an integer.
    InvalidLength(String),
    /// `Content-Length` disagrees with the actual size.
    LengthMismatch { declared: u64, actual: usize },
    /// A transfer encoding other than `chunked` was requested.
    UnsupportedEncoding(String),
    /// The body exceeds the 100 MB hard limit.
    TooLarge,
}

/// Whether the request method is one a route may accept.
#[must_use]
pub fn is_routable_method(method: &str) -> bool {
    HttpMethod::parse(method).is_some()
}

/// Whether the upstream protocol is proxied in this delivery.
#[must_use]
pub fn is_proxied_protocol(protocol: Protocol) -> bool {
    matches!(protocol, Protocol::Http)
}

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
