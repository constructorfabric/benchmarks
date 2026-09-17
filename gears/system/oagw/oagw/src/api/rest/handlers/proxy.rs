//! Handlers for the OAGW data plane: `/oagw/v1/proxy/{alias}` and
//! `/oagw/v1/proxy/{alias}/{*path}`.
//!
//! These are the only two proxy routes the gear exposes; every HTTP method is
//! routed, because the method is a *route-matching* input rather than an
//! operation id. The handlers are deliberately thin: they hand the downstream
//! request to [`crate::infra::proxy::pipeline::DataPlaneService`] and render a
//! failure as an RFC 9457 problem document.
//!
//! Bodies are **not** touched here — they are streamed end to end by the
//! pipeline, which is why the handlers take the whole [`Request`] instead of a
//! buffered body extractor.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;

use super::SharedDataPlane;
use crate::api::rest::error::OagwProblem;
use crate::infra::proxy::pipeline::PipelineFailure;

/// `GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS /oagw/v1/proxy/{alias}` — proxy
/// without a path suffix.
pub async fn proxy_root(
    Extension(data_plane): Extension<SharedDataPlane>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    proxy(data_plane, request, &alias, "").await
}

/// `GET|POST|PUT|DELETE|PATCH|HEAD|OPTIONS /oagw/v1/proxy/{alias}/{*path}` —
/// proxy with a path suffix.
pub async fn proxy_path(
    Extension(data_plane): Extension<SharedDataPlane>,
    Path((alias, path_suffix)): Path<(String, String)>,
    request: Request,
) -> Response {
    proxy(data_plane, request, &alias, &path_suffix).await
}

/// Run the pipeline and render its outcome.
///
/// The caller's [`SecurityContext`] (installed by the platform auth
/// middleware) is read out of the request extensions; an anonymous context is
/// substituted in deployments that run without authentication, so the data
/// plane still resolves a tenant chain.
async fn proxy(data_plane: SharedDataPlane, request: Request, alias: &str, path: &str) -> Response {
    let security = request
        .extensions()
        .get::<SecurityContext>()
        .cloned()
        .unwrap_or_else(SecurityContext::anonymous);
    match data_plane.proxy(request, alias, path, security, None).await {
        Ok(mut response) => {
            // ADR 0007: the header is present on *every* response, not only on
            // errors. A relayed upstream body is upstream-owned, so that is the
            // default; the pipeline sets `gateway` itself where it answers
            // locally (CORS preflight), and the error arm renders its own.
            if !response
                .headers()
                .contains_key(crate::api::rest::error::OAGW_ERROR_SOURCE_HEADER)
            {
                response.headers_mut().insert(
                    crate::api::rest::error::OAGW_ERROR_SOURCE_HEADER,
                    axum::http::HeaderValue::from_static("upstream"),
                );
            }
            response
        }
        Err(failure) => render_failure(failure),
    }
}

/// Render a [`PipelineFailure`] as the response the client sees.
///
/// The problem document comes from the standard `DomainError` mapping; the
/// `on_error` transform phase may then have added headers (and even a body),
/// which are folded in on top.
fn render_failure(failure: PipelineFailure) -> Response {
    let mut problem = OagwProblem::from(failure.error);
    if let Some(host) = failure.host.as_ref() {
        problem = problem.with_host(host.clone());
    }
    if let Some(path) = failure.path.as_ref() {
        problem = problem.with_path(path.clone());
    }
    // `trace_id` is a DESIGN extension member: an `on_error` transform that
    // produced a correlation id hands it to the client here.
    if let Some(trace_id) = failure.trace_id {
        problem = problem.with_trace_id(Some(trace_id));
    }
    let mut response = problem.into_response();
    for (name, value) in failure.headers.iter() {
        response.headers_mut().append(name, value.clone());
    }
    if let Some(body) = failure.body {
        *response.body_mut() = Body::from(body);
    }
    response
}

/// Re-exported so the route table can name the type without reaching into
/// `infra`.
#[allow(dead_code)]
pub type DataPlane = Arc<crate::infra::proxy::pipeline::DataPlaneService>;
