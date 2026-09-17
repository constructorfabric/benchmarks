//! Data-plane proxy handler.
//!
//! The catch-all under `/oagw/v1/proxy/{*rest}` forwards to the resolved
//! upstream. Gateway errors are rendered as RFC 9457 `problem+json` bodies
//! carrying the exact `type` identifiers from `docs/DESIGN.md` §3.3 and the
//! `X-OAGW-Error-Source: gateway` header; upstream responses are passed through
//! as-is with `X-OAGW-Error-Source: upstream`.
//!
//! Three response shapes are rendered, matching
//! [`crate::domain::services::proxy_service::ProxyOutcome`]: a buffered body, a
//! streamed body (SSE and any other incremental response) and an upgraded
//! connection, whose tunnel the service has already spliced.
//!
//! `OPTIONS` preflights are answered permissively at the handler level — no
//! upstream resolution and no tenant context (DESIGN.md §3.2, CORS).

use axum::Extension;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use toolkit_security::SecurityContext;
use tracing::field::Empty;

use super::caller_tenant;
use crate::config::MAX_BODY_BYTES;
use crate::domain::error::DomainError;
use crate::domain::models::TARGET_HOST_HEADER;
use crate::domain::services::proxy_service::{
    ERROR_SOURCE_HEADER, ProxyFailure, ProxyOutcome, ProxyRequest, SOURCE_GATEWAY, SOURCE_UPSTREAM,
};
use crate::gear::OagwState;

type SharedState = Extension<std::sync::Arc<OagwState>>;

/// Forwards one proxied request.
///
/// The eight parameters are axum's extractor list, not an internal API, so the
/// arity is intrinsic to the handler shape. The trailing [`Request`] is what
/// makes a WebSocket exchange tunnelable: it is where hyper parks the pending
/// [`hyper::upgrade::OnUpgrade`] for the `101` response.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip(state, ctx, headers, request, uri), fields(request_id = Empty))]
pub async fn proxy_catchall(
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(rest): Path<String>,
    request: Request,
) -> Response {
    let inbound_upgrade = request
        .extensions()
        .get::<hyper::upgrade::OnUpgrade>()
        .cloned();
    // The 100 MB request-body limit is enforced here, *before* the body is
    // buffered: `to_bytes` stops reading (and aborts the exchange) at the limit.
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => {
            return failure_response(&ProxyFailure::Domain(DomainError::PayloadTooLarge(
                MAX_BODY_BYTES,
            )));
        }
    };
    let request = build_request(&ctx, &method, &uri, &headers, query, rest, body, inbound_upgrade);
    match state.proxy.proxy(&request).await {
        Ok(outcome) => render_upstream(outcome),
        Err(failure) => failure_response(&failure),
    }
}

/// Permissive preflight: always `204` with permissive CORS headers.
#[tracing::instrument(skip(state, ctx), fields(request_id = Empty))]
pub async fn proxy_preflight(
    Extension(ctx): Extension<SecurityContext>,
    state: SharedState,
    Path(rest): Path<String>,
) -> Response {
    // Phase-2 seam: origin-scoped CORS policy. Preflight needs neither the
    // upstream nor the tenant context, so nothing else is consulted.
    let _ = (state, ctx, rest);
    let mut response = StatusCode::NO_CONTENT.into_response();
    apply_permissive_preflight(&mut response);
    response
}

