//! The oagw error-response contract (`cpt-cf-oagw-flow-error-response`).
//!
//! Every gateway-produced failure is an RFC 9457 `application/problem+json`
//! document carrying the five standard members (`type`, `title`, `status`,
//! `detail`, `instance`), the occurrence extension members declared by the
//! variant (`upstream_id`, `host`, `path`, `retry_after_seconds`, `trace_id`,
//! and, for the conflicts, `plugin_id` and `referenced_by`)
//! and the `X-OAGW-Error-Source: gateway` header. Upstream-produced error
//! responses are never re-serialized: they pass through with their status,
//! body and headers untouched and only gain `X-OAGW-Error-Source: upstream`.
// @cpt-begin:cpt-cf-oagw-dod-error-contract:p1:inst-full

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use serde_json::Value;

use crate::domain::error::OagwError;

/// Name of the header that declares who produced an error response.
pub const X_OAGW_ERROR_SOURCE: &str = "x-oagw-error-source";

/// `X-OAGW-Error-Source` value for a response the gateway itself produced.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// `X-OAGW-Error-Source` value for a response an upstream produced.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// RFC 9457 media type of every gateway-produced error response.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// Extension marker identifying a response the gateway itself produced.
///
/// The `IntoResponse` impl for [`OagwError`] inserts it so the error
/// middleware can patch `instance` / `trace_id` without ever touching an
/// upstream passthrough response.
#[derive(Debug, Clone, Copy)]
pub struct GatewayProblem;

/// Which side of the gateway produced an error response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorSource {
    /// The gateway produced the error before or while proxying.
    Gateway,
    /// The upstream produced the error and the gateway forwarded it.
    Upstream,
}

impl ErrorSource {
    /// The `X-OAGW-Error-Source` header value for this side.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gateway => ERROR_SOURCE_GATEWAY,
            Self::Upstream => ERROR_SOURCE_UPSTREAM,
        }
    }
}

/// The RFC 9457 problem document the gateway emits (`inst-er-01`..`inst-er-04`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProblemBody {
    /// GTS instance identifier of the mapped variant (RFC 9457 `type`).
    #[serde(rename = "type")]
    pub problem_type: String,
    /// Short human-readable summary (RFC 9457 `title`).
    pub title: String,
    /// HTTP status (RFC 9457 `status`).
    pub status: u16,
    /// Occurrence-specific explanation (RFC 9457 `detail`).
    pub detail: String,
    /// URI reference identifying this occurrence (RFC 9457 `instance`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Identifier of the upstream the request was routed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Upstream host the request was addressed to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path that produced the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Retry guidance, in seconds, for the retriable rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Distributed-tracing correlation identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Identifier of the plugin a `PluginInUse` conflict names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// The resources that still reference that plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<Value>,
}

impl ProblemBody {
    /// Projects an [`OagwError`] onto the wire problem document (`inst-em-05`):
    /// the mapping table supplies the standard members, the occurrence context
    /// supplies the extension members.
    #[must_use]
    pub fn from_error(err: &OagwError) -> Self {
        // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-03
        // Build the RFC 9457 standard members from the variant's mapping row
        // and the OAGW extension members from the occurrence context.
        // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-04
        let context = err.context();
        Self {
            problem_type: err.gts_type().to_owned(),
            title: err.title().to_owned(),
            status: err.effective_status(),
            detail: err.detail().to_owned(),
            instance: context.instance.clone(),
            upstream_id: context.upstream_id.clone(),
            host: context.host.clone(),
            path: context.path.clone(),
            retry_after_seconds: context.retry_after_seconds,
            trace_id: context.trace_id.clone(),
            plugin_id: context.plugin_id.clone(),
            referenced_by: context.referenced_by.clone(),
        }
        // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-04
        // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-03
    }
}

/// The status override an occurrence carries when the wire cannot represent it.
///
/// The status an override is rendered with has to be one the response line can
/// name, so an override outside the 100 to 599 range is dropped by this
/// boundary and the status the mapping row assigns is rendered in its place —
/// the HTTP status and the body's `status` member then never disagree. The
/// occurrence keeps the override it was given; this is the layer that reports
/// the drop.
#[must_use]
fn unrepresentable_status_override(err: &OagwError) -> Option<u16> {
    let status = err.context().status_override?;
    (!OagwError::is_representable_status(status)).then_some(status)
}

