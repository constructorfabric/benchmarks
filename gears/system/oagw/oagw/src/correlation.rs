//! Request correlation-ID assignment.
//!
//! Provides the mechanism later features attach their own per-request
//! values to; this feature only defines and provides the assignment
//! mechanism, not its per-request population on the proxy hot path (that is
//! `cpt-cf-oagw-feature-proxy-core`'s, 2.5, concern).
//!
//! See `docs/features/gear-foundation.md` §3 "Request Correlation-ID
//! Assignment" (`cpt-cf-oagw-algo-correlation-id`).

use axum::http::HeaderMap;
use uuid::Uuid;

/// The platform-standard correlation-id request header, consistent with the
/// api-gateway host's own `x-request-id` convention.
pub const CORRELATION_ID_HEADER: &str = "x-request-id";

/// Assign a correlation id to an inbound request: adopt an existing
/// platform-standard correlation-id header if present and non-empty,
/// otherwise generate a new identifier that is unique per invocation.
// @cpt-algo:cpt-cf-oagw-algo-correlation-id:p2
#[must_use]
pub fn assign_correlation_id(headers: &HeaderMap) -> String {
    // @cpt-begin:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-01
    // @cpt-begin:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-02
    let inbound = headers
        .get(CORRELATION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    // @cpt-end:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-02
    // @cpt-end:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-01

    // @cpt-begin:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-03
    // @cpt-begin:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-04
    // @cpt-begin:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-05
    inbound.unwrap_or_else(|| Uuid::new_v4().to_string())
    // @cpt-end:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-05
    // @cpt-end:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-04
    // @cpt-end:cpt-cf-oagw-algo-correlation-id:p2:inst-correlation-id-03
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn adopts_existing_inbound_correlation_id() {
        let mut headers = HeaderMap::new();
        headers.insert(CORRELATION_ID_HEADER, HeaderValue::from_static("req-123"));
        assert_eq!(assign_correlation_id(&headers), "req-123");
    }

    #[test]
    fn generates_non_empty_unique_id_when_absent() {
        let headers = HeaderMap::new();
        let a = assign_correlation_id(&headers);
        let b = assign_correlation_id(&headers);
        assert!(!a.is_empty());
        assert!(!b.is_empty());
        assert_ne!(a, b);
    }

    #[test]
    fn empty_inbound_header_is_treated_as_absent() {
        let mut headers = HeaderMap::new();
        headers.insert(CORRELATION_ID_HEADER, HeaderValue::from_static(""));
        assert!(!assign_correlation_id(&headers).is_empty());
    }
}