/// Assembles the domain [`ProxyRequest`] from the transport primitives.
///
/// `inbound_upgrade` is the pending protocol upgrade lifted out of the request
/// extensions, where hyper parks it until the `101` has been written; without
/// it a WebSocket exchange could not be tunnelled.
#[allow(clippy::too_many_arguments)]
fn build_request(
    ctx: &SecurityContext,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    query: Option<String>,
    rest: String,
    body: Bytes,
    inbound_upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> ProxyRequest {
    let (alias, suffix) = split_proxy_path(&rest);
    let mut request = ProxyRequest::new(
        caller_tenant(ctx),
        alias,
        method.clone(),
        normalize_suffix(&suffix, uri),
    );
    request.query = query.filter(|q| !q.is_empty());
    request.body = body;
    request.headers = filter_inbound_headers(headers);
    request.target_host = headers
        .get(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    request.subject_id = Some(ctx.subject_id());
    request.security = Some(std::sync::Arc::new(ctx.clone()));
    request.inbound_upgrade = inbound_upgrade;
    request
}

/// Splits `{alias}/{path_suffix}` from the catch-all segment.
fn split_proxy_path(rest: &str) -> (String, String) {
    let rest = rest.trim_start_matches('/');
    match rest.split_once('/') {
        Some((alias, suffix)) => (alias.to_owned(), suffix.to_owned()),
        None => (rest.to_owned(), String::new()),
    }
}

/// The catch-all arrives without its leading slash; restore it, so the suffix
/// is the aliased sub-path exactly as it appeared on the wire (`/v1/models`).
fn normalize_suffix(suffix: &str, uri: &Uri) -> String {
    if suffix.is_empty() {
        return String::new();
    }
    let path = uri.path();
    if let Some(index) = path.rfind(&format!("/{suffix}")) {
        return path[index..].to_owned();
    }
    format!("/{suffix}")
}

/// Copies inbound headers, dropping hop-by-hop headers and the OAGW control
/// headers plus inbound credentials that must never reach an upstream.
fn filter_inbound_headers(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        let dropped = key.starts_with("x-oagw-")
            || matches!(
                key.as_str(),
                "authorization" | "cookie" | "connection" | "host" | "content-length" | "transfer-encoding"
            );
        if dropped {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

/// Renders an upstream outcome to the caller.
fn render_upstream(outcome: ProxyOutcome) -> Response {
    let status = outcome.status();
    let mut builder = Response::builder().status(status);
    for (name, value) in outcome.headers() {
        builder = builder.header(name, value);
    }
    builder
        .header(HeaderName::from_static(ERROR_SOURCE_HEADER), SOURCE_UPSTREAM)
        .body(match outcome {
            ProxyOutcome::Response(response) => Body::from(response.body),
            ProxyOutcome::Stream { body, .. } => body,
            ProxyOutcome::Upgraded { .. } => Body::empty(),
        })
        .unwrap_or_else(|e| failure_response(&ProxyFailure::Domain(DomainError::Internal(e.to_string()))))
}

/// Applies the permissive preflight headers (ADR 0004).
fn apply_permissive_preflight(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("access-control-allow-origin"),
        HeaderValue::from_static("*"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-methods"),
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
    );
    headers.insert(
        HeaderName::from_static("access-control-allow-headers"),
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        HeaderName::from_static("access-control-max-age"),
        HeaderValue::from_static("600"),
    );
}

/// Renders a gateway failure as an RFC 9457 problem document.
///
/// The `type` member carries the exact GTS error identifier from the DESIGN
/// error table (or the ADR 0004 CORS identifiers), and
/// `X-OAGW-Error-Source: gateway` marks the origin.
pub fn failure_response(failure: &ProxyFailure) -> Response {
    let status = StatusCode::from_u16(failure.http_status())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (
        status,
        [(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_static(SOURCE_GATEWAY),
        )],
        axum::Json(toolkit_canonical_errors::Problem {
            problem_type: failure.gts_type(),
            title: failure.title(),
            status: failure.http_status(),
            detail: failure.detail(),
            instance: None,
            trace_id: None,
            context: serde_json::json!({ "retriable": retriable(failure) }),
            error_code: None,
            error_domain: None,
        }),
    )
        .into_response();
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    if let ProxyFailure::RateLimited(rejection) = failure {
        // ADR 0003: a 429 carries `Retry-After` *and* the `X-RateLimit-*`
        // headers, exactly as a proxied response does.
        rejection.apply_rate_limit_headers(response.headers_mut());
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from(rejection.retry_after_secs));
    }
    response
}

/// `true` when the caller may retry the failed exchange unchanged.
fn retriable(failure: &ProxyFailure) -> bool {
    match failure {
        ProxyFailure::Domain(error) => error.retriable(),
        ProxyFailure::Cors(_) => false,
        ProxyFailure::RateLimited(_) => true,
    }
}

/// Renders a gateway error as an RFC 9457 problem document.
///
/// The `type` member carries the exact GTS error identifier from the DESIGN
/// error table, and `X-OAGW-Error-Source: gateway` marks the origin.
pub fn problem_response(error: &DomainError) -> Response {
    failure_response(&ProxyFailure::Domain(error.clone()))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn proxy_suffix_keeps_its_leading_slash() {
        let uri: Uri = "/oagw/v1/proxy/upstream.test/v1/models".parse().unwrap();
        assert_eq!(normalize_suffix("v1/models", &uri), "/v1/models");
        let uri: Uri = "/oagw/v1/proxy/a.b.c".parse().unwrap();
        assert_eq!(normalize_suffix("", &uri), "");
    }

    #[test]
    fn proxy_paths_split_into_alias_and_suffix() {
        assert_eq!(split_proxy_path("api.openai.com/v1/models"), ("api.openai.com".to_owned(), "v1/models".to_owned()));
        assert_eq!(split_proxy_path("api.openai.com"), ("api.openai.com".to_owned(), String::new()));
        assert_eq!(split_proxy_path("/a.b.c/"), ("a.b.c".to_owned(), String::new()));
    }

    #[test]
    fn gateway_problems_carry_the_design_error_type() {
        let error = DomainError::RouteNotFound {
            method: "GET".to_owned(),
            path: "/v1/x".to_owned(),
            alias: "api.openai.com".to_owned(),
        };
        let response = problem_response(&error);
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get("x-oagw-error-source").unwrap(),
            "gateway"
        );
        assert_eq!(
            response.headers().get(axum::http::header::CONTENT_TYPE).unwrap(),
            "application/problem+json"
        );
    }

    #[test]
    fn a_cors_refusal_is_a_403_problem_document() {
        let failure = ProxyFailure::Cors(crate::domain::services::proxy_service::CorsRejection::origin(
            Some("https://evil.example".to_owned()),
        ));
        let response = failure_response(&failure);
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get("x-oagw-error-source").unwrap(),
            "gateway"
        );
    }

    #[test]
    fn a_rate_limit_refusal_carries_retry_after() {
        let rejection = crate::domain::services::proxy_service::RateLimitRejection {
            upstream: "a.example".to_owned(),
            limit: 5,
            remaining: 0,
            reset_secs: 3,
            retry_after_secs: 7,
        };
        let response = failure_response(&ProxyFailure::RateLimited(rejection));
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get("retry-after").unwrap(), "7");
        assert_eq!(response.headers().get("x-ratelimit-limit").unwrap(), "5");
        assert_eq!(response.headers().get("x-ratelimit-remaining").unwrap(), "0");
        assert_eq!(response.headers().get("x-ratelimit-reset").unwrap(), "3");
        assert_eq!(
            response.headers().get("x-oagw-error-source").unwrap(),
            "gateway"
        );
    }

    #[test]
    fn inbound_credentials_never_reach_the_upstream() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer secret"),
        );
        headers.insert(HeaderName::from_static("x-oagw-target-host"), HeaderValue::from_static("a"));
        headers.insert(HeaderName::from_static("accept"), HeaderValue::from_static("application/json"));
        let filtered = filter_inbound_headers(&headers);
        assert!(filtered.get("authorization").is_none());
        assert!(filtered.get("x-oagw-target-host").is_none());
        assert_eq!(filtered.get("accept").unwrap(), "application/json");
    }

    #[test]
    fn preflight_is_permissive() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        apply_permissive_preflight(&mut response);
        assert_eq!(
            response.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }
}
