//! CORS evaluation (ADR-0004).
//!
//! OAGW owns the CORS decision because the effective policy is the merged
//! hierarchical configuration, and because a preflight must be answered by the
//! gateway itself rather than by the upstream.

use axum::http::{HeaderName, HeaderValue, Method};

use crate::domain::model::CorsConfig;

/// `Access-Control-Allow-Origin`.
pub const ALLOW_ORIGIN: &str = "access-control-allow-origin";
/// `Access-Control-Allow-Methods`.
pub const ALLOW_METHODS: &str = "access-control-allow-methods";
/// `Access-Control-Allow-Headers`.
pub const ALLOW_HEADERS: &str = "access-control-allow-headers";
/// `Access-Control-Expose-Headers`.
pub const EXPOSE_HEADERS: &str = "access-control-expose-headers";
/// `Access-Control-Allow-Credentials`.
pub const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
/// `Access-Control-Max-Age`.
pub const MAX_AGE: &str = "access-control-max-age";
/// `Origin`.
pub const ORIGIN: &str = "origin";
/// `Access-Control-Request-Method`.
pub const REQUEST_METHOD: &str = "access-control-request-method";
/// `Access-Control-Request-Headers`.
pub const REQUEST_HEADERS: &str = "access-control-request-headers";
/// `Vary`.
pub const VARY: &str = "vary";
/// `Access-Control-Max-Age` the gateway answers a preflight with (ADR-0004).
///
/// The CORS configuration model carries no `max_age` field, so the permissive
/// answer publishes the ADR's example value.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// Headers a CORS decision touches, so caches never serve a cross-tenant
/// answer for a same-origin request.
pub const VARY_VALUE: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// Outcome of a simple (non-preflight) request evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimpleCors {
    /// `Access-Control-Allow-Origin`, when the request is allowed.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Credentials`, when credentials are permitted.
    pub allow_credentials: bool,
    /// `Access-Control-Expose-Headers`.
    pub expose_headers: Vec<String>,
}

impl SimpleCors {
    /// Renders the headers onto a response header map.
    pub fn apply(&self, headers: &mut axum::http::HeaderMap) {
        if let Some(value) = self
            .allow_origin
            .as_ref()
            .and_then(|origin| HeaderValue::from_str(origin).ok())
        {
            headers.insert(HeaderName::from_static(ALLOW_ORIGIN), value);
        }
        if self.allow_credentials {
            headers.insert(
                HeaderName::from_static(ALLOW_CREDENTIALS),
                HeaderValue::from_static("true"),
            );
        }
        if let Some(value) = (!self.expose_headers.is_empty())
            .then(|| self.expose_headers.join(", "))
            .and_then(|joined| HeaderValue::from_str(&joined).ok())
        {
            headers.insert(HeaderName::from_static(EXPOSE_HEADERS), value);
        }
        if self.allow_origin.is_some() {
            append_vary(headers, VARY_VALUE);
        }
    }
}

/// Adds a `Vary` token without clobbering the ones already present: an upstream
/// `Vary: Accept-Encoding` and the CORS `Vary: Origin` are both cache-relevant.
fn append_vary(headers: &mut axum::http::HeaderMap, token: &str) {
    let present = |value: &str, wanted: &str| {
        value
            .split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(wanted))
    };
    let existing = headers
        .get(VARY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let merged = match existing {
        Some(existing) => {
            let fresh: Vec<&str> = token
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty() && !present(&existing, part))
                .collect();
            if fresh.is_empty() {
                return;
            }
            format!("{existing}, {}", fresh.join(", "))
        }
        None => token.to_owned(),
    };
    if let Ok(value) = HeaderValue::from_str(&merged) {
        headers.insert(HeaderName::from_static(VARY), value);
    }
}

