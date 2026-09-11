//! Proxy REST handler (DESIGN §3.3 "Proxy API").
//!
//! The handler detaches the request from the HTTP server — method, path suffix,
//! query, headers, buffered body and the pending WebSocket upgrade — and hands
//! it to the [`DataPlaneService`].

use axum::extract::{Extension, Path, RawQuery};
use http::{HeaderMap, Request};
use http_body_util::BodyExt;
use toolkit_security::SecurityContext;

use crate::api::rest::error;
use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::infra::proxy::service::{self, DataPlaneService, ProxyBody, ProxyCall};

type Response = http::Response<ProxyBody>;

/// `ANY /oagw/v1/proxy/{*path}` — the data plane entry point.
///
/// Body validation happens here, where the inbound body is still available:
/// transfer encodings other than `chunked`, a `Content-Length` that disagrees
/// with the actual body, and bodies over `max_body_bytes` are all rejected
/// before any upstream work (DESIGN §3.2 "Body Validation Rules").
///
/// # Errors
/// Returns a problem response for a malformed request body.
pub async fn proxy(
    Extension(data_plane): Extension<std::sync::Arc<DataPlaneService>>,
    Extension(config): Extension<OagwConfig>,
    Extension(ctx): Extension<SecurityContext>,
    Path(suffix): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    mut request: Request<ProxyBody>,
) -> Response {
    // The upgrade future must be captured before the request is taken apart.
    let upgrade = if service::is_websocket_upgrade(request.method(), &headers) {
        Some(hyper::upgrade::on(&mut request))
    } else {
        None
    };

    let content_length = headers.get(http::header::CONTENT_LENGTH).cloned();
    let transfer_encoding = headers.get(http::header::TRANSFER_ENCODING).cloned();
    let (parts, body) = request.into_parts();

    let mut call = ProxyCall {
        tenant_id: ctx.subject_tenant_id().to_string(),
        user_id: Some(ctx.subject_id().to_string()),
        client_ip: client_ip(&headers),
        method: parts.method.clone(),
        path: format!("/{suffix}"),
        query: query.unwrap_or_default(),
        headers,
        body: bytes::Bytes::new(),
        upgrade,
    };

    if let Err(e) = validate_encoding(transfer_encoding.as_ref()) {
        return error::problem_response(&e, Some(&call.path));
    }

    call.body = match collect_body(content_length.as_ref(), body, &config).await {
        Ok(bytes) => bytes,
        Err(e) => return error::problem_response(&e, Some(&call.path)),
    };

    data_plane.proxy(call).await
}

/// The client IP as advertised by the fronting proxy, when present.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Rejects transfer encodings the proxy cannot frame.
fn validate_encoding(encoding: Option<&http::HeaderValue>) -> Result<(), DomainError> {
    let Some(value) = encoding else {
        return Ok(());
    };
    let value = value.to_str().unwrap_or_default();
    let supported = value
        .split(',')
        .map(str::trim)
        .all(|token| token.is_empty() || token.eq_ignore_ascii_case("chunked"));
    if supported {
        Ok(())
    } else {
        Err(DomainError::Validation(format!(
            "unsupported transfer encoding `{value}`; only chunked is supported"
        )))
    }
}

/// Buffers the inbound body, enforcing the declared length and the size limit.
async fn collect_body(
    content_length: Option<&http::HeaderValue>,
    body: ProxyBody,
    config: &OagwConfig,
) -> Result<bytes::Bytes, DomainError> {
    let declared = content_length
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| DomainError::Validation("content-length is not a valid integer".to_owned()))?;
    // A known size is checked before a single byte is buffered.
    if let Some(expected) = declared
        && expected > config.max_body_bytes()
    {
        return Err(DomainError::PayloadTooLarge);
    }
    let collected = body
        .collect()
        .await
        .map_err(|e| DomainError::Validation(format!("request body could not be read: {e}")))?
        .to_bytes();
    if let Some(expected) = declared
        && expected != collected.len() as u64
    {
        return Err(DomainError::Validation(format!(
            "content-length {expected} does not match the body size {}",
            collected.len()
        )));
    }
    if collected.len() as u64 > config.max_body_bytes() {
        return Err(DomainError::PayloadTooLarge);
    }
    Ok(collected)
}
