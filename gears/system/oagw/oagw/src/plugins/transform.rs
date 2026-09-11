//! The `request_id` transform (`cpt-cf-oagw-dod-plugin-request-id-transform`).
//!
//! Propagates the request correlation identifier: on `on_request` it
//! adopts an inbound `X-Request-ID` when the caller supplied one and
//! otherwise injects the correlation identifier
//! ([`crate::correlation::assign_correlation_id`]); on `on_response` it
//! sets the same value on the response. Declares no `on_error` phase.

use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::Response;

/// The header this transform reads and writes -- the same platform-standard
/// name [`crate::correlation::CORRELATION_ID_HEADER`] uses.
pub(crate) const REQUEST_ID_HEADER: &str = crate::correlation::CORRELATION_ID_HEADER;

/// `on_request`: adopt the inbound value when present, otherwise inject
/// `request_id` (`inst-transform-apply-04`/`-05`). Returns the value that
/// ends up on the outbound request, so the caller can hand the same value
/// to [`apply_on_response`] later in the same request.
// @cpt-algo:cpt-cf-oagw-algo-plugin-transform-apply:p2
// @cpt-dod:cpt-cf-oagw-dod-plugin-request-id-transform:p2
// @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-04
// @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-05
pub(crate) fn apply_on_request(headers: &mut HeaderMap, request_id: &str) -> String {
    if let Some(existing) = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
    {
        return existing.to_owned();
    }
    if let Ok(value) = HeaderValue::from_str(request_id) {
        headers.insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    request_id.to_owned()
}
// @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-05
// @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-04

/// `on_response`: set the same value used on the request
/// (`inst-transform-apply-06`/`-07`).
// @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-06
// @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-07
pub(crate) fn apply_on_response(response: &mut Response, request_id: &str) {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-07
// @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-06

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    #[test]
    fn adopts_inbound_request_id_when_present() {
        let mut headers = HeaderMap::new();
        headers.insert(REQUEST_ID_HEADER, "caller-supplied".parse().unwrap());
        let used = apply_on_request(&mut headers, "generated-id");
        assert_eq!(used, "caller-supplied");
        assert_eq!(headers.get(REQUEST_ID_HEADER).unwrap(), "caller-supplied");
    }

    #[test]
    fn injects_the_correlation_id_when_absent() {
        let mut headers = HeaderMap::new();
        let used = apply_on_request(&mut headers, "generated-id");
        assert_eq!(used, "generated-id");
        assert_eq!(headers.get(REQUEST_ID_HEADER).unwrap(), "generated-id");
    }

    #[test]
    fn on_response_sets_the_same_value_used_on_the_request() {
        let mut response = StatusCode::OK.into_response();
        apply_on_response(&mut response, "generated-id");
        assert_eq!(
            response.headers().get(REQUEST_ID_HEADER).unwrap(),
            "generated-id"
        );
    }
}
