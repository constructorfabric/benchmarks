//! Validation rules the control plane applies on every write.

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::{
    CorsConfig, Endpoint, GrpcMatch, HttpMatch, MatchConfig, RateLimitConfig, RouteSpec,
    UpstreamSpec,
};

/// HTTP methods a route may accept (route schema enum).
pub const ROUTE_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Proxyable methods, used by the data plane route registration.
pub const PROXY_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Validates a hostname per RFC 1123.
///
/// Total length ≤ 253, labels of 1-63 characters, alphanumeric plus hyphen,
/// no leading/trailing hyphen; a trailing dot is tolerated and stripped.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] when `host` is not a valid hostname
/// or IP address.
pub fn validate_hostname(host: &str) -> Result<(), DomainError> {
    let trimmed = host.trim_end_matches('.');
    let invalid = |why: &str| {
        DomainError::new(
            ErrorKind::ValidationError,
            format!("invalid endpoint host `{host}`: {why}"),
        )
    };
    if trimmed.is_empty() {
        return Err(invalid("empty"));
    }
    if trimmed.len() > 253 {
        return Err(invalid("longer than 253 characters"));
    }
    if trimmed.parse::<std::net::IpAddr>().is_ok() {
        return Ok(());
    }
    for label in trimmed.split('.') {
        if label.is_empty() {
            return Err(invalid("empty label"));
        }
        if label.len() > 63 {
            return Err(invalid("label longer than 63 characters"));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(invalid("label contains characters outside [a-zA-Z0-9-]"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(invalid("label starts or ends with a hyphen"));
        }
    }
    Ok(())
}

/// Validates the endpoint pool: at least one endpoint, identical scheme and
/// port across all endpoints, valid hosts.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] describing the first violation.
pub fn validate_endpoint_pool(endpoints: &[Endpoint]) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "at least one endpoint is required",
        ));
    }
    let first = &endpoints[0];
    for endpoint in endpoints {
        validate_hostname(&endpoint.host)?;
        if endpoint.scheme != first.scheme {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                "all endpoints of an upstream must use the same scheme",
            ));
        }
        if endpoint.effective_port() != first.effective_port() {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                "all endpoints of an upstream must use the same port",
            ));
        }
    }
    Ok(())
}

/// Validates the flat tag pattern `^[a-z0-9_-]+$`.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for an invalid tag.
pub fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        let valid = !tag.is_empty()
            && tag
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !valid {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                format!("invalid tag `{tag}`: expected [a-z0-9_-]+"),
            ));
        }
    }
    Ok(())
}

/// Validates a CORS configuration.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] when credentials are combined with
/// the `*` wildcard origin.
pub fn validate_cors(config: &CorsConfig) -> Result<(), DomainError> {
    if config.allow_credentials && config.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "allow_credentials cannot be combined with the wildcard origin `*`",
        ));
    }
    for origin in &config.allowed_origins {
        if origin == "*" {
            continue;
        }
        let valid = origin.starts_with("http://")
            || origin.starts_with("https://")
            || origin.starts_with("wss://");
        if !valid || origin.contains(' ') {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                format!("invalid allowed origin `{origin}`: expected an absolute origin"),
            ));
        }
    }
    for method in &config.allowed_methods {
        if crate::domain::validation::PROXY_METHODS
            .iter()
            .all(|m| m != method)
        {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                format!("invalid CORS allowed method `{method}`"),
            ));
        }
    }
    Ok(())
}

/// Validates a rate limit configuration.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for non-positive rates or costs.
pub fn validate_rate_limit(config: &RateLimitConfig) -> Result<(), DomainError> {
    if config.sustained.rate < 1 {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if config.capacity() < 1 {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if config.cost < 1 {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "rate_limit.cost must be at least 1",
        ));
    }
    Ok(())
}

/// Validates the HTTP match rule of a route.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for an empty or unknown method list
/// or an empty path.
pub fn validate_http_match(match_rule: &HttpMatch) -> Result<(), DomainError> {
    if match_rule.methods.is_empty() {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "match.http.methods must not be empty",
        ));
    }
    for method in &match_rule.methods {
        if ROUTE_METHODS.iter().all(|m| m != method) {
            return Err(DomainError::new(
                ErrorKind::ValidationError,
                format!("invalid route method `{method}`"),
            ));
        }
    }
    if match_rule.path.is_empty() {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "match.http.path must not be empty",
        ));
    }
    if !match_rule.path.starts_with('/') {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            format!("match.http.path `{}` must start with `/`", match_rule.path),
        ));
    }
    Ok(())
}

/// Validates the gRPC match rule of a route.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for empty service or method names.
pub fn validate_grpc_match(match_rule: &GrpcMatch) -> Result<(), DomainError> {
    if match_rule.service.is_empty() || match_rule.method.is_empty() {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            "match.grpc.service and match.grpc.method must not be empty",
        ));
    }
    Ok(())
}

/// Validates the match block: exactly one of `http`/`grpc`.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] when both or neither are present.
pub fn validate_match(match_rule: &MatchConfig) -> Result<(), DomainError> {
    match (&match_rule.http, &match_rule.grpc) {
        (Some(http), None) => validate_http_match(http),
        (None, Some(grpc)) => validate_grpc_match(grpc),
        (Some(_), Some(_)) => Err(DomainError::new(
            ErrorKind::ValidationError,
            "exactly one of match.http or match.grpc must be present",
        )),
        (None, None) => Err(DomainError::new(
            ErrorKind::ValidationError,
            "one of match.http or match.grpc is required",
        )),
    }
}

