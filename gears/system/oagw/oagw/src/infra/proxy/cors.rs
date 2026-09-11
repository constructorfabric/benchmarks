//! Built-in CORS handling (ADR-0004).
//!
//! Preflight is answered at the handler level with a permissive `204` that
//! echoes the request back — a browser preflight carries no credentials, so
//! there is no tenant context to resolve an upstream with. Origin and method
//! enforcement therefore happens on the *actual* request, after upstream
//! resolution and before the request is forwarded.

use http::header::{HeaderMap, HeaderValue};
use http::{Method, StatusCode};

use crate::domain::error::OagwError;
use crate::domain::model::CorsConfig;

/// `Access-Control-Max-Age` emitted on a preflight response.
pub const PREFLIGHT_MAX_AGE_SECONDS: u32 = 86_400;

/// `true` when this is a browser CORS preflight: `OPTIONS` plus `Origin` plus
/// `Access-Control-Request-Method`.
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Build the permissive preflight response headers.
#[must_use]
pub fn preflight_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    if let Some(origin) = headers.get(http::header::ORIGIN) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get(http::header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(requested) = headers.get(http::header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            requested.clone(),
        );
    }
    if let Ok(max_age) = HeaderValue::from_str(&PREFLIGHT_MAX_AGE_SECONDS.to_string()) {
        out.insert(http::header::ACCESS_CONTROL_MAX_AGE, max_age);
    }
    // Always vary on the negotiation inputs so a shared cache cannot serve one
    // origin's preflight answer to another.
    out.insert(
        http::header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    out
}

/// Status a preflight is answered with.
pub const PREFLIGHT_STATUS: StatusCode = StatusCode::NO_CONTENT;

/// Enforce the policy on an actual cross-origin request.
///
/// Returns `Ok(None)` when the request is not cross-origin (no `Origin`) or
/// CORS is not enabled, and `Ok(Some(origin))` when it passed.
///
/// # Errors
///
/// * `403 cors.origin_not_allowed` — the origin is not in `allowed_origins`;
/// * `403 cors.method_not_allowed` — the method is not in `allowed_methods`.
pub fn enforce_actual_request(
    cors: Option<&CorsConfig>,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Option<String>, OagwError> {
    let Some(cors) = cors.filter(|c| c.enabled) else {
        return Ok(None);
    };
    let Some(origin) = headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
    else {
        // Not a browser cross-origin request; CORS does not apply.
        return Ok(None);
    };
    if !cors.origin_allowed(&origin) {
        return Err(OagwError::cors_origin_not_allowed(format!(
            "Origin '{origin}' not in allowed origins list"
        )));
    }
    let allowed_methods = cors.effective_methods();
    if !allowed_methods
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method.as_str()))
    {
        return Err(OagwError::cors_method_not_allowed(format!(
            "Method '{method}' not in allowed methods list"
        )));
    }
    Ok(Some(origin))
}

