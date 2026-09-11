//! Response classification and passthrough.
//!
//! The upstream response head is classified *before* its body is read, so the
//! decision the body handling follows is taken on the head alone: a
//! `text/event-stream` response or a negotiated upgrade is an open exchange for
//! the entry-2.6 handoff, and everything else is buffered passthrough.
//!
//! The error-source contract is decided here too: a passthrough response is the
//! upstream's answer whatever its status, so its source is `upstream`; a
//! response the gateway generated is `gateway`. The header itself is applied by
//! the entry-2.1 header layer, which never overwrites a value the producing
//! path already set.

use http::{HeaderMap, StatusCode};

use crate::infra::proxy::call::CallReply;
use crate::infra::proxy::context::ResponseContext;

/// The content type that marks a streamed body.
pub const EVENT_STREAM: &str = "text/event-stream";

/// The status a negotiated upgrade answers with.
pub const SWITCHING_PROTOCOLS: u16 = 101;

/// The error source a passthrough response carries.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// The error source a gateway-generated response carries.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// The classification of an upstream response head, taken before the body is
/// read.
#[derive(Debug)]
pub struct Classification {
    /// The response the pipeline reports.
    pub context: ResponseContext,
    /// The upstream headers to pass through, before the response-side
    /// transformation.
    pub headers: HeaderMap,
    /// The body the classification left unread.
    pub body: toolkit_http::ResponseBody,
}

// @cpt-begin:cpt-cf-oagw-dod-streaming-handoff:p1:inst-full
/// Classify a response head.
///
/// The body is not read: the classification is the decision the body handling
/// follows, and a streamed body must be handed over with its bytes still in the
/// upstream's pipe.
#[must_use]
pub fn classify(reply: CallReply) -> Classification {
    // The classification reads the head only: status, headers and the
    // negotiated version. A `text/event-stream` content type, a `101` status or
    // a non-empty `Upgrade` header marks an open exchange for entry 2.6.
    let streamed = reply.status.as_u16() == SWITCHING_PROTOCOLS
        || content_type_is_streamed(&reply.headers)
        || upgrade_is_present(&reply.headers);

    Classification {
        context: ResponseContext {
            status: reply.status.as_u16(),
            streamed,
            handed_off: false,
            http_version: reply.version,
            error_source: ERROR_SOURCE_UPSTREAM,
        },
        headers: reply.headers,
        body: reply.body,
    }
}
// @cpt-end:cpt-cf-oagw-dod-streaming-handoff:p1:inst-full

/// Whether the head declares a streamed content type.
fn content_type_is_streamed(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|content_type| {
            content_type
                .split(';')
                .next()
                .unwrap_or(content_type)
                .trim()
                .eq_ignore_ascii_case(EVENT_STREAM)
        })
}

/// Whether the head declares a protocol upgrade.
fn upgrade_is_present(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|upgrade| !upgrade.trim().is_empty())
}

/// The status a passthrough keeps unchanged.
///
/// Every status an upstream returns is passed through, including the error
/// statuses: the gateway adds no body of its own to an upstream answer.
#[must_use]
pub const fn passthrough_status(status: StatusCode) -> StatusCode {
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::proxy::call::HttpVersion;
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use http::{HeaderValue, Version};

    fn body() -> toolkit_http::ResponseBody {
        Full::new(Bytes::new())
            .map_err(|err: std::convert::Infallible| match err {})
            .boxed()
    }

    fn reply(status: u16, content_type: Option<&str>, upgrade: Option<&str>) -> CallReply {
        let mut headers = HeaderMap::new();
        if let Some(value) = content_type {
            headers.insert(
                http::header::CONTENT_TYPE,
                HeaderValue::from_str(value).expect("a value"),
            );
        }
        if let Some(value) = upgrade {
            headers.insert(
                http::header::UPGRADE,
                HeaderValue::from_str(value).expect("a value"),
            );
        }
        CallReply {
            status: StatusCode::from_u16(status).expect("a status"),
            headers,
            body: body(),
            version: HttpVersion::Http11,
        }
    }

    #[test]
    fn a_plain_response_is_a_buffered_passthrough() {
        let classified = classify(reply(200, Some("application/json"), None));
        assert!(!classified.context.streamed);
        assert!(!classified.context.handed_off);
        assert_eq!(classified.context.error_source, "upstream");
        assert_eq!(
            passthrough_status(StatusCode::from_u16(classified.context.status).expect("a status")),
            StatusCode::OK
        );
    }

    #[test]
    fn a_streamed_response_is_labelled_for_the_handoff() {
        let classified = classify(reply(200, Some("text/event-stream"), None));
        assert!(classified.context.streamed);
        assert!(!classified.context.handed_off, "the handoff sets that flag");
    }

    #[test]
    fn a_streamed_content_type_is_recognized_with_its_parameters() {
        assert!(
            classify(reply(200, Some("text/event-stream; charset=utf-8"), None))
                .context
                .streamed
        );
        assert!(classify(reply(200, Some("TEXT/EVENT-STREAM"), None)).context.streamed);
        assert!(!classify(reply(200, Some("text/html"), None)).context.streamed);
    }

    #[test]
    fn an_upgrade_response_is_labelled_for_the_handoff() {
        let classified = classify(reply(101, None, Some("websocket")));
        assert!(classified.context.streamed);
        assert_eq!(classified.context.status, SWITCHING_PROTOCOLS);
    }

    #[test]
    fn an_upstream_error_status_is_still_a_passthrough() {
        let classified = classify(reply(503, Some("application/json"), None));
        assert_eq!(classified.context.error_source, "upstream");
        assert!(!classified.context.streamed);
    }

    #[test]
    fn the_negotiated_version_is_recorded_from_the_response_head() {
        let mut classified = classify(reply(200, None, None));
        classified.context.http_version = HttpVersion::of(Version::HTTP_2);
        assert_eq!(classified.context.http_version, HttpVersion::Http2);
    }

    #[test]
    fn the_error_source_tokens_are_the_contract_tokens() {
        assert_eq!(ERROR_SOURCE_UPSTREAM, "upstream");
        assert_eq!(ERROR_SOURCE_GATEWAY, "gateway");
    }

    #[test]
    fn the_body_is_left_unread_by_the_classification() {
        // The classification holds the body it was given, untouched, so the
        // buffered path and the handoff path both start from the same stream.
        let classified = classify(reply(200, Some("text/event-stream"), None));
        assert!(
            classified.context.streamed,
            "the body handling follows the classification"
        );
    }

    #[test]
    fn a_head_without_a_content_type_is_a_buffered_passthrough() {
        let classified = classify(reply(204, None, None));
        assert!(!classified.context.streamed);
    }
}
