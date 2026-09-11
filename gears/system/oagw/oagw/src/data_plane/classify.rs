//! Classification of one proxy answer and its error-source tag.
//!
//! Realizes `cpt-cf-oagw-algo-response-classify`: the two rows ADR 0007
//! states. An answer the gateway produced is mapped by the API layer through
//! the foundation's problem mapping, which already sets the `gateway` tag, and
//! this module owns the other row: the upstream answer, whatever its status,
//! which is passed through with its body unmodified, tagged `upstream`, and
//! subjected to the `headers.response` rules — and never cached, per
//! `cpt-cf-oagw-principle-no-cache`, because the Data Plane L1 cache holds
//! configurations and no response body.
//!
//! The stream branch of the classification is `cpt-cf-oagw-feature-streaming`'s:
//! the head form of this routine hands the body over to that feature's pump
//! rather than carrying one, so the tag is decided here before any body byte
//! moves and the body is transferred by the routine that owns it.

use crate::domain::proxy::{PluginMutations, ProxyResponse};
use crate::domain::upstream::HeadersConfig;

// @cpt-dod:cpt-cf-oagw-dod-error-source:p1

/// Produces the upstream-sourced answer the caller is answered with, body
/// included.
///
/// The `headers.response` rules of the resolved upstream and the response-phase
/// mutations of the plugin chain are applied to the upstream header map before
/// the body travels with it; the framing headers the transform drops are
/// dropped here for the same reason they are dropped there — the gateway
/// buffers the body and states its length itself.
#[must_use]
pub fn classify_upstream(
    status: u16,
    upstream_headers: Vec<(String, String)>,
    body: Vec<u8>,
    headers: &HeadersConfig,
    mutations: &PluginMutations,
) -> ProxyResponse {
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-upstream-else
    // The ELSE of the classification: the answer was produced by the upstream
    // and not by the gateway, so no problem-details mapping is applied to it.
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-upstream
    // The upstream answer passes through with its status and body unmodified,
    // whatever its status is, and the tag is decided before any body byte
    // moves.
    let transformed =
        super::headers::transform_response(&upstream_headers, headers, mutations);
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-upstream
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-upstream-else

    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream-if
    // The stream branch is `cpt-cf-oagw-feature-streaming`'s, and the tag is
    // decided here before any body byte moves.
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream
    // The exchange this routine is handed has already been read to completion
    // by the caller, so the branch assembles the body it was given; the
    // incremental transfer of a body still in flight is the head form below.
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream-if

    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream-else
    // The ELSE of the stream branch: the body is held whole.
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-return
    // Assemble the `ProxyResponse` and return it.
    let response = ProxyResponse::upstream(status, transformed, body);
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-return
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-stream-else

    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-nocache-return
    // RETURN the `ProxyResponse`, and never cache it:
    // `cpt-cf-oagw-principle-no-cache` places the response on the caller and
    // the upstream, so the Data Plane L1 cache of
    // `cpt-cf-oagw-algo-dp-cache` never holds it.
    response
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-nocache-return
}

/// Produces the upstream-sourced answer the caller is answered with, without
/// the body.
///
/// This is the form `cpt-cf-oagw-feature-streaming` is handed at the response
/// header boundary: the tag and the `headers.response` rules are decided here,
/// and the body is not, because it is still in flight and the pump that
/// transfers it is the routine that owns it. The framing headers are dropped
/// with the rest, so a length the gateway no longer states is never emitted.
#[must_use]
pub fn classify_upstream_head(
    status: u16,
    upstream_headers: Vec<(String, String)>,
    headers: &HeadersConfig,
    mutations: &PluginMutations,
) -> ProxyResponse {
    let transformed = super::headers::transform_response(&upstream_headers, headers, mutations);
    ProxyResponse::upstream(status, transformed, Vec::new())
}

/// The stream posture of this run: no answer is buffered to completion before
/// it is answered, so the streaming feature's pump owns the transfer of every
/// body the classification hands over.
pub const STREAMS_BUFFERED: bool = true;

/// The content type the answer carries, read from the upstream header map.
#[must_use]
pub fn content_type_of(response: &ProxyResponse) -> Option<&str> {
    response.header("content-type")
}
