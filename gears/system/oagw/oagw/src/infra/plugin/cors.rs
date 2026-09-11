//! The built-in CORS handler (ADR 0004).
//!
//! CORS is core data-plane behaviour, not a plugin: a preflight is answered
//! before any upstream is contacted, and the actual request is checked against
//! the operator's origins and methods before a byte leaves the gateway.
//! Origin matching is exact and case-insensitive on the scheme and host, and a
//! configuration that combines credentials with a wildcard origin is rejected
//! at configuration time by [`crate::domain::services::control_plane::validate_cors`].

use http::HeaderMap;
use http::header;

use crate::domain::error::DomainError;
use crate::domain::model::Cors;

/// The `Origin` header a browser sends on every cross-origin request.
pub const ORIGIN: &str = "origin";
/// The header naming the method an actual request will use.
pub const REQUEST_METHOD: &str = "access-control-request-method";
/// The header naming the headers an actual request will carry.
pub const REQUEST_HEADERS: &str = "access-control-request-headers";

/// Response header naming the origins allowed.
pub const ALLOW_ORIGIN: &str = "access-control-allow-origin";
/// Response header naming the methods allowed.
pub const ALLOW_METHODS: &str = "access-control-allow-methods";
/// Response header naming the headers allowed.
pub const ALLOW_HEADERS: &str = "access-control-allow-headers";
/// Response header advertising the preflight cache lifetime.
pub const MAX_AGE: &str = "access-control-max-age";
/// Response header naming the headers the caller may read.
pub const EXPOSE_HEADERS: &str = "access-control-expose-headers";
/// Response header allowing credentials through.
pub const ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";

/// Whether `request` is a CORS preflight: `OPTIONS` with an origin and the
/// method it wants to use.
#[must_use]
pub fn is_preflight(method: &http::Method, headers: &HeaderMap) -> bool {
    method == http::Method::OPTIONS
        && headers.get(header::ORIGIN).is_some()
        && headers.get(REQUEST_METHOD).is_some()
}

/// Whether a non-preflight request is a CORS request at all: only one carrying
/// an `Origin` is, because a client that did not send one is not cross-origin.
#[must_use]
pub fn is_cross_origin(headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).is_some()
}

/// Whether `origin` is in the configured list.
///
/// Matching is exact and case-insensitive: an origin is a scheme plus a host
/// and optionally a port, and two origins that differ in any of those are
/// different origins (ADR 0004 — port-sensitive, protocol-sensitive, no
/// patterns).
#[must_use]
pub fn origin_allowed(cors: &Cors, origin: &str) -> bool {
    let origin = origin.trim();
    cors.allow_origins
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(origin))
}

/// Whether `method` is in the configuration's allowlist; `*` allows any.
#[must_use]
pub fn method_allowed(cors: &Cors, method: &http::Method) -> bool {
    cors.allow_methods
        .iter()
        .any(|allowed| allowed == "*" || allowed.eq_ignore_ascii_case(method.as_str()))
}

/// The answer a preflight gets, echoing what the browser asked for.
///
/// A preflight carries no credentials, so no tenant context is available and
/// nothing here is validated: origin and method enforcement happens on the
/// actual request that follows. Header values the caller sent are echoed, so a
/// value that is not representable is simply left off the answer.
#[must_use]
pub fn preflight_answer(
    cors: Option<&Cors>,
    request: &HeaderMap,
) -> http::Response<axum::body::Body> {
    let mut answer = http::Response::new(axum::body::Body::empty());
    *answer.status_mut() = http::StatusCode::NO_CONTENT;
    let headers = answer.headers_mut();
    put(headers, ALLOW_ORIGIN, header_value(request, header::ORIGIN));
    put(
        headers,
        ALLOW_METHODS,
        header_value(request, REQUEST_METHOD),
    );
    put(
        headers,
        ALLOW_HEADERS,
        header_value(request, REQUEST_HEADERS),
    );
    put(
        headers,
        header::VARY.as_str(),
        Some("Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_owned()),
    );
    if let Some(cors) = cors {
        put(headers, MAX_AGE, Some(cors.max_age_secs.to_string()));
        if cors.allow_credentials {
            put(headers, ALLOW_CREDENTIALS, Some("true".to_owned()));
        }
    }
    answer
}

/// Set `name` on `headers` when `value` is representable as a header value.
fn put(headers: &mut HeaderMap, name: &str, value: Option<String>) {
    let Some(value) = value else { return };
    if let (Ok(name), Ok(value)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        http::HeaderValue::from_bytes(value.as_bytes()),
    ) {
        headers.insert(name, value);
    }
}

/// Refuse an actual cross-origin request the configuration does not allow.
///
/// # Errors
/// Returns [`ErrorKind::CorsOriginNotAllowed`] or
/// [`ErrorKind::CorsMethodNotAllowed`], the documented 400s.
pub fn check_request(
    cors: &Cors,
    method: &http::Method,
    request: &HeaderMap,
) -> Result<(), DomainError> {
    let Some(origin) = header_value(request, header::ORIGIN) else {
        return Ok(());
    };
    if !origin_allowed(cors, &origin) {
        return Err(DomainError::new(
            crate::domain::error::ErrorKind::CorsOriginNotAllowed,
            format!("origin '{origin}' is not in the allowed origins list"),
        ));
    }
    if !method_allowed(cors, method) {
        return Err(DomainError::new(
            crate::domain::error::ErrorKind::CorsMethodNotAllowed,
            format!(
                "method '{}' is not in the allowed methods list",
                method.as_str()
            ),
        ));
    }
    Ok(())
}

/// Add the CORS headers an actual request's answer carries.
///
/// The origin the caller sent is echoed, never a wildcard: a configuration
/// that lists explicit origins advertises those origins, and one carrying
/// credentials may not advertise a wildcard at all (that pair is refused at
/// configuration time).
pub fn apply_response_headers(cors: &Cors, origin: Option<&str>, response: &mut http::HeaderMap) {
    let wanted = origin
        .map(str::trim)
        .filter(|origin| origin_allowed(cors, origin))
        .and_then(|origin| http::HeaderValue::from_str(origin).ok());
    if let Some(value) = wanted {
        response.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
    }
    if cors.allow_credentials {
        response.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            http::HeaderValue::from_static("true"),
        );
    }
    vary_on_origin(response);
}

