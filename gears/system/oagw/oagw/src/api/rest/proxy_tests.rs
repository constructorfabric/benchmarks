//! Preflight handling at the transport boundary.

use super::*;

fn preflight_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::ORIGIN, HeaderValue::from_static("https://app.example.com"));
    headers.insert(
        header::ACCESS_CONTROL_REQUEST_METHOD,
        HeaderValue::from_static("POST"),
    );
    headers.insert(
        header::ACCESS_CONTROL_REQUEST_HEADERS,
        HeaderValue::from_static("content-type, authorization"),
    );
    headers
}

#[test]
fn a_preflight_needs_options_plus_origin_plus_the_requested_method() {
    assert!(is_preflight(&Method::OPTIONS, &preflight_headers()));
    assert!(!is_preflight(&Method::GET, &preflight_headers()));

    let mut without_origin = preflight_headers();
    without_origin.remove(header::ORIGIN);
    assert!(!is_preflight(&Method::OPTIONS, &without_origin));

    let mut without_method = preflight_headers();
    without_method.remove(header::ACCESS_CONTROL_REQUEST_METHOD);
    assert!(!is_preflight(&Method::OPTIONS, &without_method));
}

#[test]
fn the_preflight_answer_echoes_the_request_and_varies_on_it() {
    let response = preflight_response(&preflight_headers());
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let headers = response.headers();
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "https://app.example.com"
    );
    assert_eq!(headers.get(header::ACCESS_CONTROL_ALLOW_METHODS).unwrap(), "POST");
    assert_eq!(
        headers.get(header::ACCESS_CONTROL_ALLOW_HEADERS).unwrap(),
        "content-type, authorization"
    );
    assert_eq!(headers.get(header::ACCESS_CONTROL_MAX_AGE).unwrap(), "86400");
    assert_eq!(
        headers.get(header::VARY).unwrap(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers"
    );
    // The preflight is answered by the gateway itself, never by an upstream.
    assert_eq!(headers.get("x-oagw-error-source").unwrap(), "gateway");
}