/// `true` when `origin` is explicitly listed (the wildcard is handled apart).
fn allows(config: &CorsConfig, origin: &str) -> bool {
    config
        .allowed_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// Evaluates a simple cross-origin request.
#[must_use]
pub fn evaluate(config: &CorsConfig, origin: Option<&str>) -> SimpleCors {
    if !config.enabled {
        return SimpleCors::default();
    }
    let Some(origin) = origin else {
        return SimpleCors::default();
    };
    if !allows(config, origin) {
        return SimpleCors::default();
    }
    let wildcard = config.allowed_origins.iter().any(|allowed| allowed == "*");
    SimpleCors {
        // Echoing the exact origin is what lets a credentialled policy keep a
        // wildcard list without ever emitting `*` alongside credentials.
        allow_origin: Some(if wildcard && !config.allow_credentials {
            "*".to_owned()
        } else {
            origin.to_owned()
        }),
        allow_credentials: config.allow_credentials,
        expose_headers: config.expose_headers.clone(),
    }
}

/// Headers for the permissive preflight answer (ADR-0004).
///
/// Browser preflights carry no credentials, so there is no tenant context to
/// resolve an upstream with: the gateway echoes what the browser asked for and
/// defers origin/method validation to the actual request.
#[must_use]
pub fn permissive_preflight(
    origin: &str,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        (ALLOW_ORIGIN, origin.to_owned()),
        (ALLOW_METHODS, request_method.unwrap_or_default().to_owned()),
        (MAX_AGE, PREFLIGHT_MAX_AGE.to_owned()),
    ];
    if let Some(requested) = request_headers
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        headers.push((ALLOW_HEADERS, requested.to_owned()));
    }
    headers
}

