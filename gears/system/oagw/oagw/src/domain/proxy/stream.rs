//! Server-sent-event pass-through streaming: event-stream response
//! detection and incremental, unbuffered forwarding
//! (`cpt-cf-oagw-algo-stream-detection`, `cpt-cf-oagw-algo-incremental-forwarding`,
//! `cpt-cf-oagw-dod-sse-detection`, `cpt-cf-oagw-dod-sse-incremental-forwarding`,
//! `cpt-cf-oagw-dod-sse-framing-preserved`).
//!
//! [`is_event_stream`] runs on the upstream response head alone, before any
//! body byte is read (`crate::domain::proxy::upstream::UpstreamHead`).
//! [`build_streaming_response`] then switches the client-facing response to
//! pass-through streaming instead of the buffered path: `reqwest`'s
//! `bytes_stream` becomes `axum::body::Body::from_stream` directly, so a
//! chunk reaches the caller as soon as it arrives, with no aggregation
//! window and no reframing.
//!
//! Lifecycle in both directions rests entirely on ownership, not on any
//! extra bookkeeping: dropping the returned response's body — which axum
//! does automatically when the caller disconnects — drops the wrapped
//! `reqwest::Response` and, with it, the upstream connection
//! (`cpt-cf-oagw-dod-sse-client-disconnect`); the upstream ending its stream
//! ends the iterator the wrapped body reads from, which axum turns into the
//! end of the caller's response body (`cpt-cf-oagw-dod-sse-upstream-close`).
//! A mid-stream transport failure surfaces as an `Err` item from
//! `bytes_stream`, which is logged and then ends the body with no problem
//! document, since the status and headers were already sent
//! (`cpt-cf-oagw-dod-sse-abort-handling`).
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};

use crate::domain::proxy::headers as header_xform;
use crate::domain::proxy::plugin_seam;
use crate::domain::proxy::upstream::UpstreamHead;
use crate::domain::resolve::ResolvedPlan;
use crate::error::OagwError;

/// The exact media type an upstream response must declare, ignoring any
/// `;`-separated parameters such as `charset`, to be classified as an event
/// stream (`cpt-cf-oagw-dod-sse-detection`).
const EVENT_STREAM_MEDIA_TYPE: &str = "text/event-stream";

/// Classifies an upstream response as an event stream from its
/// `Content-Type` header alone, before any body byte is read, ignoring
/// media-type parameters (`cpt-cf-oagw-algo-stream-detection`,
/// `cpt-cf-oagw-dod-sse-detection`).
// @cpt-begin:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-fn-01
#[must_use]
pub fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(media_type_of)
        .is_some_and(|media_type| media_type.eq_ignore_ascii_case(EVENT_STREAM_MEDIA_TYPE))
}

/// The media type alone from a `Content-Type` header value, discarding any
/// parameters (e.g. `text/event-stream; charset=utf-8` -> `text/event-stream`).
fn media_type_of(content_type: &str) -> &str {
    content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
}
// @cpt-end:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-fn-01

/// Builds the pass-through streaming client response for an upstream
/// response already classified as an event stream: forwards the status and
/// headers immediately, preserving the upstream `Content-Type` verbatim and
/// adding no `Content-Length`, then streams body chunks to the caller as
/// they arrive with no aggregation delay, reframing, or reordering
/// (`cpt-cf-oagw-algo-incremental-forwarding`, `cpt-cf-oagw-dod-sse-incremental-forwarding`,
/// `cpt-cf-oagw-dod-sse-framing-preserved`, `cpt-cf-oagw-dod-sse-upstream-close`,
/// `cpt-cf-oagw-dod-sse-client-disconnect`, `cpt-cf-oagw-dod-sse-abort-handling`).
// @cpt-begin:cpt-cf-oagw-dod-sse-incremental-forwarding:p1:inst-sse-stream-response-fn-01
// @cpt-begin:cpt-cf-oagw-dod-sse-framing-preserved:p1:inst-sse-stream-response-fn-01
// @cpt-begin:cpt-cf-oagw-dod-sse-upstream-close:p1:inst-sse-stream-response-fn-01
// @cpt-begin:cpt-cf-oagw-dod-sse-client-disconnect:p1:inst-sse-stream-response-fn-01
///
/// # Errors
///
/// Returns the mapped [`OagwError`] when a response-phase guard binding
/// rejects (`cpt-cf-oagw-dod-required-headers-guard`), before any body byte
/// is streamed to the caller; also returns [`OagwError::stream_aborted`]
/// (`502`, CONS-F-001) when the upstream stream's very first item fails —
/// still before any byte reached the caller, so the status can still change
/// to reflect the abort, unlike a later mid-stream failure.
pub async fn build_streaming_response(
    head: UpstreamHead,
    plan: &ResolvedPlan,
    origin: Option<&str>,
    chain_outcome: &plugin_seam::RequestChainOutcome,
) -> Result<Response, OagwError> {
    let status = head.response.status();
    let mut headers = head.response.headers().clone();
    // The upstream `Content-Type` is kept verbatim by never touching it here;
    // any pre-existing `Content-Length` is dropped since the forwarded body
    // is now a stream of unknown total length (`cpt-cf-oagw-dod-sse-framing-preserved`).
    headers.remove(CONTENT_LENGTH);
    header_xform::strip_hop_by_hop_response_headers(&mut headers);
    header_xform::apply_response_header_plan(&mut headers, &plan.header_plan.response);
    header_xform::apply_cors_response_headers(&mut headers, plan.effective_cors.as_ref(), origin);
    invoke_response_hook(plan, status, &mut headers, chain_outcome)?;

    let mut byte_stream = Box::pin(head.response.bytes_stream());
    // The response head above (status, headers) is only ever handed back to
    // the router once the first body item is known, so a failure surfacing
    // here — after the head arrived and was classified as an event stream,
    // but before any byte reached the caller — can still be reclassified as
    // the documented `StreamAborted` (CONS-F-001): distinct from a
    // pre-header connect/handshake failure (the response type isn't known
    // yet, so that case keeps the shared transport classification, e.g.
    // `DownstreamError`) and from a later mid-stream failure (the status has
    // already committed by then, so the body just ends, unchanged below).
    let first_chunk = read_first_chunk(&mut byte_stream).await?;

    // @cpt-begin:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-abort-midstream-01
    let rest = byte_stream.inspect_err(|error| {
        tracing::warn!(
            error = %error,
            "oagw: sse stream aborted after response headers were forwarded"
        );
    });
    // @cpt-end:cpt-cf-oagw-dod-sse-abort-handling:p1:inst-sse-abort-midstream-01
    let body_stream = futures_util::stream::iter(first_chunk.map(Ok)).chain(rest);
    let body = axum::body::Body::from_stream(body_stream);

    let mut response = (status, body).into_response();
    *response.headers_mut() = headers;
    crate::error::stamp_upstream_source(&mut response);
    Ok(response)
}

