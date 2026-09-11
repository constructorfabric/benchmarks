//! The `request_id` transform plugin (`cpt-cf-oagw-algo-request-id-propagation`,
//! `cpt-cf-oagw-dod-request-id-transform`).

use axum::http::{HeaderMap, HeaderValue};
use uuid::Uuid;

/// The header name this plugin guarantees on both the forwarded request and
/// the returned response.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Ensures `X-Request-ID` on the forwarded request: keeps a non-blank
/// inbound value unchanged, or generates a fresh unique value
/// (`cpt-cf-oagw-algo-request-id-propagation`). Returns the chosen value so
/// the response phase can propagate the same one.
// @cpt-begin:cpt-cf-oagw-algo-request-id-propagation:p2:inst-request-id-request-fn-01
#[must_use]
pub fn ensure_on_request(headers: &mut HeaderMap) -> String {
    if let Some(existing) = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
    {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let generated = Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&generated) {
        headers.insert(REQUEST_ID_HEADER, value);
    }
    generated
}
// @cpt-end:cpt-cf-oagw-algo-request-id-propagation:p2:inst-request-id-request-fn-01

/// Sets `X-Request-ID` on the returned response to `request_id` only when
/// the upstream response omitted it (`cpt-cf-oagw-algo-request-id-propagation`).
// @cpt-begin:cpt-cf-oagw-algo-request-id-propagation:p2:inst-request-id-response-fn-01
pub fn ensure_on_response(headers: &mut HeaderMap, request_id: &str) {
    if !headers.contains_key(REQUEST_ID_HEADER)
        && let Ok(value) = HeaderValue::from_str(request_id)
    {
        headers.insert(REQUEST_ID_HEADER, value);
    }
}
// @cpt-end:cpt-cf-oagw-algo-request-id-propagation:p2:inst-request-id-response-fn-01

#[cfg(test)]
mod tests {
    use super::{ensure_on_request, ensure_on_response};
    use axum::http::HeaderMap;

    // @cpt-begin:cpt-cf-oagw-dod-request-id-transform:p2:inst-request-id-generate-test-01
    #[test]
    fn a_missing_inbound_id_is_generated_and_propagated() {
        let mut request_headers = HeaderMap::new();
        let chosen = ensure_on_request(&mut request_headers);
        assert!(!chosen.is_empty());
        assert_eq!(
            request_headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(chosen.as_str())
        );

        let mut response_headers = HeaderMap::new();
        ensure_on_response(&mut response_headers, &chosen);
        assert_eq!(
            response_headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some(chosen.as_str())
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-request-id-transform:p2:inst-request-id-generate-test-01

    // @cpt-begin:cpt-cf-oagw-dod-request-id-transform:p2:inst-request-id-preserve-test-01
    #[test]
    fn an_inbound_id_is_kept_unchanged() {
        let mut request_headers = HeaderMap::new();
        request_headers.insert("x-request-id", "abc-123".parse().unwrap());
        let chosen = ensure_on_request(&mut request_headers);
        assert_eq!(chosen, "abc-123");
    }
    // @cpt-end:cpt-cf-oagw-dod-request-id-transform:p2:inst-request-id-preserve-test-01

    #[test]
    fn a_response_that_already_carries_the_header_is_left_unchanged() {
        let mut response_headers = HeaderMap::new();
        response_headers.insert("x-request-id", "upstream-value".parse().unwrap());
        ensure_on_response(&mut response_headers, "gateway-value");
        assert_eq!(
            response_headers
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("upstream-value")
        );
    }

    #[test]
    fn a_blank_inbound_id_is_treated_as_absent() {
        let mut request_headers = HeaderMap::new();
        request_headers.insert("x-request-id", "   ".parse().unwrap());
        let chosen = ensure_on_request(&mut request_headers);
        assert_ne!(chosen, "   ");
        assert!(!chosen.trim().is_empty());
    }
}
