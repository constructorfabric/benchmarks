//! The wire layer: axum handlers over [`crate::domain::service::Service`].
//!
//! Two surface families live here, both mounted under `/oagw/v1`:
//!
//! * [`handlers`] — the management API (upstream / route / plugin CRUD);
//! * [`proxy`] — the data plane, `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]`.
//!
//! Every failure these handlers produce is a [`DomainError`], rendered as an
//! `application/problem+json` body carrying `X-OAGW-Error-Source: gateway`.
//! Upstream responses — including upstream *errors* — are passed through with
//! `X-OAGW-Error-Source: upstream`.

use std::sync::Arc;

use axum::http::{HeaderName, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

use crate::domain::service::Service;
use crate::error::{DomainError, PROBLEM_JSON};
use crate::ids::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER};
use crate::infra::outbound::Outbound;

pub mod handlers;
pub mod proxy;
pub mod routes;

/// State every handler extracts.
#[derive(Clone)]
pub struct ApiState {
    /// The control-plane / data-plane service.
    pub service: Arc<Service>,
    /// The outbound transport.
    pub outbound: Arc<Outbound>,
}

/// Parse a request body as JSON.
///
/// # Errors
///
/// [`crate::error::ErrorKind::Validation`] with the serde error message.
pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, DomainError> {
    serde_json::from_slice(body).map_err(|err| {
        DomainError::new(
            crate::error::ErrorKind::Validation,
            format!("request body is not valid: {err}"),
        )
    })
}

/// Render a [`DomainError`] as the problem response it is specified as.
///
/// `instance` comes from the request path and `trace_id` from the
/// `x-request-id` header the platform middleware injects, when present.
pub(crate) fn problem(err: DomainError, uri: &Uri, trace_id: Option<&str>) -> Response {
    let status = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = err.to_problem(Some(uri.path()), trace_id);
    let mut response = (status, axum::Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// The `x-request-id` header value, when the platform middleware supplied one.
pub(crate) fn trace_id_of(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
}
