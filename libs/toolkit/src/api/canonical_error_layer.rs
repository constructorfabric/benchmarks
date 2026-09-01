//! Canonical error middleware (DESIGN.md §3.2 / §3.6 / §3.7).
//!
//! Post-processes responses with `Content-Type: application/problem+json`,
//! filling missing `trace_id` (W3C `traceparent` → `x-trace-id` →
//! `x-request-id` → span-id fallback) and `instance` (request URI path).
//! Logs at `warn!` for 4xx / `error!` for 5xx with structured fields.
//!
//! Catch-all behaviour (panics, unknown error types) is out of scope per
//! PRD §4.2 — `CatchPanicLayer` handles panics; handlers are responsible
//! for typing their errors as `CanonicalError`.

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderMap, HeaderValue, header},
    middleware::Next,
    response::Response,
};
use toolkit_canonical_errors::{CanonicalError, Problem};

const PROBLEM_JSON: &str = "application/problem+json";

/// Tower middleware function that fills `trace_id` / `instance` on canonical
/// Problem responses and logs at `warn!` (4xx) / `error!` (5xx).
///
/// Non-problem responses pass through unchanged. Malformed Problem bodies
/// are logged at `error!` and returned to the client as-is.
pub async fn canonical_error_middleware(request: Request, next: Next) -> Response {
    let uri_path = request.uri().path().to_owned();
    let request_headers = request.headers().clone();

    let response = next.run(request).await;

    if !is_problem_response(&response) {
        return response;
    }

    let (parts, body) = response.into_parts();

    // The `IntoResponse` impl for `CanonicalError` stashes the original error
    // into the response extensions. Recover it so the diagnostic on
    // `Internal` / `Unknown` (carried by `#[serde(skip)]` fields and thus
    // absent from the wire body) can be logged server-side per DESIGN §3.6.
    let canonical_err = parts.extensions.get::<CanonicalError>().cloned();

    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(error = %e, "canonical error middleware: failed to read response body");
            return Response::from_parts(parts, Body::empty());
        }
    };

    let mut problem: Problem = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "canonical error middleware: failed to deserialize problem+json body");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    if problem.instance.is_none() {
        problem.instance = Some(uri_path);
    }
    if problem.trace_id.is_none() {
        problem.trace_id = extract_trace_id(&request_headers);
    }

    log_problem(&problem, canonical_err.as_ref());

    let new_bytes = match serde_json::to_vec(&problem) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(
                error = %e,
                "canonical error middleware: failed to re-serialize problem+json body"
            );
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    let mut response = Response::from_parts(parts, Body::from(new_bytes.clone()));
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(new_bytes.len()));
    response
}

fn is_problem_response(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with(PROBLEM_JSON))
}

/// W3C `traceparent` → `x-trace-id` → `x-request-id` → span-id fallback.
///
/// For `traceparent`, returns the 32-hex trace-id segment only — matching
/// `toolkit_http::otel::parse_trace_id` and the access log / `OTel` span
/// recording in this codebase, so the wire `trace_id` is grep-equal to the
/// trace-id surfaced in logs and traces. A malformed traceparent falls
/// through to `x-trace-id` / `x-request-id` (preserves the function's
/// graceful-failure semantics).
fn extract_trace_id(headers: &HeaderMap) -> Option<String> {
    if let Some(tp) = headers.get("traceparent").and_then(|v| v.to_str().ok())
        && let Some(trace_id) = parse_w3c_trace_id(tp)
    {
        return Some(trace_id);
    }
    for name in ["x-trace-id", "x-request-id"] {
        if let Some(v) = headers.get(name).and_then(|v| v.to_str().ok()) {
            return Some(v.to_owned());
        }
    }
    tracing::Span::current()
        .id()
        .map(|id| id.into_u64().to_string())
}

/// Mirror of `toolkit_http::otel::parse_trace_id`. Duplicated rather than
/// taking a new dep edge from `toolkit` onto `toolkit-http` for seven lines
/// of parsing. Keep behaviour in lock-step with the source.
fn parse_w3c_trace_id(traceparent: &str) -> Option<String> {
    let parts: Vec<&str> = traceparent.split('-').collect();
    if parts.len() >= 4 && parts[0] == "00" {
        Some(parts[1].to_owned())
    } else {
        None
    }
}