/// Add the actual-request CORS response headers.
pub fn apply_response_headers(headers: &mut HeaderMap, cors: &CorsConfig, origin: &str) {
    // Echo the concrete origin (never `*`) whenever credentials are in play,
    // and echo it regardless so `Vary: Origin` stays meaningful.
    let allow_origin = if cors.allowed_origins.iter().any(|o| o == "*") && !cors.allow_credentials {
        "*".to_owned()
    } else {
        origin.to_owned()
    };
    if let Ok(value) = HeaderValue::from_str(&allow_origin) {
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    if cors.allow_credentials {
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    headers.append(http::header::VARY, HeaderValue::from_static("Origin"));
}

#[cfg(test)]
mod tests {
    use super::{
        PREFLIGHT_MAX_AGE_SECONDS, apply_response_headers, enforce_actual_request, is_preflight,
        preflight_headers,
    };
    use crate::domain::gts_helpers as gts;
    use crate::domain::model::CorsConfig;
    use http::Method;
    use http::header::{HeaderMap, HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_bytes(name.as_bytes()).expect("name"),
                HeaderValue::from_str(value).expect("value"),
            );
        }
        map
    }

    fn policy() -> CorsConfig {
        CorsConfig {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: Some(vec!["GET".to_owned(), "POST".to_owned()]),
            expose_headers: vec!["X-Request-ID".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        }
    }

    #[test]
    fn preflight_detection_needs_all_three_signals() {
        let full = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ]);
        assert!(is_preflight(&Method::OPTIONS, &full));
        assert!(!is_preflight(&Method::POST, &full));
        assert!(!is_preflight(
            &Method::OPTIONS,
            &headers(&[("origin", "https://app.example.com")])
        ));
    }

    #[test]
    fn preflight_echoes_the_request_and_varies_on_it() {
        let request = headers(&[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "DELETE"),
            (
                "access-control-request-headers",
                "Content-Type, Authorization",
            ),
        ]);
        let out = preflight_headers(&request);
        assert_eq!(
            out["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(out["access-control-allow-methods"], "DELETE");
        assert_eq!(
            out["access-control-allow-headers"],
            "Content-Type, Authorization"
        );
        assert_eq!(
            out["access-control-max-age"],
            PREFLIGHT_MAX_AGE_SECONDS.to_string()
        );
        let vary = out["vary"].to_str().expect("ascii");
        assert!(vary.contains("Origin"));
        assert!(vary.contains("Access-Control-Request-Method"));
        assert!(vary.contains("Access-Control-Request-Headers"));
    }

    #[test]
    fn a_disallowed_origin_is_refused_on_the_actual_request() {
        let request = headers(&[("origin", "https://evil.com")]);
        let err =
            enforce_actual_request(Some(&policy()), &Method::GET, &request).expect_err("refused");
        assert_eq!(err.status, 403);
        assert_eq!(err.error_type, gts::ERR_CORS_ORIGIN_NOT_ALLOWED);
    }

    #[test]
    fn a_disallowed_method_is_refused_on_the_actual_request() {
        let request = headers(&[("origin", "https://app.example.com")]);
        let err = enforce_actual_request(Some(&policy()), &Method::DELETE, &request)
            .expect_err("refused");
        assert_eq!(err.status, 403);
        assert_eq!(err.error_type, gts::ERR_CORS_METHOD_NOT_ALLOWED);
    }

    #[test]
    fn origin_matching_is_protocol_and_port_sensitive() {
        for origin in [
            "http://app.example.com",
            "https://app.example.com:8443",
            "https://app.example.com.evil.com",
        ] {
            let request = headers(&[("origin", origin)]);
            assert!(
                enforce_actual_request(Some(&policy()), &Method::GET, &request).is_err(),
                "{origin} must not match"
            );
        }
    }

    #[test]
    fn a_same_origin_request_is_unaffected() {
        assert_eq!(
            enforce_actual_request(Some(&policy()), &Method::GET, &HeaderMap::new())
                .expect("no origin"),
            None
        );
    }

    #[test]
    fn cors_disabled_never_refuses() {
        let disabled = CorsConfig::default();
        let request = headers(&[("origin", "https://evil.com")]);
        assert_eq!(
            enforce_actual_request(Some(&disabled), &Method::GET, &request).expect("ignored"),
            None
        );
        assert_eq!(
            enforce_actual_request(None, &Method::GET, &request).expect("ignored"),
            None
        );
    }

    #[test]
    fn response_headers_echo_the_origin_with_credentials() {
        let mut out = HeaderMap::new();
        apply_response_headers(&mut out, &policy(), "https://app.example.com");
        assert_eq!(
            out["access-control-allow-origin"],
            "https://app.example.com"
        );
        assert_eq!(out["access-control-allow-credentials"], "true");
        assert_eq!(out["access-control-expose-headers"], "X-Request-ID");
        assert_eq!(out["vary"], "Origin");
    }

    #[test]
    fn a_wildcard_policy_without_credentials_answers_with_a_wildcard() {
        let wildcard = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            ..CorsConfig::default()
        };
        let mut out = HeaderMap::new();
        apply_response_headers(&mut out, &wildcard, "https://anything.example.com");
        assert_eq!(out["access-control-allow-origin"], "*");
        assert!(!out.contains_key("access-control-allow-credentials"));
    }
}