/// Reads the upstream byte stream's first item, returning `None` when the
/// stream was already exhausted (a legitimately empty body, not an abort).
///
/// # Errors
///
/// Returns [`OagwError::stream_aborted`] when the very first read errors
/// (CONS-F-001): no response byte has reached the caller yet, so the status
/// can still change.
async fn read_first_chunk(
    stream: &mut (impl futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin),
) -> Result<Option<Bytes>, OagwError> {
    match stream.next().await {
        None => Ok(None),
        Some(Ok(chunk)) => Ok(Some(chunk)),
        Some(Err(error)) => {
            tracing::warn!(
                error = %error,
                "oagw: sse stream aborted before any response byte was forwarded"
            );
            Err(OagwError::stream_aborted(format!(
                "upstream sse stream aborted before any byte was forwarded: {error}"
            )))
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-sse-client-disconnect:p1:inst-sse-stream-response-fn-01
// @cpt-end:cpt-cf-oagw-dod-sse-upstream-close:p1:inst-sse-stream-response-fn-01
// @cpt-end:cpt-cf-oagw-dod-sse-framing-preserved:p1:inst-sse-stream-response-fn-01
// @cpt-end:cpt-cf-oagw-dod-sse-incremental-forwarding:p1:inst-sse-stream-response-fn-01

/// Invokes the single response-phase plugin hook on the streamed response
/// head, exactly once, the same way the buffered path does
/// (`cpt-cf-oagw-dod-longlived-pipeline`).
fn invoke_response_hook(
    plan: &ResolvedPlan,
    status: StatusCode,
    headers: &mut HeaderMap,
    outcome: &plugin_seam::RequestChainOutcome,
) -> Result<(), OagwError> {
    let mut ctx = plugin_seam::ResponseHookContext {
        effective_plugins: &plan.effective_plugins,
        status,
        headers,
        outcome,
    };
    plugin_seam::invoke_response_hooks(&mut ctx)
}

#[cfg(test)]
mod tests {
    use super::is_event_stream;
    use axum::http::{HeaderMap, HeaderValue, header::CONTENT_TYPE};

    fn headers_with_content_type(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_str(value).expect("valid"));
        headers
    }

    // @cpt-begin:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-exact-test-01
    #[test]
    fn an_exact_event_stream_content_type_is_detected() {
        assert!(is_event_stream(&headers_with_content_type(
            "text/event-stream"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-exact-test-01

    // @cpt-begin:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-params-test-01
    #[test]
    fn a_charset_parameter_is_ignored_when_classifying() {
        assert!(is_event_stream(&headers_with_content_type(
            "text/event-stream; charset=utf-8"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-sse-detection:p1:inst-sse-detect-params-test-01

    #[test]
    fn detection_is_case_insensitive() {
        assert!(is_event_stream(&headers_with_content_type(
            "Text/Event-Stream"
        )));
    }

    #[test]
    fn an_unrelated_content_type_is_not_detected() {
        assert!(!is_event_stream(&headers_with_content_type(
            "application/json"
        )));
    }

    #[test]
    fn a_missing_content_type_is_not_detected() {
        assert!(!is_event_stream(&HeaderMap::new()));
    }
}