/// Builds the gateway error response for `err`.
///
/// This is the gateway-classified branch of
/// `cpt-cf-oagw-flow-error-response`: the response is produced by the gateway
/// itself, never by an upstream.
#[must_use]
pub fn error_response(err: &OagwError) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-04
    // The variant was raised by the gateway, so it is gateway-classified.
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-06
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-07
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-05
    let problem = ProblemBody::from_error(err);
    if let Some(override_status) = unrepresentable_status_override(err) {
        tracing::warn!(
            override_status,
            status = problem.status,
            "oagw: the status override is outside the HTTP status range and is not rendered; \
             the status the mapping row assigns is rendered instead"
        );
    }
    let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

    let body = match serde_json::to_vec(&problem) {
        Ok(body) => body,
        Err(error) => {
            tracing::error!(error = %error, "oagw: failed to serialize problem+json body");
            Vec::new()
        }
    };

    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;

    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-08
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    headers.insert(
        X_OAGW_ERROR_SOURCE,
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    if let Some(seconds) = err.context().retry_after_seconds
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        headers.insert(header::RETRY_AFTER, value);
    }
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-08

    response.extensions_mut().insert(GatewayProblem);
    response
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-05
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-07
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-06
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-04
}

/// Adds `X-OAGW-Error-Source: upstream` to an upstream response.
///
/// This is the only mutation the gateway applies to an upstream-produced error
/// response: status, body and every other header are forwarded untouched and
/// the document is never re-serialized.
#[must_use]
pub fn upstream_passthrough(response: Response) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-06
    // The response was produced by the upstream service, not by the gateway.
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-em-07
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-09
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        X_OAGW_ERROR_SOURCE,
        HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
    );
    Response::from_parts(parts, body)
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-09
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-07
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-em-06
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-01
        // A request the gateway rejects surfaces here as an `OagwError` and
        // is answered with the mapped problem+json document.
        // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-10
        error_response(&self)
        // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-10
        // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-01
    }
}

/// Error middleware of the oagw route shell.
///
/// Fills `instance` (request URI path) and `trace_id` on gateway-produced
/// problem responses. Responses that did not originate from an [`OagwError`] —
/// including upstream passthroughs — are returned untouched.
pub async fn error_mapping_middleware(request: Request, next: Next) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-02
    // Intercept the raised `OagwError` in the REST error layer and classify
    // the variant through the mapping table.
    // @cpt-begin:cpt-cf-oagw-flow-error-response:p1:inst-er-03
    let uri_path = request.uri().path().to_owned();
    let request_headers = request.headers().clone();

    let response = next.run(request).await;

    if response.extensions().get::<GatewayProblem>().is_none() {
        return response;
    }
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-03
    // @cpt-end:cpt-cf-oagw-flow-error-response:p1:inst-er-02

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "oagw error middleware: failed to read problem body");
            return Response::from_parts(parts, Body::empty());
        }
    };

    let mut problem: ProblemBody = match serde_json::from_slice(&bytes) {
        Ok(problem) => problem,
        Err(error) => {
            tracing::error!(error = %error, "oagw error middleware: malformed problem body");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    if problem.instance.is_none() {
        problem.instance = Some(uri_path);
    }
    if problem.trace_id.is_none() {
        problem.trace_id = toolkit::api::extract_trace_id(&request_headers);
    }

    let bytes = match serde_json::to_vec(&problem) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::error!(error = %error, "oagw error middleware: failed to re-serialize problem");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };

    let mut response = Response::from_parts(parts, Body::from(bytes.clone()));
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(bytes.len()));
    response
}

