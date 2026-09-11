//! Write-time validation for control-plane payloads.
//!
//! Realizes `cpt-cf-oagw-algo-um-validate-payload`,
//! `cpt-cf-oagw-algo-rm-validate-match-payload` and the CORS write-time rule.

use crate::domain::error::DomainError;
use crate::domain::model::{
    CorsConfig, Endpoint, HttpMatch, RateLimit, RouteMatch, Scheme, Server,
};

/// The methods a route may name.
pub const ALLOWED_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// The two protocol identifiers the schema admits.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// The gRPC protocol identifier; accepted structurally, not served.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Validate a tag list against `^[a-z0-9_-]+$`.
///
/// # Errors
/// Returns [`DomainError::Validation`] naming the offending index.
pub fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    for (i, t) in tags.iter().enumerate() {
        if t.is_empty()
            || !t
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'))
        {
            return Err(DomainError::validation(
                format!("tags[{i}]"),
                "tags must match ^[a-z0-9_-]+$",
            ));
        }
    }
    Ok(())
}

/// Validate the endpoint pool.
///
/// The accepted `scheme` set is widened beyond the frozen schema's TLS family
/// to include the plaintext counterparts, so `{"scheme":"http","port":80}` is
/// a legal endpoint. Whether a plaintext connection is actually made is decided
/// at connect time by `allow_http_upstream`, not here.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an empty pool, an empty host, or a
/// port outside 1-65535.
// @cpt-begin:cpt-cf-oagw-dod-um-scheme-widening:p1:inst-full
pub fn validate_server(server: &Server) -> Result<(), DomainError> {
    if server.endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints",
            "at least one endpoint is required",
        ));
    }
    for (i, e) in server.endpoints.iter().enumerate() {
        validate_endpoint(i, e)?;
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-um-scheme-widening:p1:inst-full

fn validate_endpoint(i: usize, e: &Endpoint) -> Result<(), DomainError> {
    if e.host.trim().is_empty() {
        return Err(DomainError::validation(
            format!("server.endpoints[{i}].host"),
            "host must not be empty",
        ));
    }
    if e.port == Some(0) {
        return Err(DomainError::validation(
            format!("server.endpoints[{i}].port"),
            "port must be between 1 and 65535",
        ));
    }
    // Every variant of `Scheme` is accepted at this layer, including the
    // plaintext ones. Nothing to reject.
    let _ = e.scheme;
    Ok(())
}

/// Validate the protocol identifier.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an unknown identifier.
pub fn validate_protocol(protocol: &str) -> Result<(), DomainError> {
    if protocol == PROTOCOL_HTTP || protocol == PROTOCOL_GRPC {
        Ok(())
    } else {
        Err(DomainError::validation(
            "protocol",
            format!("protocol must be `{PROTOCOL_HTTP}` or `{PROTOCOL_GRPC}`"),
        ))
    }
}

/// Validate a rate-limit block.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the sustained rate or the burst
/// capacity is below one.
pub fn validate_rate_limit(field: &str, rl: &RateLimit) -> Result<(), DomainError> {
    if rl.sustained.rate < 1 {
        return Err(DomainError::validation(
            format!("{field}.sustained.rate"),
            "sustained rate must be at least 1",
        ));
    }
    if rl.burst.capacity.is_some_and(|c| c < 1) {
        return Err(DomainError::validation(
            format!("{field}.burst.capacity"),
            "burst capacity must be at least 1",
        ));
    }
    if rl.cost < 1 {
        return Err(DomainError::validation(
            format!("{field}.cost"),
            "cost must be at least 1",
        ));
    }
    Ok(())
}

/// Validate a CORS block.
///
/// Rejects the wildcard-origin-with-credentials combination at write time, per
/// `cpt-cf-oagw-dod-um-cors-validation`.
///
/// # Errors
/// Returns [`DomainError::Validation`] on the wildcard/credentials conflict or
/// an unknown method.
// @cpt-begin:cpt-cf-oagw-dod-um-cors-validation:p1:inst-full
pub fn validate_cors(field: &str, cors: &CorsConfig) -> Result<(), DomainError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::validation(
            format!("{field}.allowed_origins"),
            "a wildcard origin cannot be combined with allow_credentials",
        ));
    }
    for (i, m) in cors.allowed_methods.iter().enumerate() {
        let up = m.to_ascii_uppercase();
        if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"].contains(&up.as_str()) {
            return Err(DomainError::validation(
                format!("{field}.allowed_methods[{i}]"),
                format!("`{m}` is not a recognized HTTP method"),
            ));
        }
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-um-cors-validation:p1:inst-full

