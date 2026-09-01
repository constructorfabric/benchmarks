//! CORS policy evaluation (ADR-0004).
//!
//! Pure functions over [`crate::domain::model::CorsConfig`]: the Data Plane
//! calls these for preflight and actual cross-origin requests. Preflight
//! handling is permissive at the handler level (no upstream resolution, no
//! tenant context); actual requests are validated before forwarding.

use crate::domain::error::DomainError;
use crate::domain::model::CorsConfig;

/// Headers required on a CORS preflight response, per ADR-0004.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";
/// `Access-Control-Max-Age` used for preflight responses.
pub const PREFLIGHT_MAX_AGE_SECS: u64 = 86_400;

/// Returns `true` when the origin matches an entry of `allowed_origins`.
///
/// Matching is protocol- and port-sensitive: `https://app.example.com` does
/// not match `http://app.example.com` nor `https://app.example.com:8443`.
/// A wildcard entry matches every origin; matching is ASCII
/// case-insensitive on scheme and host.
#[must_use]
pub fn origin_allowed(config: &CorsConfig, origin: &str) -> bool {
    if !config.enabled {
        return false;
    }
    if config.allowed_origins.iter().any(|o| o == "*") {
        return true;
    }
    let normalized = origin.trim().to_ascii_lowercase();
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed.trim().to_ascii_lowercase() == normalized)
}

/// Returns `true` when the request method is allowed for cross-origin requests.
#[must_use]
pub fn method_allowed(config: &CorsConfig, method: &str) -> bool {
    if !config.enabled {
        return false;
    }
    config
        .allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
}

/// Validates a CORS configuration at create/replace time (ADR-0004).
///
/// `docs/schemas/upstream.v1.schema.json` and `docs/schemas/route.v1.schema.json`
/// encode the same rule as an `if/then`: `allow_credentials: true` requires
/// explicit origins, so `allowed_origins` containing `"*"` is rejected at
/// validation time rather than at proxy time.
///
/// # Errors
///
/// 400 [`DomainError::Validation`] when `allow_credentials` is combined with
/// the wildcard origin.
pub fn validate_config(config: &CorsConfig) -> Result<(), DomainError> {
    if config.allow_credentials && config.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(DomainError::Validation(
            "allow_credentials requires explicit allowed_origins; '*' is not permitted".to_owned(),
        ));
    }
    Ok(())
}

/// Returns `true` when this pair of headers identifies an OPTIONS preflight.
#[must_use]
pub fn is_preflight(method: &str, headers: &http::HeaderMap) -> bool {
    method == http::Method::OPTIONS.as_str()
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Validates an actual (non-preflight) cross-origin request.
///
/// # Errors
///
/// * origin outside `allowed_origins` → 403 `cors.origin_not_allowed.v1`
/// * method outside `allowed_methods` → 403 `cors.method_not_allowed.v1`
pub fn validate_actual_request(
    config: Option<&CorsConfig>,
    origin: Option<&http::HeaderValue>,
    method: &str,
) -> Result<(), DomainError> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let Some(config) = config.filter(|cfg| cfg.enabled) else {
        return Ok(());
    };
    let origin = origin.to_str().unwrap_or_default();
    if !origin_allowed(config, origin) {
        return Err(DomainError::CorsOriginNotAllowed(format!(
            "origin '{origin}' is not in the upstream allowed_origins"
        )));
    }
    if !method_allowed(config, method) {
        return Err(DomainError::CorsMethodNotAllowed(format!(
            "method '{method}' is not in the upstream allowed_methods"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::model::SharingMode;

    fn config(origins: &[&str], methods: &[&str], enabled: bool) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: false,
        }
    }

    #[test]
    fn origin_matching_is_protocol_and_port_sensitive() {
        let cfg = config(&["https://app.example.com"], &["GET"], true);
        assert!(origin_allowed(&cfg, "https://app.example.com"));
        assert!(!origin_allowed(&cfg, "https://app.example.com:8443"));
        assert!(!origin_allowed(&cfg, "http://app.example.com"));
        assert!(!origin_allowed(&cfg, "https://evil.example.com"));
        assert!(!origin_allowed(&cfg, ""));
    }

    #[test]
    fn wildcard_matches_everything() {
        let cfg = config(&["*"], &["GET"], true);
        assert!(origin_allowed(&cfg, "https://anything.example"));
    }

    #[test]
    fn disabled_config_allows_nothing() {
        let cfg = config(&["*"], &["GET"], false);
        assert!(!origin_allowed(&cfg, "https://app.example.com"));
        assert!(!method_allowed(&cfg, "GET"));
    }

    #[test]
    fn validate_config_rejects_credentials_with_wildcard_origin() {
        let mut cfg = config(&["*"], &["GET"], true);
        cfg.allow_credentials = true;
        let error = validate_config(&cfg).expect_err("wildcard + credentials");
        assert!(matches!(error, DomainError::Validation(_)));
        assert_eq!(error.status(), 400);

        // Explicit origins with credentials are fine.
        let explicit = config(&["https://app.example.com"], &["GET"], true);
        let mut credentialed = explicit;
        credentialed.allow_credentials = true;
        assert!(validate_config(&credentialed).is_ok());

        // Wildcard without credentials is allowed (discouraged but legal).
        assert!(validate_config(&cfg).is_err());
        let mut public = cfg.clone();
        public.allow_credentials = false;
        assert!(validate_config(&public).is_ok());
    }

    #[test]
    fn validate_actual_request_rejects_bad_origin_and_method() {
        let cfg = config(&["https://app.example.com"], &["GET", "POST"], true);
        let origin = http::HeaderValue::from_static("https://app.example.com");
        assert!(validate_actual_request(Some(&cfg), Some(&origin), "GET").is_ok());
        let foreign = http::HeaderValue::from_static("https://evil.example");
        assert!(matches!(
            validate_actual_request(Some(&cfg), Some(&foreign), "GET"),
            Err(DomainError::CorsOriginNotAllowed(_))
        ));
        assert!(matches!(
            validate_actual_request(Some(&cfg), Some(&origin), "DELETE"),
            Err(DomainError::CorsMethodNotAllowed(_))
        ));
        // No origin header (same-origin or non-browser client) → allowed.
        assert!(validate_actual_request(Some(&cfg), None, "DELETE").is_ok());
        // No CORS config at all → allowed.
        assert!(validate_actual_request(None, Some(&origin), "GET").is_ok());
    }
}