// @cpt-end:cpt-cf-oagw-dod-error-contract:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::{ErrorContext, MAPPING_TABLE};
    use axum::body::to_bytes as read_body;
    use axum::http::Request as HttpRequest;
    use axum::middleware::from_fn;
    use axum::routing::get;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const STANDARD_MEMBERS: [&str; 5] = ["type", "title", "status", "detail", "instance"];
    const EXTENSION_MEMBERS: [&str; 5] = [
        "upstream_id",
        "host",
        "path",
        "retry_after_seconds",
        "trace_id",
    ];

    async fn body_json(response: Response) -> Value {
        let bytes = read_body(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn headers_of(response: &Response) -> Vec<(String, String)> {
        response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    fn upstream_response() -> Response {
        let mut response = Response::new(Body::from("<html>upstream 502 page</html>"));
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html"));
        response
            .headers_mut()
            .insert("x-upstream-trailer", HeaderValue::from_static("keep"));
        response
    }

    #[tokio::test]
    async fn gateway_errors_are_problem_json_with_the_gateway_header() {
        let err = OagwError::route_not_found("no route matches /oagw/v1/proxy/payments")
            .with_context(ErrorContext::new().with_instance("/oagw/v1/proxy/payments"));
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROBLEM_JSON)
        );
        assert_eq!(
            response
                .headers()
                .get(X_OAGW_ERROR_SOURCE)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );

        let body = body_json(response).await;
        for member in STANDARD_MEMBERS {
            assert!(
                body.get(member).is_some(),
                "missing standard member {member}"
            );
        }
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(body["title"], "Route not found");
        assert_eq!(body["status"], 404);
        assert_eq!(body["detail"], "no route matches /oagw/v1/proxy/payments");
        assert_eq!(body["instance"], "/oagw/v1/proxy/payments");
    }

    #[tokio::test]
    async fn extension_members_are_present_only_when_declared() {
        let bare = OagwError::payload_too_large("body exceeds the configured limit");
        let bare_body = body_json(bare.into_response()).await;
        for member in EXTENSION_MEMBERS {
            assert!(
                bare_body.get(member).is_none(),
                "undeclared extension member {member} leaked onto the wire"
            );
        }

        let rich = OagwError::rate_limit_exceeded("tenant quota exhausted").with_context(
            ErrorContext::new()
                .with_upstream_id("payments")
                .with_host("api.example.com")
                .with_path("/oagw/v1/proxy/payments/accounts")
                .with_instance("/oagw/v1/proxy/payments/accounts")
                .with_retry_after_seconds(9)
                .with_trace_id("4bf92f3577b34da6a3ce929d0e0e4736"),
        );
        let rich_body = body_json(rich.into_response()).await;
        for member in EXTENSION_MEMBERS {
            assert!(
                rich_body.get(member).is_some(),
                "missing extension member {member}"
            );
        }
        assert_eq!(rich_body["upstream_id"], "payments");
        assert_eq!(rich_body["host"], "api.example.com");
        assert_eq!(rich_body["path"], "/oagw/v1/proxy/payments/accounts");
        assert_eq!(rich_body["retry_after_seconds"], 9);
        assert_eq!(rich_body["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[tokio::test]
    async fn retriable_errors_carry_retry_after() {
        let err = OagwError::circuit_breaker_open("upstream circuit open")
            .with_context(ErrorContext::new().with_retry_after_seconds(12));
        let response = err.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("12")
        );
    }

    #[tokio::test]
    async fn every_mapping_row_produces_its_wire_document() {
        for row in MAPPING_TABLE {
            let err = OagwError::from_variant_name(row.variant, "occurrence detail")
                .unwrap_or_else(|| panic!("row {} is not constructible", row.variant))
                .with_context(ErrorContext::new().with_instance("/oagw/v1/proxy/x"));
            let response = err.into_response();

            let status = StatusCode::from_u16(row.status).unwrap_or(StatusCode::BAD_REQUEST);
            assert_eq!(
                response.status(),
                status,
                "status drift for {}",
                row.variant
            );
            assert_eq!(
                response
                    .headers()
                    .get(X_OAGW_ERROR_SOURCE)
                    .and_then(|value| value.to_str().ok()),
                Some(ERROR_SOURCE_GATEWAY),
                "error-source header missing for {}",
                row.variant
            );

            let body = body_json(response).await;
            assert_eq!(body["type"], row.gts_type, "type drift for {}", row.variant);
            assert_eq!(body["title"], row.title, "title drift for {}", row.variant);
            assert_eq!(
                body["status"], row.status,
                "status member drift for {}",
                row.variant
            );
            assert_eq!(
                body["detail"], "occurrence detail",
                "detail drift for {}",
                row.variant
            );
        }
    }

    #[tokio::test]
    async fn upstream_passthrough_adds_exactly_one_header() {
        let before = headers_of(&upstream_response());

        let after = upstream_passthrough(upstream_response());
        let after_headers = headers_of(&after);

        assert_eq!(after.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            after_headers.len(),
            before.len() + 1,
            "passthrough must not add or drop headers other than the error source"
        );
        assert_eq!(
            after_headers
                .iter()
                .find(|(name, _)| name == X_OAGW_ERROR_SOURCE)
                .map(|(_, value)| value.clone()),
            Some(ERROR_SOURCE_UPSTREAM.to_owned())
        );
        for (name, value) in &before {
            assert_eq!(
                after_headers.iter().find(|(key, _)| key == name),
                Some(&(name.clone(), value.clone())),
                "header {name} was rewritten by the passthrough"
            );
        }

        let bytes = read_body(after.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], b"<html>upstream 502 page</html>");
    }

    #[tokio::test]
    async fn error_middleware_fills_instance_and_trace_id() {
        let router = axum::Router::new()
            .route("/oagw/v1/proxy/payments", get(not_found_handler))
            .layer(from_fn(error_mapping_middleware));

        let response = router
            .oneshot(
                HttpRequest::get("/oagw/v1/proxy/payments")
                    .header("x-trace-id", "trace-1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_json(response).await;
        assert_eq!(body["instance"], "/oagw/v1/proxy/payments");
        assert_eq!(body["trace_id"], "trace-1234");
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    async fn not_found_handler() -> Response {
        OagwError::route_not_found("no route matches").into_response()
    }

    #[tokio::test]
    async fn error_middleware_leaves_upstream_responses_untouched() {
        let router = axum::Router::new()
            .route("/upstream", get(upstream_handler))
            .layer(from_fn(error_mapping_middleware));

        let response = router
            .oneshot(
                HttpRequest::get("/upstream")
                    .header("x-trace-id", "trace-1234")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/html")
        );
        assert!(response.extensions().get::<GatewayProblem>().is_none());
        let bytes = read_body(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&bytes[..], b"<html>upstream 502 page</html>");
    }

    async fn upstream_handler() -> Response {
        upstream_response()
    }

    #[test]
    fn error_source_values_are_the_wire_tokens() {
        assert_eq!(ErrorSource::Gateway.as_str(), "gateway");
        assert_eq!(ErrorSource::Upstream.as_str(), "upstream");
    }

    #[test]
    fn problem_body_round_trips_through_serde() {
        let err = OagwError::link_unavailable("types-registry unreachable");
        let problem = ProblemBody::from_error(&err);
        let value = json!(problem);
        assert_eq!(value["title"], "Link unavailable");

        let parsed: ProblemBody = serde_json::from_value(value).unwrap();
        assert_eq!(parsed, problem);
    }

    #[tokio::test]
    async fn a_status_override_drives_the_body_and_the_http_status() {
        let err =
            OagwError::validation_error("tenant 't' already holds the alias").with_status(409);
        assert_eq!(err.effective_status(), 409);
        assert_eq!(err.mapping().status, 400, "the row itself stays closed");
        assert_eq!(
            err.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let problem = body_json(response).await;
        assert_eq!(problem["status"], 409);
        assert_eq!(
            problem["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn an_unrepresentable_status_override_renders_a_consistent_status() {
        // A `GuardRejection` whose status the wire cannot represent (the guard
        // phases carry 400 and 502) must not render a 500 that contradicts its
        // own body: the status the mapping row assigns is rendered instead, so
        // the HTTP status and the body's `status` member are the same value.
        let err =
            OagwError::validation_error("a guard phase rejected the request").with_status(600);
        assert_eq!(
            err.context().status_override,
            Some(600),
            "the override is kept"
        );
        assert!(!OagwError::is_representable_status(600));
        assert_eq!(err.effective_status(), 400, "the row's status is rendered");
        assert_eq!(
            unrepresentable_status_override(&err),
            Some(600),
            "the render boundary reports the drop"
        );
        // An override the wire can represent is never reported.
        assert_eq!(
            unrepresentable_status_override(
                &OagwError::validation_error("rejected").with_status(409)
            ),
            None
        );

        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let problem = body_json(response).await;
        assert_eq!(problem["status"], 400, "the body carries the same status");
    }

    #[test]
    fn the_extension_members_of_a_conflict_are_absent_until_set() {
        let plain = ProblemBody::from_error(&OagwError::validation_error("rejected"));
        let rendered = serde_json::to_value(&plain).unwrap();
        for member in ["plugin_id", "referenced_by"] {
            assert!(rendered.get(member).is_none(), "{member} is skipped");
        }
        for member in EXTENSION_MEMBERS {
            assert!(rendered.get(member).is_none(), "{member} is skipped");
        }

        let conflict = OagwError::plugin_in_use("still referenced")
            .with_plugin_id("gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-0000000000aa")
            .with_referenced_by(json!({
                "upstreams": [],
                "routes": ["00000000-0000-0000-0000-0000000000aa"]
            }));
        let rendered = serde_json::to_value(ProblemBody::from_error(&conflict)).unwrap();
        assert_eq!(
            rendered["plugin_id"],
            "gts.cf.core.oagw.auth_plugin.v1~00000000-0000-0000-0000-0000000000aa"
        );
        assert_eq!(
            rendered["referenced_by"]["routes"][0],
            "00000000-0000-0000-0000-0000000000aa"
        );
    }
}
