//! Transport extraction for the OAGW REST layer.
//!
//! Axum renders `Path`/`Query` extractor rejections as `(status, &str)` — a
//! plain-text body with no `Content-Type: application/problem+json` and no
//! `X-OAGW-Error-Source` (DESIGN §2.1, ADR-0007). Handlers therefore extract
//! the raw [`Path`](axum::extract::Path), [`Query`](axum::extract::Query) and
//! [`Request`](axum::extract::Request) — reading the body themselves through
//! [`buffer_body`] — and funnel every failure through
//! [`crate::error::OagwError`], so all gateway errors share one wire surface.

use axum::body::{Bytes, to_bytes};
use axum::extract::{DefaultBodyLimit, Request};
use serde_json::Value;
use uuid::Uuid;

use crate::error::OagwError;

/// Hard limit on a management-API request body (DESIGN §2.2
/// `cpt-cf-oagw-constraint-body-limit`).
pub const MAX_BODY_BYTES: usize = 100 * 1024 * 1024;

/// The message `http_body_util::LengthLimitError` renders when a body outgrows
/// the limit it is read under.
const LENGTH_LIMIT_EXCEEDED: &str = "length limit exceeded";

/// Reject a request body above [`MAX_BODY_BYTES`] before it is parsed.
///
/// # Errors
/// [`crate::error::OagwErrorKind::PayloadTooLarge`] (413) when the body exceeds
/// the hard limit.
pub fn check_body_limit(bytes: &[u8]) -> Result<(), OagwError> {
    check_body_limit_with(bytes, MAX_BODY_BYTES)
}

/// [`check_body_limit`] against an explicit limit.
///
/// Visible to the handlers, which test the 413 path with a small limit instead
/// of allocating the 100 MB hard limit.
pub(crate) fn check_body_limit_with(bytes: &[u8], limit: usize) -> Result<(), OagwError> {
    enforce_body_limit(bytes.len(), limit)
}

/// Body-limit policy, split from [`check_body_limit`] so tests can exercise the
/// 413 path with a small limit instead of allocating 100 MB.
fn enforce_body_limit(len: usize, limit: usize) -> Result<(), OagwError> {
    if len <= limit {
        return Ok(());
    }

    Err(OagwError::payload_too_large(format!(
        "the request body is {len} bytes, above the {limit}-byte limit"
    )))
}

/// Buffer a management request body up to [`MAX_BODY_BYTES`].
///
/// The body is read by the handler — not by an extractor — because the extractors
/// that buffer (`Bytes`, and `Json` above it) render an over-limit body as a
/// plain-text 413 with no `X-OAGW-Error-Source`, and because axum's own default
/// limit (2 MB) would otherwise reject a body the DESIGN allows before the
/// handler ever runs. The limit is therefore stated twice on purpose: as the
/// router-level `DefaultBodyLimit` (see `register_routes`) and on this read, so
/// neither can silently drift from [`MAX_BODY_BYTES`].
///
/// # Errors
/// [`crate::error::OagwErrorKind::PayloadTooLarge`] when the body is above the
/// limit, [`crate::error::OagwErrorKind::ValidationError`] when the body cannot
/// be read at all (a truncated or reset connection is a client problem, not a
/// gateway failure).
pub(crate) async fn buffer_body(request: &mut Request) -> Result<Bytes, OagwError> {
    DefaultBodyLimit::max(MAX_BODY_BYTES).apply(request);

    // The body is taken out so the request — its headers in particular — stays
    // readable by the caller.
    let body = std::mem::take(request.body_mut());
    to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|error| body_read_error(&error))
}

/// Map a body-read failure onto the OAGW error surface (413 or 400).
fn body_read_error(error: &axum::Error) -> OagwError {
    if is_length_limit(error) {
        OagwError::payload_too_large(format!(
            "the request body is above the {MAX_BODY_BYTES}-byte limit"
        ))
    } else {
        OagwError::validation(format!("the request body could not be read: {error}"))
    }
}

/// `true` when the error chain reports a body that outgrew its limit.
///
/// `to_bytes` signals the overflow with `http_body_util::LengthLimitError`,
/// which this crate cannot name (http-body-util is a dev-dependency only), so
/// the chain is matched on that error's documented message — the same predicate
/// axum applies with a downcast in its `multipart` extractor.
fn is_length_limit(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut candidate = Some(error);
    while let Some(current) = candidate {
        if current.to_string() == LENGTH_LIMIT_EXCEEDED {
            return true;
        }
        candidate = current.source();
    }

    false
}

/// Parse a resource path parameter.
///
/// Two spellings are accepted (DESIGN §3.3 / §3.6 "Resource Identification
/// Pattern"): the bare UUID and the anonymous GTS identifier
/// `gts.cf.core.oagw.<type>.v1~{uuid}`. A leading `<base-type>~` prefix is
/// stripped when present and the remainder must be a UUID; response bodies
/// always carry the bare UUID (`docs/schemas/*.schema.json` say `format: uuid`).
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] (400) — never a 500 — when
/// the parameter names no resource id.
pub fn resource_id(parameter: &'static str, raw: &str) -> Result<Uuid, OagwError> {
    let candidate = raw.split_once('~').map_or(raw, |(_, remainder)| remainder);

    Uuid::parse_str(candidate).map_err(|_| malformed_id(parameter, raw))
}

/// The 400 problem document for an unparseable resource identifier.
fn malformed_id(parameter: &'static str, raw: &str) -> OagwError {
    OagwError::validation(format!(
        "the path parameter `{parameter}` must be a UUID or an anonymous GTS \
         identifier (`<base-type>~<uuid>`), got '{raw}'"
    ))
    .with_extension("field", Value::String(parameter.to_owned()))
}