/// The answer depends on the origin that was sent, so caches are told so.
fn vary_on_origin(response: &mut http::HeaderMap) {
    let already = response
        .get(header::VARY)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("origin"))
        });
    if !already {
        response.append(header::VARY, http::HeaderValue::from_static("Origin"));
    }
}

fn header_value(headers: &HeaderMap, name: impl http::header::AsHeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod cors_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn config(origins: &[&str], methods: &[&str]) -> Cors {
        Cors {
            allow_origins: origins.iter().map(|origin| (*origin).to_owned()).collect(),
            allow_methods: methods.iter().map(|method| (*method).to_owned()).collect(),
            ..Cors::default()
        }
    }

    #[test]
    fn only_an_options_with_a_requested_method_is_a_preflight() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://app.example.com".parse().unwrap());
        assert!(!is_preflight(&http::Method::OPTIONS, &headers));
        headers.insert(REQUEST_METHOD, "POST".parse().unwrap());
        assert!(is_preflight(&http::Method::OPTIONS, &headers));
        assert!(!is_preflight(&http::Method::POST, &headers));
    }

    #[test]
    fn origins_match_exactly_and_case_insensitively() {
        let cors = config(&["https://app.example.com"], &["GET"]);
        assert!(origin_allowed(&cors, "https://app.example.com"));
        assert!(origin_allowed(&cors, "https://APP.example.com"));
        assert!(!origin_allowed(&cors, "https://evil.example.com"));
        // Port- and protocol-sensitive.
        assert!(!origin_allowed(&cors, "https://app.example.com:8443"));
        assert!(!origin_allowed(&cors, "http://app.example.com"));
    }

    #[test]
    fn the_wildcard_origin_allows_everything() {
        let cors = config(&["*"], &["GET"]);
        assert!(origin_allowed(&cors, "https://anything.example.net"));
    }

    #[test]
    fn methods_match_the_allowlist() {
        let cors = config(&["https://a.example.com"], &["GET", "POST"]);
        assert!(method_allowed(&cors, &http::Method::GET));
        assert!(method_allowed(&cors, &http::Method::POST));
        assert!(!method_allowed(&cors, &http::Method::DELETE));
    }

    #[test]
    fn a_request_without_an_origin_is_never_refused() {
        let cors = config(&["https://a.example.com"], &["GET"]);
        let headers = HeaderMap::new();
        assert!(check_request(&cors, &http::Method::GET, &headers).is_ok());
    }

    #[test]
    fn a_disallowed_origin_is_refused_by_kind() {
        let cors = config(&["https://a.example.com"], &["GET"]);
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://evil.example.net".parse().unwrap());
        let error = check_request(&cors, &http::Method::GET, &headers).unwrap_err();
        assert_eq!(
            error.kind(),
            crate::domain::error::ErrorKind::CorsOriginNotAllowed
        );
    }

    #[test]
    fn a_disallowed_method_is_refused_by_kind() {
        let cors = config(&["https://a.example.com"], &["GET"]);
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://a.example.com".parse().unwrap());
        let error = check_request(&cors, &http::Method::DELETE, &headers).unwrap_err();
        assert_eq!(
            error.kind(),
            crate::domain::error::ErrorKind::CorsMethodNotAllowed
        );
    }

    #[test]
    fn the_preflight_answer_echoes_the_request() {
        let cors = config(&["https://a.example.com"], &["GET"]);
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://a.example.com".parse().unwrap());
        headers.insert(REQUEST_METHOD, "GET".parse().unwrap());
        headers.insert(REQUEST_HEADERS, "content-type".parse().unwrap());

        let answer = preflight_answer(Some(&cors), &headers);
        assert_eq!(answer.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            answer.headers().get(ALLOW_ORIGIN).unwrap(),
            "https://a.example.com"
        );
        assert_eq!(answer.headers().get(ALLOW_METHODS).unwrap(), "GET");
        assert_eq!(answer.headers().get(ALLOW_HEADERS).unwrap(), "content-type");
    }

    #[test]
    fn the_preflight_answer_stands_without_a_configuration() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://a.example.com".parse().unwrap());
        headers.insert(REQUEST_METHOD, "GET".parse().unwrap());

        let answer = preflight_answer(None, &headers);
        assert_eq!(answer.status(), http::StatusCode::NO_CONTENT);
        assert_eq!(
            answer.headers().get(ALLOW_ORIGIN).unwrap(),
            "https://a.example.com"
        );
    }
}
