//! Cross-cutting error-source stamping and problem-body correlation (ADR-0007).
//!
//! Every response the gear emits must carry `X-OAGW-Error-Source`, so that
//! clients can tell a gateway-side failure (`gateway`) from a failure the
//! upstream itself returned (`upstream`, stamped by the proxy engine in
//! slice 4). The middleware is additive: it only fills the header in when the
//! inner service has not already set it.
//!
//! The same layer fills the two correlation fields the ADR-0007 problem
//! examples carry but the taxonomy itself cannot know: `trace_id` (from the
//! request) and `instance` (the request path, when the handler left it unset).
//! It is the last thing that touches a problem response before it leaves the
//! gear, and it is deliberately fail-safe: any parsing surprise returns the
//! inner response byte-for-byte.
//!
//! ## Upstream problem documents are passthrough
//!
//! An upstream is free to answer `application/problem+json` itself, and the
//! proxy stamps that answer `X-OAGW-Error-Source: upstream` before it reaches
//! this layer. Such a body is passthrough (ADR-0007): the gateway does not own
//! it, so injecting its own `trace_id`/`instance` would misattribute the
//! failure and rewriting it would cost a full buffer of an arbitrarily large
//! upstream payload. The correlation step therefore runs for gateway-produced
//! problem documents only.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::domain::error::{
    APPLICATION_PROBLEM_JSON, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM,
    ProblemBody,
};

/// Largest problem body the layer rewrites.
///
/// Problem documents are gear-generated and therefore tiny; the only field a
/// caller can inflate is a validation `detail`, whose inputs are bounded by the
/// management body cap (1 MiB). Two MiB therefore never truncates a real body,
/// and a body that still will not fit is handed through untouched rather than
/// parsed.
const MAX_PROBLEM_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Request facts the correlation step needs after the handler has run.
#[derive(Debug, Clone)]
struct RequestTrace {
    /// `traceparent` / `x-request-id` / `x-trace-id` correlation id, `None` when
    /// the request carries none (the field stays absent rather than invented).
    trace_id: Option<String>,
    /// Path of the request, for a problem `instance` the handler left unset.
    path: String,
}

impl RequestTrace {
    fn from_parts(headers: &HeaderMap, path: &str) -> Self {
        Self {
            trace_id: trace_id_of(headers),
            path: path.to_owned(),
        }
    }
}

/// Correlation id of a request: `traceparent` (W3C trace-id segment), then
/// `x-request-id`, then `x-trace-id` — first present wins, none is generated.
fn trace_id_of(headers: &HeaderMap) -> Option<String> {
    if let Some(traceparent) = headers
        .get("traceparent")
        .and_then(|value| value.to_str().ok())
        .and_then(w3c_trace_id)
    {
        return Some(traceparent);
    }
    for name in ["x-request-id", "x-trace-id"] {
        if let Some(value) = headers.get(name).and_then(|value| value.to_str().ok()) {
            return Some(value.to_owned());
        }
    }
    None
}

/// Extracts the 32-hex trace-id segment of a W3C `traceparent` header.
///
/// `00-<trace-id>-<span-id>-<flags>`: a malformed header falls through to the
/// `x-request-id` / `x-trace-id` fallbacks instead of poisoning the field.
fn w3c_trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() >= 4 && parts[0] == "00" {
        Some(parts[1].to_owned())
    } else {
        None
    }
}

/// Stamps `X-OAGW-Error-Source: gateway` on any response that does not
/// already carry the header, and correlates problem bodies with the request.
pub async fn error_source_layer(request: Request, next: Next) -> Response {
    let trace = RequestTrace::from_parts(request.headers(), request.uri().path());
    let mut response = next.run(request).await;
    {
        let headers = response.headers_mut();
        if !headers.contains_key(ERROR_SOURCE_HEADER) {
            headers.insert(
                HeaderName::from_static(ERROR_SOURCE_HEADER),
                HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
            );
        }
    }
    if is_problem_response(&response) && !is_upstream_response(&response) {
        response = correlate(response, &trace).await;
    }
    response
}

/// `true` when the response body is the upstream's own, which the data plane
/// stamped before this layer ran.
///
/// The check reads the stamp the handler already set, so it stays correct even
/// if a future change moves the stamping into the engine after the layer: a
/// response without the header (or stamped `gateway`) is the gateway's own and
/// keeps being correlated.
fn is_upstream_response(response: &Response) -> bool {
    response
        .headers()
        .get(ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|source| source == ERROR_SOURCE_UPSTREAM)
}