/// Validate a route match block: exactly one of `http` or `grpc`, with its own
/// required fields.
///
/// # Errors
/// Returns [`DomainError::Validation`] when neither or both are present, or a
/// required sub-field is missing or malformed.
// @cpt-begin:cpt-cf-oagw-dod-rm-create-route:p1:inst-full
pub fn validate_match(m: &RouteMatch) -> Result<(), DomainError> {
    match (&m.http, &m.grpc) {
        (Some(h), None) => validate_http_match(h),
        (None, Some(g)) => {
            if g.service.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.service",
                    "service must not be empty",
                ));
            }
            if g.method.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.method",
                    "method must not be empty",
                ));
            }
            Ok(())
        }
        (Some(_), Some(_)) => Err(DomainError::validation(
            "match",
            "exactly one of `http` or `grpc` may be present",
        )),
        (None, None) => Err(DomainError::validation(
            "match",
            "exactly one of `http` or `grpc` is required",
        )),
    }
}
// @cpt-end:cpt-cf-oagw-dod-rm-create-route:p1:inst-full

fn validate_http_match(h: &HttpMatch) -> Result<(), DomainError> {
    if h.methods.is_empty() {
        return Err(DomainError::validation(
            "match.http.methods",
            "at least one method is required",
        ));
    }
    for (i, m) in h.methods.iter().enumerate() {
        if !ALLOWED_METHODS.contains(&m.to_ascii_uppercase().as_str()) {
            return Err(DomainError::validation(
                format!("match.http.methods[{i}]"),
                format!("`{m}` is not one of {ALLOWED_METHODS:?}"),
            ));
        }
    }
    if h.path.is_empty() {
        return Err(DomainError::validation(
            "match.http.path",
            "path must not be empty",
        ));
    }
    Ok(())
}

