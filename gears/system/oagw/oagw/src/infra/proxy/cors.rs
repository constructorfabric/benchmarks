//! Built-in CORS handler (ADR-0004).
//!
//! A preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//! answered locally with a permissive `204` — no upstream resolution and no
//! tenant context, since a preflight carries no credentials. On an actual
//! request the origin and the method are validated against the merged
//! upstream/route configuration *before* the request is forwarded, and the
//! CORS response headers are added to the passthrough.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::json;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::model::CorsConfig;

/// `Access-Control-Max-Age` of a preflight answer (one day, per ADR-0004).
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// Instance id of a refused origin.
pub const ORIGIN_NOT_ALLOWED: &str = "cf.oagw.cors.origin_not_allowed.v1";
/// Instance id of a refused method.
pub const METHOD_NOT_ALLOWED: &str = "cf.oagw.cors.method_not_allowed.v1";

/// Whether the request is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &str, headers: &HeaderMap) -> bool {
    method.eq_ignore_ascii_case("OPTIONS")
        && headers.contains_key(axum::http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

/// The merged CORS policy of an upstream and its route.
///
/// The route's origins are unioned onto the upstream's unless the route
/// *enforces* its own; the policy is active when either side enables it.
#[must_use]
pub fn effective_config(
    upstream: Option<&CorsConfig>,
    route: Option<&CorsConfig>,
) -> Option<CorsConfig> {
    let upstream = upstream?;
    if !upstream.enabled && !route.is_some_and(|route| route.enabled) {
        return None;
    }
    let Some(route) = route else {
        return Some(clone_config(upstream));
    };
    if route.sharing == crate::domain::model::SharingMode::Enforce {
        return Some(clone_config(route));
    }
    let mut merged = clone_config(upstream);
    merged.enabled = upstream.enabled || route.enabled;
    for origin in &route.allowed_origins {
        if !merged.allowed_origins.iter().any(|known| known == origin) {
            merged.allowed_origins.push(origin.clone());
        }
    }
    if !route.allowed_methods.is_empty() {
        for method in &route.allowed_methods {
            if !merged.allowed_methods.iter().any(|known| known == method) {
                merged.allowed_methods.push(method.clone());
            }
        }
    }
    for header in &route.expose_headers {
        if !merged.expose_headers.iter().any(|known| known == header) {
            merged.expose_headers.push(header.clone());
        }
    }
    merged.allow_credentials = upstream.allow_credentials || route.allow_credentials;
    Some(merged)
}

fn clone_config(config: &CorsConfig) -> CorsConfig {
    CorsConfig {
        sharing: config.sharing,
        enabled: config.enabled,
        allowed_origins: config.allowed_origins.clone(),
        allowed_methods: config.allowed_methods.clone(),
        expose_headers: config.expose_headers.clone(),
        allow_credentials: config.allow_credentials,
    }
}

/// Whether an origin is allowed: exact match, or the `*` wildcard.
///
/// Matching is protocol- and port-sensitive because the whole origin is
/// compared; there are no patterns (ADR-0004 §Origin Matching).
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// Whether a method is allowed by the policy.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// Validates an origin/method pair of an actual request.
///
/// # Errors
/// Returns the ADR-0004 403 problems for a disallowed origin or method.
pub fn check_actual(config: &CorsConfig, origin: &str, method: &str) -> Result<(), DomainError> {
    if !origin_allowed(config, origin) {
        return Err(DomainError::new(
            ErrorKind::CorsOriginNotAllowed,
            format!("origin {origin:?} is not in the allowed origins of this upstream"),
        )
        .with_field("origin", json!(origin))
        .with_field(
            "allowed_origins",
            json!(config.allowed_origins.iter().collect::<Vec<_>>()),
        ));
    }
    if !method_allowed(config, method) {
        return Err(DomainError::new(
            ErrorKind::CorsMethodNotAllowed,
            format!("method {method:?} is not in the allowed methods of this upstream"),
        )
        .with_field("method", json!(method.to_ascii_uppercase())));
    }
    Ok(())
}

/// The headers a preflight answer carries.
///
/// The origin, method and requested headers of the *caller* are echoed back,
/// which makes the answer permissive: enforcement happens on the actual
/// request.
#[must_use]
pub fn preflight_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    let origin = origin_of(headers).unwrap_or_else(|| "*".to_owned());
    let requested = headers
        .get("access-control-request-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*")
        .to_owned();
    let requested_headers = headers
        .get("access-control-request-headers")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let mut emitted = vec![
        ("Access-Control-Allow-Origin".to_owned(), origin),
        ("Access-Control-Allow-Methods".to_owned(), requested),
        (
            "Access-Control-Max-Age".to_owned(),
            PREFLIGHT_MAX_AGE.to_owned(),
        ),
        (
            "Vary".to_owned(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
        ),
    ];
    if !requested_headers.is_empty() {
        emitted.push(("Access-Control-Allow-Headers".to_owned(), requested_headers));
    }
    emitted
}

/// The headers added to an allowed actual request.
#[must_use]
pub fn actual_headers(config: &CorsConfig, origin: &str) -> Vec<(String, String)> {
    let mut emitted = vec![
        ("Access-Control-Allow-Origin".to_owned(), origin.to_owned()),
        ("Vary".to_owned(), "Origin".to_owned()),
    ];
    if config.allow_credentials {
        emitted.push((
            "Access-Control-Allow-Credentials".to_owned(),
            "true".to_owned(),
        ));
    }
    if !config.expose_headers.is_empty() {
        emitted.push((
            "Access-Control-Expose-Headers".to_owned(),
            config.expose_headers.join(", "),
        ));
    }
    emitted
}

/// The `Origin` header of a request, trimmed.
#[must_use]
pub fn origin_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_owned)
}

