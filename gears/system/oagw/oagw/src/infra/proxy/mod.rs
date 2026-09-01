//! Data-plane infrastructure: HTTP forwarding, WebSocket bridging, rate
//! limiting and CORS enforcement.

pub mod cors;
pub mod forwarder;
pub mod rate_limiter;
pub mod websocket;

/// True when an `axum::body::to_bytes` error is a body-length-limit overflow.
///
/// axum 0.8's boxed error exposes no `is_overflow` accessor; the limit is
/// enforced inside by `http_body_util::LengthLimitError`, which appears as the
/// error's direct source. We match on its type-name / message rather than
/// depending on that crate directly.
pub fn is_body_overflow(err: &(dyn std::error::Error + 'static)) -> bool {
    let Some(source) = err.source() else {
        return false;
    };
    std::any::type_name_of_val(source).contains("LengthLimitError")
        || source.to_string().contains("length limit")
}