/// Map an axum `Path` rejection onto the OAGW problem surface (400).
///
/// Rejections are mapped rather than rendered by axum so the response is
/// `application/problem+json` and carries `X-OAGW-Error-Source: gateway`.
///
/// # Errors
/// Always: [`crate::error::OagwErrorKind::ValidationError`].
pub fn path_rejection(parameter: &'static str, rejection: impl std::fmt::Display) -> OagwError {
    OagwError::validation(format!(
        "the path parameter `{parameter}` could not be read: {rejection}"
    ))
    .with_extension("field", Value::String(parameter.to_owned()))
}

/// Map an axum `Query` rejection onto the OAGW problem surface (400).
///
/// # Errors
/// Always: [`crate::error::OagwErrorKind::ValidationError`].
pub fn query_rejection(rejection: impl std::fmt::Display) -> OagwError {
    OagwError::validation(format!("the query string could not be read: {rejection}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::OagwErrorKind;
    use axum::body::Body;
    use futures_util::stream;

    #[test]
    fn max_body_bytes_is_the_documented_100mb_limit() {
        assert_eq!(MAX_BODY_BYTES, 100 * 1024 * 1024);
        assert_eq!(MAX_BODY_BYTES, 104_857_600);
    }

    #[test]
    fn body_limit_accepts_bodies_at_the_limit() {
        assert!(enforce_body_limit(0, MAX_BODY_BYTES).is_ok());
        assert!(enforce_body_limit(MAX_BODY_BYTES, MAX_BODY_BYTES).is_ok());
    }

    #[test]
    fn body_limit_rejects_oversized_bodies_with_a_413_problem() {
        let err =
            enforce_body_limit(MAX_BODY_BYTES + 1, MAX_BODY_BYTES).expect_err("over the limit");

        assert_eq!(err.status().as_u16(), 413);
        assert_eq!(err.kind(), OagwErrorKind::PayloadTooLarge);
        assert!(err.detail().contains("104857601"));

        let response = err.to_response();
        assert_eq!(response.status().as_u16(), 413);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        assert_eq!(
            content_type.as_deref(),
            Some(crate::error::PROBLEM_JSON_MEDIA_TYPE)
        );
    }

    #[tokio::test]
    async fn buffer_body_returns_a_body_under_the_limit() {
        let mut request = Request::builder()
            .uri("/oagw/v1/plugins")
            .body(Body::from("{\"name\":\"validator\"}"))
            .expect("request builds");

        let bytes = buffer_body(&mut request).await.expect("body buffers");

        assert_eq!(bytes, Bytes::from_static(b"{\"name\":\"validator\"}"));
        assert_eq!(
            request.headers().get("content-type"),
            None,
            "the request stays readable once the body is taken"
        );
    }

    #[tokio::test]
    async fn a_length_limit_failure_maps_to_a_413_problem() {
        // `to_bytes` under a limit far below the hard one, so the mapping is
        // exercised without allocating 100 MB: the failure is the same
        // `LengthLimitError` an over-limit management body produces.
        let error = to_bytes(Body::from("ab"), 1)
            .await
            .expect_err("over the limit");
        let err = body_read_error(&error);

        assert_eq!(err.status().as_u16(), 413);
        assert_eq!(err.kind(), OagwErrorKind::PayloadTooLarge);
        assert!(err.detail().contains("limit"), "{}", err.detail());
    }

    #[tokio::test]
    async fn a_body_that_cannot_be_read_is_not_a_413() {
        // A body whose stream fails (a reset connection) is a client problem,
        // not an over-limit one: it must not masquerade as a payload-too-large.
        let error = to_bytes(
            Body::from_stream(stream::once(async {
                Err::<Bytes, _>(std::io::Error::other("connection reset"))
            })),
            MAX_BODY_BYTES,
        )
        .await
        .expect_err("the stream fails");
        let err = body_read_error(&error);

        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert!(
            err.detail().contains("connection reset"),
            "{}",
            err.detail()
        );
    }

    #[test]
    fn resource_id_accepts_a_bare_uuid() {
        let id = Uuid::new_v4();

        assert_eq!(resource_id("id", &id.to_string()).expect("bare uuid"), id);
    }

    #[test]
    fn resource_id_accepts_the_anonymous_gts_identifier() {
        let id = Uuid::new_v4();
        let raw = format!("gts.cf.core.oagw.upstream.v1~{id}");

        assert_eq!(
            resource_id("id", &raw).expect("gts identifier"),
            id,
            "the base-type prefix is stripped"
        );
        assert_ne!(Uuid::nil(), id);
    }

    #[test]
    fn resource_id_rejects_malformed_ids_with_a_400() {
        for raw in [
            "not-a-uuid",
            "gts.cf.core.oagw.upstream.v1~not-a-uuid",
            "gts.cf.core.oagw.upstream.v1~",
            "~",
            "00000000-0000-0000-0000-00000000000g",
        ] {
            let err = resource_id("id", raw).expect_err(raw);
            assert_eq!(err.status().as_u16(), 400, "{raw} is a 400, not a 500");
            assert_eq!(err.kind(), OagwErrorKind::ValidationError, "{raw}");
            assert!(err.detail().contains(raw), "{raw} is named in the detail");
            assert_eq!(
                err.extensions().get("field").and_then(Value::as_str),
                Some("id")
            );
        }
    }

    #[test]
    fn rejections_map_onto_the_validation_error_surface() {
        let err = path_rejection("id", "missing path parameter");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert!(err.detail().contains("`id`"));
        assert_eq!(
            err.extensions().get("field").and_then(Value::as_str),
            Some("id")
        );

        let err = query_rejection("Failed to deserialize query string: $top: invalid digit");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(err.kind(), OagwErrorKind::ValidationError);
        assert!(err.detail().contains("$top"));
    }
}
