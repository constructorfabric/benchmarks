//! Request extractors for the proxy API.

use crate::domain::error::DomainError;

/// Marker extension carrying the proxy alias.
#[derive(Debug, Clone)]
pub struct ProxyAlias(pub String);

/// Marker extension carrying the proxy path suffix.
#[derive(Debug, Clone)]
pub struct ProxySuffix(pub String);

/// Parses a query string into pairs, keeping duplicates and order.
pub fn parse_query(raw: &str) -> Vec<(String, String)> {
    form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Splits a forwarded path into the route-matched part and the suffix.
///
/// `/proxy/{alias}/{a}/{b}` with route path `/a` yields suffix `b`. The rule
/// itself lives in the domain layer, which owns route semantics.
pub fn split_suffix(route_path: &str, forwarded_path: &str) -> String {
    crate::domain::matching::path_suffix(route_path, forwarded_path)
}

/// The `X-OAGW-Target-Host` header name.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Reads and consumes the target-host hint from a header map.
pub fn take_target_host(headers: &mut axum::http::HeaderMap) -> Option<String> {
    headers
        .remove(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok().map(|s| s.to_string()))
}

/// Validates a target host value: hostname or IP literal, no port, path or
/// special characters.
pub fn validate_target_host(value: &str) -> Result<String, DomainError> {
    let value = value.trim().trim_end_matches('.');
    if value.is_empty() {
        return Err(DomainError::InvalidTargetHost { invalid_value: value.to_string() });
    }
    if value.contains('/') || value.contains('\\') || value.contains('?') || value.contains('#') {
        return Err(DomainError::InvalidTargetHost { invalid_value: value.to_string() });
    }
    if value.contains(':') {
        // A port or an IPv6 literal in `[..]` form; bare IPv6 is accepted.
        if value.starts_with('[') {
            return Err(DomainError::InvalidTargetHost { invalid_value: value.to_string() });
        }
        if value.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(DomainError::InvalidTargetHost { invalid_value: value.to_string() });
        }
    }
    if crate::domain::alias::is_ip_literal(value) {
        return Ok(value.to_ascii_lowercase());
    }
    if !crate::domain::alias::is_valid_hostname(value) {
        return Err(DomainError::InvalidTargetHost { invalid_value: value.to_string() });
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_string_is_parsed_with_duplicates_preserved() {
        let q = parse_query("a=1&b=2&a=3");
        assert_eq!(q, vec![("a".into(), "1".into()), ("b".into(), "2".into()), ("a".into(), "3".into())]);
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn the_suffix_is_what_follows_the_route_path() {
        assert_eq!(split_suffix("/v1", "/v1/chat/completions"), "chat/completions");
        assert_eq!(split_suffix("/v1", "/v1"), "");
        assert_eq!(split_suffix("/v1/", "/v1"), "");
    }

    #[test]
    fn target_host_validation_accepts_hostnames_and_ips() {
        assert!(validate_target_host("api.openai.com").is_ok());
        assert!(validate_target_host("api.openai.com.").is_ok());
        assert!(validate_target_host("10.0.1.1").is_ok());
        assert!(validate_target_host("US.Vendor.COM").is_ok());
    }

    #[test]
    fn target_host_rejects_ports_paths_and_garbage() {
        assert!(validate_target_host("api.openai.com:8443").is_err());
        assert!(validate_target_host("api.openai.com/v1").is_err());
        assert!(validate_target_host("not a host").is_err());
        assert!(validate_target_host("").is_err());
    }

    #[test]
    fn the_target_host_header_is_consumed_not_read() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-oagw-target-host", "api.openai.com".parse().unwrap());
        assert_eq!(take_target_host(&mut headers).as_deref(), Some("api.openai.com"));
        assert!(take_target_host(&mut headers).is_none(), "the header is consumed");
        assert!(!headers.contains_key("x-oagw-target-host"));
    }
}