fn log_problem(problem: &Problem, canonical: Option<&CanonicalError>) {
    let status = problem.status;
    let problem_type = problem.problem_type.as_str();
    let instance = problem.instance.as_deref().unwrap_or("");
    let trace_id = problem.trace_id.as_deref().unwrap_or("");
    // `diagnostic()` returns Some only for `Internal` / `Unknown` (5xx-only
    // categories). Surface it server-side so operators can correlate
    // `trace_id` → root cause without exposing it on the wire.
    let description = canonical.and_then(CanonicalError::diagnostic).unwrap_or("");

    if (400..500).contains(&status) {
        tracing::warn!(
            status,
            problem_type,
            instance,
            trace_id,
            "canonical error response (client)"
        );
    } else if (500..600).contains(&status) {
        tracing::error!(
            status,
            problem_type,
            instance,
            trace_id,
            description,
            "canonical error response (server)"
        );
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware::from_fn,
        routing::get,
    };
    use serde_json::Value;
    use toolkit_canonical_errors::CanonicalError;
    use tower::ServiceExt;

    fn problem_response(problem: &Problem, status: StatusCode) -> Response {
        let body = serde_json::to_vec(problem).expect("serialize problem");
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, PROBLEM_JSON)
            .body(Body::from(body))
            .expect("build response")
    }

    fn build_app(responder: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
        Router::new()
            .route(
                "/api/v1/widgets/42",
                get(move || {
                    let responder = responder.clone();
                    async move { responder() }
                }),
            )
            .layer(from_fn(canonical_error_middleware))
    }

    async fn body_to_problem(response: Response) -> Problem {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("parse problem+json")
    }

    #[tokio::test]
    async fn fills_instance_and_trace_id_from_headers() {
        let problem: Problem = CanonicalError::internal("boom").create().into();
        let app = build_app(move || problem_response(&problem, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let problem = body_to_problem(res).await;

        assert_eq!(problem.instance.as_deref(), Some("/api/v1/widgets/42"));
        // Only the 32-hex trace-id segment is surfaced on the wire — matches
        // the format the access log and OTel span recording use, so an
        // operator can grep the same value across all three.
        assert_eq!(
            problem.trace_id.as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[tokio::test]
    async fn does_not_overwrite_existing_instance() {
        let preset: Problem =
            Problem::from(CanonicalError::internal("boom").create()).with_instance("/handler-set");
        let app = build_app(move || problem_response(&preset, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let problem = body_to_problem(res).await;

        assert_eq!(problem.instance.as_deref(), Some("/handler-set"));
    }

    #[tokio::test]
    async fn does_not_overwrite_existing_trace_id() {
        let preset: Problem =
            Problem::from(CanonicalError::internal("boom").create()).with_trace_id("handler-trace");
        let app = build_app(move || problem_response(&preset, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .header("traceparent", "should-be-ignored")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let problem = body_to_problem(res).await;

        assert_eq!(problem.trace_id.as_deref(), Some("handler-trace"));
    }

    #[tokio::test]
    async fn passes_through_non_problem_responses_verbatim() {
        let payload = b"{\"hello\":\"world\"}";
        let app = Router::new()
            .route(
                "/plain",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(&b"{\"hello\":\"world\"}"[..]))
                        .unwrap()
                }),
            )
            .layer(from_fn(canonical_error_middleware));

        let req = Request::builder()
            .uri("/plain")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), payload);
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn malformed_problem_passes_through_with_error_log() {
        let raw = b"{not-json}";
        let app = Router::new()
            .route(
                "/api/v1/widgets/42",
                get(|| async {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::CONTENT_TYPE, PROBLEM_JSON)
                        .body(Body::from(&b"{not-json}"[..]))
                        .unwrap()
                }),
            )
            .layer(from_fn(canonical_error_middleware));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), raw);
        assert!(logs_contain(
            "canonical error middleware: failed to deserialize problem+json body"
        ));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn logs_warn_for_4xx_and_error_for_5xx() {
        // 4xx → warn
        let problem_4xx: Problem = CanonicalError::unauthenticated()
            .with_reason("MISSING_TOKEN")
            .create()
            .into();
        let app_4xx = build_app(move || problem_response(&problem_4xx, StatusCode::UNAUTHORIZED));
        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();
        let _ = app_4xx.oneshot(req).await.unwrap();
        assert!(logs_contain("canonical error response (client)"));

        // 5xx → error
        let problem_5xx: Problem = CanonicalError::internal("boom").create().into();
        let app_5xx =
            build_app(move || problem_response(&problem_5xx, StatusCode::INTERNAL_SERVER_ERROR));
        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();
        let _ = app_5xx.oneshot(req).await.unwrap();
        assert!(logs_contain("canonical error response (server)"));
    }

    #[tokio::test]
    async fn extract_trace_id_prefers_traceparent_over_other_headers() {
        let problem: Problem = CanonicalError::internal("boom").create().into();
        let app = build_app(move || problem_response(&problem, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .header("x-trace-id", "from-x-trace-id")
            .header("x-request-id", "from-x-request-id")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let problem = body_to_problem(res).await;

        // 32-hex trace-id segment from traceparent wins over the other headers.
        assert_eq!(
            problem.trace_id.as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );
    }

    #[tokio::test]
    async fn malformed_traceparent_falls_through_to_x_trace_id() {
        let problem: Problem = CanonicalError::internal("boom").create().into();
        let app = build_app(move || problem_response(&problem, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .header("traceparent", "not-a-w3c-traceparent")
            .header("x-trace-id", "from-x-trace-id")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let problem = body_to_problem(res).await;

        // Malformed traceparent does not block extraction from the next header.
        assert_eq!(problem.trace_id.as_deref(), Some("from-x-trace-id"));
    }

    #[tokio::test]
    async fn falls_back_to_span_id_when_no_trace_headers_present() {
        // Parity with the `sets_trace_id_when_in_span` test the legacy
        // `CanonicalProblemMigrationExt` trait file used to carry: when
        // none of `traceparent` / `x-trace-id` / `x-request-id` is set,
        // the middleware fills `trace_id` from `tracing::Span::current().id()`.
        use tracing::Instrument;
        use tracing_subscriber::fmt;

        // Thread-local subscriber so the assigned span ID is observable;
        // `set_default` returns a guard that restores the previous default
        // when dropped.
        let subscriber = fmt().with_test_writer().finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("span_id_fallback_test");
        let span_id = span
            .id()
            .expect("the test subscriber must assign an ID to the span")
            .into_u64()
            .to_string();

        let problem: Problem = CanonicalError::internal("boom").create().into();
        let app = build_app(move || problem_response(&problem, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();

        // `.instrument(span)` makes `span` the current span every time the
        // request future is polled, so `Span::current().id()` inside the
        // middleware (after `next.run(...).await`) resolves to `Some(span)`.
        let res = app.oneshot(req).instrument(span).await.unwrap();
        let problem = body_to_problem(res).await;

        assert_eq!(
            problem.trace_id.as_deref(),
            Some(span_id.as_str()),
            "trace_id should fall back to the active span's id when no header is present",
        );
    }

    #[tokio::test]
    async fn body_is_valid_json_after_rewrite() {
        let problem: Problem = CanonicalError::internal("boom").create().into();
        let app = build_app(move || problem_response(&problem, StatusCode::INTERNAL_SERVER_ERROR));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .header("x-trace-id", "abc123")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).expect("rewritten body must be valid JSON");
        assert_eq!(v["instance"].as_str(), Some("/api/v1/widgets/42"));
        assert_eq!(v["trace_id"].as_str(), Some("abc123"));
    }

    #[tokio::test]
    #[tracing_test::traced_test]
    async fn logs_internal_description_from_extension() {
        // `Internal::description` is `#[serde(skip)]` so the wire body cannot
        // carry the unredacted message. The middleware recovers the original
        // `CanonicalError` from response extensions (DESIGN §3.6) and logs
        // the diagnostic alongside `trace_id` for server-side correlation.
        use axum::response::IntoResponse;

        let app = Router::new()
            .route(
                "/api/v1/widgets/42",
                get(|| async {
                    CanonicalError::internal("db connection refused: secret-host:5432")
                        .create()
                        .into_response()
                }),
            )
            .layer(from_fn(canonical_error_middleware));

        let req = Request::builder()
            .uri("/api/v1/widgets/42")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);

        // The wire body must NOT contain the diagnostic.
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&bytes).unwrap();
        assert!(
            !body_str.contains("secret-host:5432"),
            "diagnostic must not appear on the wire"
        );

        // Server-side log must contain the diagnostic.
        assert!(logs_contain("canonical error response (server)"));
        assert!(logs_contain("db connection refused: secret-host:5432"));
    }
}