/// Whether an endpoint's scheme may actually be connected to under the current
/// configuration. Plaintext requires `allow_http_upstream`.
#[must_use]
pub const fn connection_permitted(scheme: Scheme, allow_http_upstream: bool) -> bool {
    scheme.is_tls() || allow_http_upstream
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Burst, PathSuffixMode, RateAlgorithm, RateScope, RateStrategy, Sharing, Sustained, Window};

    fn server(scheme: Scheme, host: &str, port: Option<u16>) -> Server {
        Server {
            endpoints: vec![Endpoint {
                scheme,
                host: host.to_owned(),
                port,
            }],
        }
    }

    #[test]
    fn a_plaintext_http_endpoint_is_accepted_at_write_time() {
        // This is the widened-scheme rule: creating the upstream must succeed
        // even though the frozen schema's enum lists only the TLS family.
        validate_server(&server(Scheme::Http, "example.com", Some(80))).unwrap();
    }

    #[test]
    fn a_plaintext_websocket_endpoint_is_accepted_at_write_time() {
        validate_server(&server(Scheme::Ws, "example.com", Some(80))).unwrap();
    }

    #[test]
    fn tls_endpoints_remain_accepted() {
        validate_server(&server(Scheme::Https, "example.com", None)).unwrap();
        validate_server(&server(Scheme::Wss, "example.com", None)).unwrap();
    }

    #[test]
    fn an_empty_pool_is_rejected() {
        let err = validate_server(&Server { endpoints: vec![] }).unwrap_err();
        assert!(matches!(err, DomainError::Validation { ref field, .. } if field == "server.endpoints"));
    }

    #[test]
    fn an_empty_host_is_rejected() {
        assert!(validate_server(&server(Scheme::Http, "  ", Some(80))).is_err());
    }

    #[test]
    fn scheme_acceptance_and_connection_permission_are_separate_questions() {
        // Accepted at write time regardless.
        validate_server(&server(Scheme::Http, "example.com", Some(80))).unwrap();
        // But only connected to when the flag allows it.
        assert!(!connection_permitted(Scheme::Http, false));
        assert!(connection_permitted(Scheme::Http, true));
        // TLS is always permitted.
        assert!(connection_permitted(Scheme::Https, false));
    }

    #[test]
    fn protocol_must_be_one_of_the_two_identifiers() {
        validate_protocol(PROTOCOL_HTTP).unwrap();
        validate_protocol(PROTOCOL_GRPC).unwrap();
        assert!(validate_protocol("gts.cf.core.oagw.protocol.v1~nope").is_err());
    }

    #[test]
    fn tags_must_match_the_schema_pattern() {
        validate_tags(&["a-b".to_owned(), "c_d".to_owned(), "e9".to_owned()]).unwrap();
        assert!(validate_tags(&["Upper".to_owned()]).is_err());
        assert!(validate_tags(&["has space".to_owned()]).is_err());
        assert!(validate_tags(&[String::new()]).is_err());
    }

    fn rl(rate: u32, capacity: Option<u32>, cost: u32) -> RateLimit {
        RateLimit {
            sharing: Sharing::Private,
            algorithm: RateAlgorithm::TokenBucket,
            sustained: Sustained { rate, window: Window::Second },
            burst: Burst { capacity },
            scope: RateScope::Tenant,
            strategy: RateStrategy::Reject,
            cost,
        }
    }

    #[test]
    fn rate_limit_bounds_are_enforced() {
        validate_rate_limit("rate_limit", &rl(1, None, 1)).unwrap();
        assert!(validate_rate_limit("rate_limit", &rl(0, None, 1)).is_err());
        assert!(validate_rate_limit("rate_limit", &rl(1, Some(0), 1)).is_err());
        assert!(validate_rate_limit("rate_limit", &rl(1, None, 0)).is_err());
    }

    fn cors(origins: &[&str], creds: bool) -> CorsConfig {
        CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec![],
            allow_credentials: creds,
        }
    }

    #[test]
    fn wildcard_origin_with_credentials_is_rejected_at_write_time() {
        assert!(validate_cors("cors", &cors(&["*"], true)).is_err());
        validate_cors("cors", &cors(&["*"], false)).unwrap();
        validate_cors("cors", &cors(&["https://a.example"], true)).unwrap();
    }

    #[test]
    fn an_unknown_cors_method_is_rejected() {
        let mut c = cors(&["https://a.example"], false);
        c.allowed_methods = vec!["TELEPORT".to_owned()];
        assert!(validate_cors("cors", &c).is_err());
    }

    #[test]
    fn a_match_needs_exactly_one_kind() {
        let http = RouteMatch {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        validate_match(&http).unwrap();

        let neither = RouteMatch { http: None, grpc: None };
        assert!(validate_match(&neither).is_err());

        let both = RouteMatch {
            http: http.http.clone(),
            grpc: Some(crate::domain::model::GrpcMatch {
                service: "s".to_owned(),
                method: "m".to_owned(),
            }),
        };
        assert!(validate_match(&both).is_err());
    }

    #[test]
    fn an_unsupported_method_is_rejected() {
        let m = RouteMatch {
            http: Some(HttpMatch {
                methods: vec!["TRACE".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&m).is_err());
    }

    #[test]
    fn an_empty_path_is_rejected() {
        let m = RouteMatch {
            http: Some(HttpMatch {
                methods: vec!["GET".to_owned()],
                path: String::new(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_match(&m).is_err());
    }
}
