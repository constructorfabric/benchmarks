//! Built-in CORS handling (ADR-0004).
//!
//! * Preflight (`OPTIONS` + `Origin` + `Access-Control-Request-Method`) is
//!   answered at the handler level with a permissive `204` that echoes the
//!   requested origin/method/headers — before upstream resolution and without
//!   tenant context (preflight requests carry no credentials).
//! * Actual cross-origin requests are validated against the effective CORS
//!   config after upstream resolution, before forwarding.
//! * Secure defaults: CORS is disabled unless configured; no regex origin
//!   matching; `Vary: Origin` is always emitted to prevent cache poisoning.

use http::Method;

use crate::domain::dto::ProxyResponse;
use crate::domain::error::{OagwError, OagwResult};
use crate::domain::merge::EffectiveCors;

const ORIGIN: &str = "origin";
const ACRM: &str = "access-control-request-method";
const ACRH: &str = "access-control-request-headers";

/// Whether the request is a CORS preflight (OPTIONS with Origin + ACRM).
#[must_use]
pub fn is_preflight(method: &Method, headers: &[(String, String)]) -> bool {
    if method != Method::OPTIONS {
        return false;
    }
    has_header(headers, ORIGIN) && has_header(headers, ACRM)
}

/// Build the handler-level permissive preflight response (204, no body).
#[must_use]
pub fn preflight_response(headers: &[(String, String)]) -> ProxyResponse {
    let origin = header_value(headers, ORIGIN).unwrap_or_else(|| "*".to_owned());
    let method = header_value(headers, ACRM)
        .unwrap_or("GET,".to_owned())
        .replace(' ', "");
    let requested_headers = header_value(headers, ACRH).unwrap_or_default();

    let mut out = vec![
        ("access-control-allow-origin".to_owned(), origin),
        ("access-control-allow-methods".to_owned(), method),
        ("access-control-max-age".to_owned(), "86400".to_owned()),
        (
            "vary".to_owned(),
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned(),
        ),
    ];
    if !requested_headers.is_empty() {
        out.push(("access-control-allow-headers".to_owned(), requested_headers));
    }
    ProxyResponse {
        status: http::StatusCode::NO_CONTENT,
        headers: out,
        body: bytes::Bytes::new(),
    }
}

/// Validate an actual cross-origin request against the effective CORS config.
/// Non-browser requests (no `Origin`) bypass CORS entirely.
pub fn validate_actual(cors: &EffectiveCors, origin: Option<&str>, method: &str) -> OagwResult<()> {
    let Some(origin) = origin else {
        return Ok(());
    };
    if cors.enabled {
        if !cors.allows_origin(origin) {
            return Err(OagwError::CorsOriginNotAllowed {
                origin: origin.to_owned(),
            });
        }
        if !cors.allows_method(method) {
            return Err(OagwError::CorsMethodNotAllowed {
                method: method.to_owned(),
            });
        }
    }
    Ok(())
}

/// Append CORS response headers for an actual request.
pub fn append_response_headers(
    headers: &mut Vec<(String, String)>,
    cors: &EffectiveCors,
    origin: Option<&str>,
) {
    if !cors.enabled {
        return;
    }
    let Some(origin) = origin else {
        return;
    };
    if !cors.allows_origin(origin) {
        return;
    }
    let wildcard = cors.allowed_origins.iter().any(|o| o == "*");
    push_unique(
        headers,
        "access-control-allow-origin",
        if wildcard && !cors.allow_credentials {
            "*".to_owned()
        } else {
            origin.to_owned()
        },
    );
    if !cors.expose_headers.is_empty() {
        push_unique(
            headers,
            "access-control-expose-headers",
            cors.expose_headers.join(", "),
        );
    }
    if cors.allow_credentials {
        push_unique(
            headers,
            "access-control-allow-credentials",
            "true".to_owned(),
        );
    }
    push_unique(headers, "vary", ORIGIN.to_owned());
}

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

fn header_value(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn push_unique(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return;
    }
    headers.push((name.to_owned(), value));
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn preflight_detection() {
        let method = Method::OPTIONS;
        let headers = vec![
            ("origin".to_owned(), "https://app.example.com".to_owned()),
            (
                "access-control-request-method".to_owned(),
                "POST".to_owned(),
            ),
        ];
        assert!(is_preflight(&method, &headers));
        assert!(!is_preflight(&Method::GET, &headers));
    }

    #[test]
    fn preflight_echoes_origin_and_method() {
        let headers = vec![
            ("origin".to_owned(), "https://app.example.com".to_owned()),
            (
                "access-control-request-method".to_owned(),
                "POST".to_owned(),
            ),
            (
                "access-control-request-headers".to_owned(),
                "Content-Type, Authorization".to_owned(),
            ),
        ];
        let resp = preflight_response(&headers);
        assert_eq!(resp.status, http::StatusCode::NO_CONTENT);
        let body = resp
            .headers
            .iter()
            .find(|(n, _)| n == "access-control-allow-origin")
            .map(|(_, v)| v.clone())
            .expect("origin echoed");
        assert_eq!(body, "https://app.example.com");
        assert!(resp.body.is_empty());
    }

    #[test]
    fn actual_request_origin_and_method_validation() {
        let cors = EffectiveCors {
            enabled: true,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
            ..Default::default()
        };
        assert!(validate_actual(&cors, Some("https://app.example.com"), "GET").is_ok());
        assert!(validate_actual(&cors, Some("https://evil.com"), "GET").is_err());
        assert!(validate_actual(&cors, Some("https://app.example.com"), "DELETE").is_err());
        // No origin => not a browser request => pass.
        assert!(validate_actual(&cors, None, "DELETE").is_ok());
    }
}