/// Enforces the policy on an actual cross-origin request (ADR-0004).
///
/// Preflight is permissive by design; this is where a disallowed origin or
/// method is rejected, before the request reaches the upstream. Requests
/// without an `Origin` header are not cross-origin and pass untouched, as do
/// upstreams with CORS disabled.
///
/// # Errors
///
/// [`DomainError::CorsOriginNotAllowed`] for an origin outside the allowlist,
/// [`DomainError::CorsMethodNotAllowed`] for a method outside it.
pub fn enforce(
    config: Option<&CorsConfig>,
    origin: Option<&str>,
    method: &Method,
) -> Result<(), crate::domain::error::DomainError> {
    let Some(origin) = origin else {
        return Ok(());
    };
    let Some(config) = config else {
        // No effective policy at either level: nothing to enforce.
        return Ok(());
    };
    if !config.enabled {
        return Ok(());
    }
    if !allows(config, origin) {
        return Err(crate::domain::error::DomainError::CorsOriginNotAllowed {
            origin: origin.to_owned(),
        });
    }
    let method_allowed = config
        .allowed_methods
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(method.as_str()));
    if !method_allowed {
        return Err(crate::domain::error::DomainError::CorsMethodNotAllowed {
            method: method.as_str().to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Sharing;

    fn config(origins: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: Sharing::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: credentials,
        }
    }

    #[test]
    fn disabled_policy_emits_nothing() {
        let mut config = config(&["https://a.example"], false);
        config.enabled = false;
        assert_eq!(
            evaluate(&config, Some("https://a.example")),
            SimpleCors::default()
        );
    }

    #[test]
    fn unknown_origins_are_not_allowed() {
        let config = config(&["https://a.example"], false);
        assert_eq!(
            evaluate(&config, Some("https://evil.example")),
            SimpleCors::default()
        );
        assert_eq!(evaluate(&config, None), SimpleCors::default());
    }

    #[test]
    fn wildcard_without_credentials_emits_a_star() {
        let config = config(&["*"], false);
        let cors = evaluate(&config, Some("https://a.example"));
        assert_eq!(cors.allow_origin.as_deref(), Some("*"));
        assert!(!cors.allow_credentials);
    }

    #[test]
    fn credentials_never_emit_a_bare_star() {
        let config = config(&["*"], true);
        let cors = evaluate(&config, Some("https://a.example"));
        assert_eq!(cors.allow_origin.as_deref(), Some("https://a.example"));
        assert!(cors.allow_credentials);
    }

    #[test]
    fn the_permissive_preflight_echoes_the_browser_request() {
        let headers = permissive_preflight(
            "https://app.example",
            Some("POST"),
            Some("Content-Type, Authorization"),
        );
        assert_eq!(headers[0], (ALLOW_ORIGIN, "https://app.example".to_owned()));
        assert_eq!(headers[1], (ALLOW_METHODS, "POST".to_owned()));
        assert_eq!(headers[2], (MAX_AGE, PREFLIGHT_MAX_AGE.to_owned()));
        assert_eq!(
            headers[3],
            (ALLOW_HEADERS, "Content-Type, Authorization".to_owned())
        );
        // No requested headers means nothing to allow.
        assert!(
            !permissive_preflight("https://app.example", Some("GET"), None)
                .iter()
                .any(|(name, _)| *name == ALLOW_HEADERS)
        );
    }

    #[test]
    fn a_vary_token_is_appended_not_clobbered() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(VARY, HeaderValue::from_static("Accept-Encoding"));
        append_vary(&mut headers, VARY_VALUE);
        assert_eq!(
            headers.get(VARY).and_then(|value| value.to_str().ok()),
            Some(
                "Accept-Encoding, Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
            )
        );
        append_vary(&mut headers, VARY_VALUE);
        // Appending twice does not duplicate the token.
        assert_eq!(
            headers
                .get(VARY)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.matches("Origin").count()),
            Some(1)
        );
    }

    #[test]
    fn enforcement_needs_an_origin_and_a_policy() {
        let config = config(&["https://app.example"], false);
        // No Origin header: not a cross-origin request.
        assert!(enforce(Some(&config), None, &Method::POST).is_ok());
        // No policy at either level: nothing to enforce.
        assert!(enforce(None, Some("https://evil.example"), &Method::POST).is_ok());
        let mut disabled = config.clone();
        disabled.enabled = false;
        assert!(enforce(Some(&disabled), Some("https://evil.example"), &Method::POST).is_ok());
    }

    #[test]
    fn disallowed_origins_and_methods_are_rejected_on_the_actual_request() {
        let config = config(&["https://app.example"], false);
        assert!(enforce(Some(&config), Some("https://app.example"), &Method::POST).is_ok());
        let origin = enforce(Some(&config), Some("https://evil.example"), &Method::POST);
        assert!(matches!(
            origin,
            Err(crate::domain::error::DomainError::CorsOriginNotAllowed { .. })
        ));
        let method = enforce(Some(&config), Some("https://app.example"), &Method::DELETE);
        assert!(matches!(
            method,
            Err(crate::domain::error::DomainError::CorsMethodNotAllowed { .. })
        ));
    }

    #[test]
    fn a_wildcard_policy_admits_every_origin_but_not_every_method() {
        let config = config(&["*"], false);
        assert!(
            enforce(
                Some(&config),
                Some("https://anywhere.example"),
                &Method::GET
            )
            .is_ok()
        );
        assert!(
            enforce(
                Some(&config),
                Some("https://anywhere.example"),
                &Method::DELETE
            )
            .is_err()
        );
    }

    #[test]
    fn headers_render_onto_the_response() {
        let cors = SimpleCors {
            allow_origin: Some("https://a.example".to_owned()),
            allow_credentials: true,
            expose_headers: vec!["x-request-id".to_owned()],
        };
        let mut headers = axum::http::HeaderMap::new();
        cors.apply(&mut headers);
        assert_eq!(headers.get(ALLOW_ORIGIN).unwrap(), "https://a.example");
        assert_eq!(headers.get(ALLOW_CREDENTIALS).unwrap(), "true");
        assert!(headers.contains_key(VARY));
    }
}