/// Validates an upstream specification.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for the first violated rule. The
/// alias itself is resolved by `crate::domain::alias` before calling this.
pub fn validate_upstream(spec: &UpstreamSpec) -> Result<(), DomainError> {
    validate_endpoint_pool(&spec.server.endpoints)?;
    validate_tags(&spec.tags)?;
    if let Some(cors) = &spec.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &spec.rate_limit {
        validate_rate_limit(rate)?;
    }
    Ok(())
}

/// Validates a route specification.
///
/// # Errors
/// Returns [`ErrorKind::ValidationError`] for the first violated rule.
pub fn validate_route(spec: &RouteSpec) -> Result<(), DomainError> {
    validate_match(&spec.match_rule)?;
    validate_tags(&spec.tags)?;
    if let Some(cors) = &spec.cors {
        validate_cors(cors)?;
    }
    if let Some(rate) = &spec.rate_limit {
        validate_rate_limit(rate)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{EndpointScheme, PathSuffixMode, SustainedRate};

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn accepts_valid_hostnames_and_ips() {
        for host in [
            "api.openai.com",
            "api.openai.com.",
            "a-b.example.com",
            "localhost",
            "10.0.1.1",
            "2001:db8::1",
            "xn--bcher-kva.example",
        ] {
            assert_eq!(
                validate_hostname(host).ok(),
                Some(()),
                "{host} must be valid"
            );
        }
    }

    #[test]
    fn rejects_invalid_hostnames() {
        for host in [
            "",
            "-leading.example.com",
            "trailing-.example.com",
            "a_underscore.example.com",
        ] {
            assert_eq!(validate_hostname(host).ok(), None, "{host} must be invalid");
        }
        assert!(validate_hostname("a".repeat(64).as_str()).is_err());
        assert!(validate_hostname(format!("{}x", "a.".repeat(130)).as_str()).is_err());
    }

    #[test]
    fn validates_the_endpoint_pool() {
        assert!(validate_endpoint_pool(&[]).is_err());
        let ok = vec![
            endpoint("a.example.com", 443),
            endpoint("b.example.com", 443),
        ];
        assert_eq!(validate_endpoint_pool(&ok).ok(), Some(()));
        let mixed_ports = vec![
            endpoint("a.example.com", 443),
            endpoint("b.example.com", 8443),
        ];
        assert!(validate_endpoint_pool(&mixed_ports).is_err());
        let bad_host = vec![endpoint("-bad.example.com", 443)];
        assert!(validate_endpoint_pool(&bad_host).is_err());
    }

    #[test]
    fn validates_tags() {
        assert_eq!(
            validate_tags(&["llm".to_owned(), "a-b_c".to_owned()]).ok(),
            Some(())
        );
        assert!(validate_tags(&["UPPER".to_owned()]).is_err());
        assert!(validate_tags(&["with space".to_owned()]).is_err());
    }

    #[test]
    fn rejects_credentials_with_wildcard_origin() {
        let config = CorsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: true,
        };
        assert!(validate_cors(&config).is_err());
        let wildcard_without_credentials = CorsConfig {
            allow_credentials: false,
            ..config
        };
        assert_eq!(validate_cors(&wildcard_without_credentials).ok(), Some(()));
    }

    #[test]
    fn rejects_invalid_origins_and_methods() {
        let config = CorsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            enabled: true,
            allowed_origins: vec!["not-a-url".to_owned()],
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: Vec::new(),
            allow_credentials: false,
        };
        assert!(validate_cors(&config).is_err());
        let bad_method = CorsConfig {
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["TRACE".to_owned()],
            ..config
        };
        assert!(validate_cors(&bad_method).is_err());
    }

    #[test]
    fn validates_rate_limits() {
        let ok = RateLimitConfig {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
            sustained: SustainedRate {
                rate: 10,
                window: crate::domain::model::RateWindow::Second,
            },
            burst: None,
            scope: crate::domain::model::RateScope::Tenant,
            strategy: crate::domain::model::RateStrategy::Reject,
            response_headers: true,
            cost: 1,
        };
        assert_eq!(validate_rate_limit(&ok).ok(), Some(()));
        let zero_rate = RateLimitConfig {
            sustained: SustainedRate {
                rate: 0,
                window: crate::domain::model::RateWindow::Second,
            },
            ..ok
        };
        assert!(validate_rate_limit(&zero_rate).is_err());
    }

    #[test]
    fn validates_route_matches() {
        let both = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: Some(GrpcMatch {
                service: "svc".to_owned(),
                method: "Get".to_owned(),
            }),
        };
        assert!(validate_match(&both).is_err());
        assert!(validate_match(&MatchConfig::default()).is_err());
        let no_methods = MatchConfig {
            http: Some(HttpMatch {
                methods: Vec::new(),
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&no_methods).is_err());
        let bad_method = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["OPTIONS".to_owned()],
                path: "/".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&bad_method).is_err());
        let empty_path = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: String::new(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&empty_path).is_err());
        let relative_path = MatchConfig {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "v1/chat".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&relative_path).is_err());
        let grpc_only = MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: "foo.v1.UserService".to_owned(),
                method: "GetUser".to_owned(),
            }),
        };
        assert_eq!(validate_match(&grpc_only).ok(), Some(()));
    }
}
