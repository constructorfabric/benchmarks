//! CORS evaluation (`ADR/0004`).
//!
//! Preflight is answered locally, never forwarded. Actual requests carry the
//! configured headers when the origin and the method are allowed.

use crate::domain::error::DomainError;
use crate::domain::model::Cors;

/// Builds the `Access-Control-Allow-Origin` value for an allowed origin.
#[must_use]
pub fn allow_origin(config: &Cors, origin: &str) -> Option<String> {
    if !config.enabled {
        return None;
    }
    if config.allowed_origins.iter().any(|allowed| allowed == "*") {
        return Some(if config.allow_credentials {
            origin.to_owned()
        } else {
            "*".to_owned()
        });
    }
    if config
        .allowed_origins
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(origin))
    {
        return Some(origin.to_owned());
    }
    None
}

/// Whether a method is allowed by the CORS configuration.
#[must_use]
pub fn method_allowed(config: &Cors, method: &str) -> bool {
    if !config.enabled {
        return true;
    }
    config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(method))
}

/// Validates an actual (non-preflight) request against the CORS config.
///
/// # Errors
/// [`DomainError::CorsOriginNotAllowed`] / [`DomainError::CorsMethodNotAllowed`]
/// when the origin or the method is not allowed.
pub fn validate_actual(
    config: &Cors,
    origin: Option<&str>,
    method: &str,
) -> Result<(), DomainError> {
    if !config.enabled {
        return Ok(());
    }
    let Some(origin) = origin else {
        return Ok(());
    };
    if allow_origin(config, origin).is_none() {
        return Err(DomainError::CorsOriginNotAllowed(origin.to_owned()));
    }
    if !method_allowed(config, method) {
        return Err(DomainError::CorsMethodNotAllowed(method.to_owned()));
    }
    Ok(())
}

/// Header set returned for a preflight response.
#[derive(Debug, Clone, Default)]
pub struct PreflightHeaders {
    /// `Access-Control-Allow-Origin`.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Methods`.
    pub allow_methods: Option<String>,
    /// `Access-Control-Allow-Headers`.
    pub allow_headers: Option<String>,
    /// `Access-Control-Allow-Credentials`.
    pub allow_credentials: bool,
    /// `Access-Control-Max-Age`.
    pub max_age: Option<u32>,
    /// `Vary`.
    pub vary: Option<String>,
}

/// Builds the preflight response headers.
///
/// ADR 0004 § 3: a preflight is answered with `204` even when the alias has no
/// CORS configuration, so an unconfigured upstream yields the permissive
/// default set.
#[must_use]
pub fn preflight(
    config: Option<&Cors>,
    origin: &str,
    requested_method: &str,
    requested_headers: &str,
) -> PreflightHeaders {
    let Some(config) = config.filter(|c| c.enabled) else {
        return PreflightHeaders {
            allow_origin: Some("*".to_owned()),
            allow_methods: Some(requested_method.to_owned()),
            allow_headers: Some(requested_headers.to_owned()),
            ..PreflightHeaders::default()
        };
    };

    let origin_allowed = allow_origin(config, origin).is_some();
    let method_allowed = config
        .allowed_methods
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(requested_method));
    let requested: Vec<&str> = requested_headers
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .collect();

    if origin_allowed && method_allowed {
        PreflightHeaders {
            allow_origin: allow_origin(config, origin),
            allow_methods: Some(config.allowed_methods.join(", ")),
            allow_headers: (!requested.is_empty()).then(|| requested.join(", ")),
            allow_credentials: config.allow_credentials,
            max_age: Some(600),
            vary: None,
        }
    } else {
        PreflightHeaders::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(origins: &[&str], methods: &[&str], credentials: bool) -> Cors {
        Cors {
            enabled: true,
            allowed_origins: origins.iter().map(|s| (*s).to_owned()).collect(),
            allowed_methods: methods.iter().map(|s| (*s).to_owned()).collect(),
            allow_credentials: credentials,
            ..Cors::default()
        }
    }

    #[test]
    fn a_listed_origin_is_echoed() {
        let config = cors(&["https://app.example"], &["GET"], false);
        assert_eq!(
            allow_origin(&config, "https://app.example").as_deref(),
            Some("https://app.example")
        );
        assert!(allow_origin(&config, "https://evil.example").is_none());
    }

    #[test]
    fn the_wildcard_is_not_echoed_when_credentials_are_allowed() {
        let config = cors(&["*"], &["GET"], true);
        assert_eq!(
            allow_origin(&config, "https://app.example").as_deref(),
            Some("https://app.example")
        );
        let no_credentials = cors(&["*"], &["GET"], false);
        assert_eq!(
            allow_origin(&no_credentials, "https://app.example").as_deref(),
            Some("*")
        );
    }

    #[test]
    fn an_actual_request_is_validated() {
        let config = cors(&["https://app.example"], &["GET", "POST"], false);
        assert!(validate_actual(&config, Some("https://app.example"), "POST").is_ok());
        let bad_origin =
            validate_actual(&config, Some("https://evil.example"), "POST").expect_err("rejected");
        assert_eq!(bad_origin.status(), 403);
        let bad_method =
            validate_actual(&config, Some("https://app.example"), "DELETE").expect_err("rejected");
        assert_eq!(bad_method.status(), 403);
    }

    #[test]
    fn a_request_without_cors_is_unrestricted() {
        let mut config = cors(&["https://app.example"], &["GET"], false);
        config.enabled = false;
        assert!(validate_actual(&config, Some("https://evil.example"), "TRACE").is_ok());
    }

    #[test]
    fn an_unconfigured_upstream_preflight_is_permissive() {
        let headers = preflight(None, "https://app.example", "POST", "X-Custom");
        assert_eq!(headers.allow_origin.as_deref(), Some("*"));
        assert_eq!(headers.allow_methods.as_deref(), Some("POST"));
        assert_eq!(headers.allow_headers.as_deref(), Some("X-Custom"));
    }

    #[test]
    fn a_configured_preflight_echoes_the_allowed_set() {
        let mut config = cors(&["https://app.example"], &["GET", "POST"], false);
        config.expose_headers = vec!["X-OAGW-Error-Source".to_owned()];
        let headers = preflight(
            Some(&config),
            "https://app.example",
            "POST",
            "X-Custom, X-Other",
        );
        assert_eq!(headers.allow_origin.as_deref(), Some("https://app.example"));
        assert!(
            headers
                .allow_methods
                .as_deref()
                .is_some_and(|m| m.contains("POST"))
        );
        assert_eq!(headers.allow_headers.as_deref(), Some("X-Custom, X-Other"));
        assert_eq!(headers.max_age, Some(600));

        let denied = preflight(Some(&config), "https://evil.example", "POST", "");
        assert!(denied.allow_origin.is_none());
    }
}