/// `true` when `response` is an RFC 9457 problem document.
fn is_problem_response(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|media_type| media_type.starts_with(APPLICATION_PROBLEM_JSON))
}

/// Rewrites the problem body of `response` with the request's `trace_id` and,
/// when absent, its `instance` as the `instance`.
///
/// Fail-safe by construction: a body that cannot be buffered, parsed or
/// re-serialised is returned unchanged.
async fn correlate(response: Response, trace: &RequestTrace) -> Response {
    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_PROBLEM_BODY_BYTES).await {
        Ok(bytes) => bytes,
        // Unreachable for a gear-generated problem document (see
        // [`MAX_PROBLEM_BODY_BYTES`]): the original body is gone, so the
        // response is completed with the headers it already carried.
        Err(error) => {
            tracing::warn!(error = %error, "problem body could not be buffered");
            return Response::from_parts(parts, Body::empty());
        }
    };
    let mut problem: ProblemBody = match serde_json::from_slice(&bytes) {
        Ok(problem) => problem,
        Err(error) => {
            tracing::warn!(error = %error, "problem body is not a known problem document");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };
    if problem.trace_id.is_none() {
        problem.trace_id = trace.trace_id.clone();
    }
    if problem.instance.is_none() {
        problem.instance = Some(trace.path.clone());
    }
    problem.apply_context_extensions();
    let rewritten_body = match serde_json::to_vec(&problem) {
        Ok(rewritten) => rewritten,
        Err(error) => {
            tracing::warn!(error = %error, "problem body could not be re-serialised");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };
    let mut rewritten = Response::from_parts(parts, Body::from(rewritten_body.clone()));
    rewritten.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(rewritten_body.len()),
    );
    rewritten
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test-only assertions"
    )]

    use super::error_source_layer;
    use crate::domain::error::{
        APPLICATION_PROBLEM_JSON, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM,
        OagwError,
    };
    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request as HttpRequest, StatusCode, header};
    use axum::middleware::from_fn;
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use tower::ServiceExt;

    /// A problem document the upstream answered itself: a complete one, so it
    /// *would* parse, but with no correlation fields — the gateway must not
    /// invent any.
    const UPSTREAM_PROBLEM_BODY: &str = concat!(
        r#"{"type":"https://errors.example.com/outage","title":"Upstream Outage","#,
        r#""status":503,"detail":"the upstream is over capacity"}"#
    );

    async fn ok_handler() -> &'static str {
        std::future::ready("ok").await
    }

    async fn already_stamped() -> Response {
        let mut response = Response::new(Body::empty());
        response.headers_mut().insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        std::future::ready(response).await
    }

    async fn failing() -> OagwError {
        std::future::ready(OagwError::not_found("no such upstream")).await
    }

    async fn alias_failure() -> OagwError {
        std::future::ready(
            OagwError::unknown_target_host("host is not part of the upstream pool")
                .with_alias("payments")
                .with_upstream_id(uuid::Uuid::from_u128(0xA1)),
        )
        .await
    }

    async fn plain_failure() -> Response {
        (StatusCode::BAD_REQUEST, "not json").into_response()
    }

    /// The problem document an upstream answered itself, stamped `upstream` by
    /// the data plane before this layer ran.
    async fn upstream_problem() -> Response {
        let response = (
            StatusCode::SERVICE_UNAVAILABLE,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static(APPLICATION_PROBLEM_JSON),
            )],
            UPSTREAM_PROBLEM_BODY,
        )
            .into_response();
        let mut stamped = response;
        stamped.headers_mut().insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        std::future::ready(stamped).await
    }
    fn source_of(response: &Response) -> Option<String> {
        response
            .headers()
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    }

    fn into_owned(response: axum::http::Response<Body>) -> Response {
        response.into_response()
    }

    async fn json_of(response: Response) -> serde_json::Value {
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("problem json")
    }

    async fn send(app: Router, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut builder = HttpRequest::builder().method("GET").uri(path);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(Body::empty()).expect("request");
        into_owned(app.oneshot(request).await.expect("response"))
    }

    fn app() -> Router {
        Router::new()
            .route("/plain", get(ok_handler))
            .route("/stamped", get(already_stamped))
            .route("/error", get(failing))
            .route("/alias", get(alias_failure))
            .route("/plain-error", get(plain_failure))
            .route("/upstream-problem", get(upstream_problem))
            .layer(from_fn(error_source_layer))
    }

    #[tokio::test]
    async fn stamps_gateway_on_untagged_and_error_responses() {
        let app = app();

        let plain = send(app.clone(), "/plain", &[]).await;
        assert_eq!(plain.status(), StatusCode::OK);
        assert_eq!(source_of(&plain).as_deref(), Some(ERROR_SOURCE_GATEWAY));

        let stamped = send(app.clone(), "/stamped", &[]).await;
        assert_eq!(source_of(&stamped).as_deref(), Some(ERROR_SOURCE_UPSTREAM));

        let failed = send(app, "/error", &[]).await;
        assert_eq!(failed.status(), StatusCode::NOT_FOUND);
        assert_eq!(source_of(&failed).as_deref(), Some(ERROR_SOURCE_GATEWAY));
    }

    #[tokio::test]
    async fn a_problem_body_carries_the_trace_id_of_the_request() {
        let from_traceparent = send(
            app(),
            "/error",
            &[(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )],
        )
        .await;
        let body = json_of(from_traceparent).await;
        assert_eq!(
            body["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736",
            "{body}"
        );
        assert_eq!(
            body["instance"], "/error",
            "instance falls back to the path"
        );

        let from_request_id = send(app(), "/error", &[("x-request-id", "req-9")]).await;
        assert_eq!(json_of(from_request_id).await["trace_id"], "req-9");

        let from_trace_header = send(app(), "/error", &[("x-trace-id", "trace-9")]).await;
        assert_eq!(json_of(from_trace_header).await["trace_id"], "trace-9");

        let unmatched = send(app(), "/error", &[]).await;
        assert!(
            json_of(unmatched).await.get("trace_id").is_none(),
            "no id is invented"
        );
    }

    #[tokio::test]
    async fn traceparent_wins_over_the_other_headers() {
        let response = send(
            app(),
            "/error",
            &[
                (
                    "traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                ),
                ("x-request-id", "req-1"),
            ],
        )
        .await;
        assert_eq!(
            json_of(response).await["trace_id"],
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
    }

    #[tokio::test]
    async fn a_malformed_traceparent_falls_through_to_the_request_id() {
        let response = send(
            app(),
            "/error",
            &[
                ("traceparent", "not-a-traceparent"),
                ("x-request-id", "req-2"),
            ],
        )
        .await;
        assert_eq!(json_of(response).await["trace_id"], "req-2");
    }

    #[tokio::test]
    async fn an_existing_instance_is_kept() {
        let response = send(app(), "/alias", &[]).await;
        let body = json_of(response).await;
        assert_eq!(body["instance"], "/alias", "the handler set no instance");
    }

    #[tokio::test]
    async fn context_extension_fields_are_mirrored_at_the_top_level() {
        let response = send(app(), "/alias", &[("x-request-id", "req-3")]).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(APPLICATION_PROBLEM_JSON)
        );
        let body = json_of(response).await;
        assert_eq!(body["alias"], "payments", "{body}");
        assert_eq!(
            body["upstream_id"],
            uuid::Uuid::from_u128(0xA1).to_string(),
            "{body}"
        );
        assert_eq!(body["context"]["alias"], "payments", "{body}");
        assert_eq!(body["trace_id"], "req-3", "{body}");
    }

    #[tokio::test]
    async fn a_non_problem_failure_passes_through_untouched() {
        let response = send(app(), "/plain-error", &[("x-request-id", "req-4")]).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(&bytes[..], b"not json");
    }

    #[tokio::test]
    async fn an_upstream_problem_document_passes_through_byte_for_byte() {
        let response = send(app(), "/upstream-problem", &[("x-request-id", "req-5")]).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            source_of(&response).as_deref(),
            Some(ERROR_SOURCE_UPSTREAM),
            "the upstream keeps its own stamp"
        );
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        assert_eq!(
            &bytes[..],
            UPSTREAM_PROBLEM_BODY.as_bytes(),
            "the gateway rewrites nothing: not its document, not its correlation ids"
        );
    }
}
