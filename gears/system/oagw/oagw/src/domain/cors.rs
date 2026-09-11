//! Built-in CORS handling (`ADR/0004-cors.md`).
//!
//! Two distinct paths:
//!
//! * **Preflight** — `OPTIONS` carrying `Origin` and
//!   `Access-Control-Request-Method`. Browsers send no credentials on a
//!   preflight, so there is no tenant context and therefore no upstream to
//!   resolve: the handler answers a permissive `204` that echoes the request.
//! * **Actual request** — origin and method are validated against the
//!   effective policy *after* upstream resolution and *before* forwarding.

use super::error::{DomainError, DomainResult};
use super::model::CorsConfig;

/// Preflight cache lifetime advertised to browsers.
pub const PREFLIGHT_MAX_AGE_SECS: u32 = 86_400;

/// The `Vary` value for a preflight response.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// Header names echoed back on a preflight response.
#[derive(Debug, Clone)]
pub struct PreflightEcho {
    /// Echoed `Access-Control-Allow-Origin`.
    pub origin: String,
    /// Echoed `Access-Control-Allow-Methods`.
    pub methods: Option<String>,
    /// Echoed `Access-Control-Allow-Headers`.
    pub headers: Option<String>,
}

/// Detect a CORS preflight: `OPTIONS` plus `Origin` plus
/// `Access-Control-Request-Method`.
#[must_use]
pub fn is_preflight(method: &str, origin: Option<&str>, request_method: Option<&str>) -> bool {
    method.eq_ignore_ascii_case("OPTIONS") && origin.is_some() && request_method.is_some()
}

/// Build the permissive echo for a preflight.
#[must_use]
pub fn preflight_echo(
    origin: &str,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> PreflightEcho {
    PreflightEcho {
        origin: origin.to_owned(),
        methods: request_method.map(str::to_owned),
        headers: request_headers.map(str::to_owned),
    }
}

/// Reject a policy that pairs credentials with a wildcard origin.
///
/// # Errors
///
/// `400` when `allow_credentials` is set alongside `allowed_origins: ["*"]`.
pub fn validate_config(config: &CorsConfig) -> DomainResult<()> {
    if config.allow_credentials && config.allowed_origins.iter().any(|o| o == "*") {
        return Err(DomainError::validation(
            "cors.allow_credentials cannot be combined with the wildcard origin '*'",
        ));
    }
    for origin in &config.allowed_origins {
        if origin == "*" {
            continue;
        }
        // Exact, scheme- and port-sensitive matching only; no patterns.
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins entries must be an absolute origin or '*': '{origin}'"
            )));
        }
        if origin.contains('*') {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins does not support patterns: '{origin}'"
            )));
        }
    }
    Ok(())
}

/// Exact origin match. Port- and scheme-sensitive by construction; `*`
/// matches anything.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed == origin)
}

/// Method membership check, case-insensitive on the method token.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// Headers to add to a response for an allowed actual cross-origin request.
#[must_use]
pub fn response_headers(config: &CorsConfig, origin: &str) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    // Echo the concrete origin rather than `*` whenever credentials are in
    // play; `Vary: Origin` is always present to keep caches honest.
    let allow_origin = if config.allow_credentials || !config.allowed_origins.iter().any(|o| o == "*")
    {
        origin.to_owned()
    } else {
        "*".to_owned()
    };
    headers.push(("access-control-allow-origin", allow_origin));
    if config.allow_credentials {
        headers.push(("access-control-allow-credentials", "true".to_owned()));
    }
    if !config.expose_headers.is_empty() {
        headers.push((
            "access-control-expose-headers",
            config.expose_headers.join(", "),
        ));
    }
    headers.push(("vary", "Origin".to_owned()));
    headers
}

/// Enforce the policy on an actual (non-preflight) cross-origin request.
///
/// # Errors
///
/// `403` with the origin- or method-specific GTS type when the request is
/// not permitted.
pub fn enforce(config: &CorsConfig, origin: &str, method: &str) -> DomainResult<()> {
    if !origin_allowed(config, origin) {
        return Err(DomainError::cors_origin_not_allowed(format!(
            "Origin '{origin}' not in allowed origins list"
        )));
    }
    if !method_allowed(config, method) {
        return Err(DomainError::cors_method_not_allowed(format!(
            "Method '{method}' not in allowed methods list"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::errors;

    fn config() -> CorsConfig {
        CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        }
    }

    #[test]
    fn preflight_detection_requires_all_three_signals() {
        assert!(is_preflight(
            "OPTIONS",
            Some("https://app.example.com"),
            Some("POST")
        ));
        assert!(!is_preflight("OPTIONS", Some("https://a"), None));
        assert!(!is_preflight("OPTIONS", None, Some("POST")));
        assert!(!is_preflight("POST", Some("https://a"), Some("POST")));
    }

    #[test]
    fn preflight_echoes_the_request() {
        let echo = preflight_echo(
            "https://app.example.com",
            Some("POST"),
            Some("Content-Type, Authorization"),
        );
        assert_eq!(echo.origin, "https://app.example.com");
        assert_eq!(echo.methods.as_deref(), Some("POST"));
        assert_eq!(echo.headers.as_deref(), Some("Content-Type, Authorization"));
    }

    #[test]
    fn origin_matching_is_exact() {
        let cfg = config();
        assert!(origin_allowed(&cfg, "https://app.example.com"));
        assert!(!origin_allowed(&cfg, "https://evil.com"));
        assert!(!origin_allowed(&cfg, "https://app.example.com:8080"));
        assert!(!origin_allowed(&cfg, "http://app.example.com"));
    }

    #[test]
    fn wildcard_matches_any_origin() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            ..CorsConfig::default()
        };
        assert!(origin_allowed(&cfg, "https://anything.example"));
    }

    #[test]
    fn credentials_with_wildcard_is_rejected_at_validation_time() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn patterns_are_rejected() {
        let cfg = CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://*.example.com".to_owned()],
            ..CorsConfig::default()
        };
        assert!(validate_config(&cfg).is_err());
    }

    #[test]
    fn disallowed_origin_and_method_map_to_distinct_types() {
        let cfg = config();
        let origin_err = enforce(&cfg, "https://evil.com", "GET").expect_err("origin rejected");
        assert_eq!(origin_err.status(), 403);
        assert_eq!(origin_err.gts_type(), errors::CORS_ORIGIN_NOT_ALLOWED);

        let method_err =
            enforce(&cfg, "https://app.example.com", "DELETE").expect_err("method rejected");
        assert_eq!(method_err.status(), 403);
        assert_eq!(method_err.gts_type(), errors::CORS_METHOD_NOT_ALLOWED);
    }

    #[test]
    fn response_headers_always_vary_on_origin() {
        let headers = response_headers(&config(), "https://app.example.com");
        assert!(headers.iter().any(|(k, v)| *k == "vary" && v == "Origin"));
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "access-control-allow-origin" && v == "https://app.example.com")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "access-control-allow-credentials" && v == "true")
        );
        assert!(
            headers
                .iter()
                .any(|(k, v)| *k == "access-control-expose-headers" && v == "X-Request-ID")
        );
    }
}