/// The name/value pair that survives rendering.
pub fn to_header_pair(name: &str, value: &str) -> Option<(String, String)> {
    match (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        (Ok(_), Ok(_)) => Some((name.to_owned(), value.to_owned())),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::domain::model::SharingMode;

    fn config(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    #[test]
    fn preflight_needs_origin_and_request_method() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        assert!(!is_preflight("OPTIONS", &headers), "no request method yet");
        headers.insert("access-control-request-method", "POST".parse().unwrap());
        assert!(is_preflight("OPTIONS", &headers));
        assert!(
            !is_preflight("POST", &headers),
            "the method must be OPTIONS"
        );
    }

    #[test]
    fn origin_matching_is_exact_and_case_insensitive() {
        let config = config(&["https://app.example.com"], &["GET"]);
        assert!(origin_allowed(&config, "https://app.example.com"));
        assert!(origin_allowed(&config, "https://APP.example.com"));
        for evil in [
            "https://evil.com",
            "https://app.example.com:8080",
            "http://app.example.com",
            "https://evil.com.example.com",
        ] {
            assert!(!origin_allowed(&config, evil), "{evil} must be refused");
        }
    }

    #[test]
    fn the_wildcard_matches_any_origin() {
        let config = config(&["*"], &["GET"]);
        assert!(origin_allowed(&config, "https://anything.example.org"));
    }

    #[test]
    fn credentials_cannot_be_combined_with_the_wildcard() {
        let mut invalid = config(&["*"], &["GET"]);
        invalid.allow_credentials = true;
        assert!(
            crate::domain::validation::validate_cors(&invalid).is_err(),
            "wildcard + credentials is rejected"
        );
        let mut valid = config(&["https://app.example.com"], &["GET"]);
        valid.allow_credentials = true;
        assert!(crate::domain::validation::validate_cors(&valid).is_ok());
    }

    #[test]
    fn a_disallowed_origin_is_a_403_problem() {
        let config = config(&["https://app.example.com"], &["GET"]);
        let error = check_actual(&config, "https://evil.com", "GET").unwrap_err();
        assert_eq!(error.kind, ErrorKind::CorsOriginNotAllowed);
        assert_eq!(error.status(), 403);
        assert_eq!(error.kind.gts_instance(), ORIGIN_NOT_ALLOWED);
        assert_eq!(error.field("origin"), Some(&json!("https://evil.com")));
    }

    #[test]
    fn a_disallowed_method_is_a_403_problem() {
        let config = config(&["https://app.example.com"], &["GET"]);
        let error = check_actual(&config, "https://app.example.com", "DELETE").unwrap_err();
        assert_eq!(error.status(), 403);
        assert_eq!(error.kind.gts_instance(), METHOD_NOT_ALLOWED);
        assert_eq!(error.field("method"), Some(&json!("DELETE")));
    }

    #[test]
    fn an_inheriting_route_unions_onto_the_upstream() {
        let upstream = config(&["https://app.example.com"], &["GET"]);
        let route = config(&["https://admin.example.com"], &["DELETE"]);
        let merged = effective_config(Some(&upstream), Some(&route)).expect("merged");
        assert_eq!(
            merged.allowed_origins,
            vec!["https://app.example.com", "https://admin.example.com"]
        );
        assert_eq!(merged.allowed_methods, vec!["GET", "DELETE"]);
        assert!(merged.enabled, "either side enables the policy");
    }

    #[test]
    fn a_route_can_enable_a_disabled_upstream_policy() {
        let mut upstream = config(&["https://app.example.com"], &["GET"]);
        upstream.enabled = false;
        let route = config(&["https://admin.example.com"], &["GET"]);
        let merged = effective_config(Some(&upstream), Some(&route)).expect("merged");
        assert!(merged.enabled);
    }

    #[test]
    fn an_enforcing_route_keeps_only_its_own_origins() {
        let upstream = config(&["https://app.example.com"], &["GET"]);
        let mut route = config(&["https://admin.example.com"], &["GET"]);
        route.sharing = SharingMode::Enforce;
        let merged = effective_config(Some(&upstream), Some(&route)).expect("merged");
        assert_eq!(merged.allowed_origins, vec!["https://admin.example.com"]);
    }

    #[test]
    fn an_unconfigured_policy_is_no_cors_handling() {
        assert!(effective_config(None, None).is_none());
        let disabled = CorsConfig {
            sharing: SharingMode::Private,
            enabled: false,
            allowed_origins: vec!["*".to_owned()],
            allowed_methods: default_allowed(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        };
        assert!(effective_config(Some(&disabled), None).is_none());
    }

    #[test]
    fn the_preflight_echoes_the_caller() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "https://app.example.com".parse().unwrap(),
        );
        headers.insert("access-control-request-method", "POST".parse().unwrap());
        headers.insert(
            "access-control-request-headers",
            "Content-Type, Authorization".parse().unwrap(),
        );
        let emitted = preflight_headers(&headers);
        let lookup = |name: &str| {
            emitted
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            lookup("access-control-allow-origin").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(
            lookup("access-control-allow-methods").as_deref(),
            Some("POST")
        );
        assert_eq!(
            lookup("access-control-allow-headers").as_deref(),
            Some("Content-Type, Authorization")
        );
        assert_eq!(lookup("access-control-max-age").as_deref(), Some("86400"));
        assert!(lookup("vary").unwrap().contains("Origin"));
    }

    #[test]
    fn an_allowed_actual_request_gets_the_cors_headers() {
        let mut config = config(&["https://app.example.com"], &["GET"]);
        config.allow_credentials = true;
        config.expose_headers = vec!["X-Request-ID".to_owned()];
        let emitted = actual_headers(&config, "https://app.example.com");
        let lookup = |name: &str| {
            emitted
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        };
        assert_eq!(
            lookup("access-control-allow-origin").as_deref(),
            Some("https://app.example.com")
        );
        assert_eq!(
            lookup("access-control-allow-credentials").as_deref(),
            Some("true")
        );
        assert_eq!(
            lookup("access-control-expose-headers").as_deref(),
            Some("X-Request-ID")
        );
    }

    fn default_allowed() -> Vec<String> {
        vec!["GET".to_owned(), "POST".to_owned()]
    }
}
