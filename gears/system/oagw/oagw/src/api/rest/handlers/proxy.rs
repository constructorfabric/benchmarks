//! Proxy handlers: the data plane's HTTP surface.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::RawQuery;
use axum::extract::{Extension, Path, Request};
use axum::response::Response;
use bytes::Bytes;
use futures_util::StreamExt;
use secrecy::ExposeSecret;
use toolkit_security::SecurityContext;

use crate::api::rest::error::{OagwError, mark_upstream};
use crate::infra::plugin::request_id_transform::REQUEST_ID_HEADER;
use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::infra::proxy::body as body_rules;
use crate::infra::proxy::cors;
use crate::infra::proxy::service::{DataPlaneServiceImpl, ProxyRequestContext};

/// Bytes a bounded body read speculatively reserves before the first chunk.
const INITIAL_BODY_ALLOCATION: usize = 64 * 1024;

/// `POST /oagw/v1/proxy/{alias}/{*path}` and friends.
///
/// # Errors
/// Returns an [`OagwError`] for every gateway-generated failure.
pub async fn proxy(
    context: Option<Extension<SecurityContext>>,
    Extension(plane): Extension<Arc<DataPlaneServiceImpl>>,
    Path((alias, suffix)): Path<(String, String)>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, OagwError> {
    let (method, headers, body, upgrade) = split(request);
    run(
        context.map(|Extension(context)| context),
        &plane,
        alias,
        Some(suffix),
        raw,
        method,
        headers,
        body,
        upgrade,
    )
    .await
}

/// `POST /oagw/v1/proxy/{alias}` and friends: the same handler with no suffix.
///
/// # Errors
/// Returns an [`OagwError`] for every gateway-generated failure.
pub async fn proxy_root(
    context: Option<Extension<SecurityContext>>,
    Extension(plane): Extension<Arc<DataPlaneServiceImpl>>,
    Path(alias): Path<String>,
    RawQuery(raw): RawQuery,
    request: Request,
) -> Result<Response, OagwError> {
    let (method, headers, body, upgrade) = split(request);
    run(
        context.map(|Extension(context)| context),
        &plane,
        alias,
        None,
        raw,
        method,
        headers,
        body,
        upgrade,
    )
    .await
}

/// Splits the inbound request into the parts the pipeline wants.
///
/// The `OnUpgrade` extension is present only when the client asked to switch
/// protocols; `None` means there is nothing to tunnel.
fn split(request: Request) -> (http::Method, http::HeaderMap, Body, Option<hyper::upgrade::OnUpgrade>) {
    let (parts, body) = request.into_parts();
    let upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();
    (parts.method, parts.headers, body, upgrade)
}

/// Shared body of both proxy entry points.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run(
    context: Option<SecurityContext>,
    plane: &DataPlaneServiceImpl,
    alias: String,
    suffix: Option<String>,
    raw: Option<String>,
    method: http::Method,
    headers: http::HeaderMap,
    body: Body,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
) -> Result<Response, OagwError> {
    let config = plane.config().clone();

    // A preflight is answered before anything else: it carries no credentials,
    // needs no tenant and must not reach the upstream (ADR-0004).
    if cors::is_preflight(&method, &headers) {
        return Ok(cors::preflight_response(&headers));
    }

    let context = context.ok_or_else(|| {
        OagwError::new(DomainError::AuthenticationFailed(
            "the proxy requires an authenticated security context".into(),
        ))
    })?;

    let query = parse_query(raw.as_deref());
    let instance = proxy_instance(&alias, suffix.as_deref());

    let declared =
        body_rules::validate_content_length_declaration(&headers).map_err(OagwError::new)?;
    body_rules::validate_transfer_encoding(&headers).map_err(OagwError::new)?;
    let body = read_body(body, declared, &config).await.map_err(OagwError::new)?;

    let mut request = ProxyRequestContext {
        alias,
        path_suffix: suffix.unwrap_or_default(),
        query,
        inbound_headers: headers,
        method,
        tenant_id: context.subject_tenant_id(),
        subject_id: context.subject_id(),
        client_ip: None,
        bearer_token: context
            .bearer_token()
            .map(|token| token.expose_secret().to_owned()),
    };

    if is_websocket_upgrade(&request.inbound_headers) {
        return respond(
            plane
                .proxy_websocket(&mut request, upgrade)
                .await,
            &instance,
            &request,
        );
    }
    respond(
        plane.proxy(&mut request, Body::from(body)).await,
        &instance,
        &request,
    )
}

/// Turns a data-plane outcome into an axum response.
fn respond(
    outcome: Result<
        crate::infra::proxy::service::ForwardedResponse,
        crate::infra::proxy::service::ProxyFailure,
    >,
    instance: &str,
    request: &ProxyRequestContext,
) -> Result<Response, OagwError> {
    match outcome {
        Ok(forwarded) => {
            let mut headers = forwarded.headers;
            mark_upstream(&mut headers);
            let mut response = Response::new(forwarded.body);
            *response.status_mut() = forwarded.status;
            *response.headers_mut() = headers;
            Ok(response)
        }
        Err(failure) => Err(OagwError::from(failure)
            .with_instance(instance)
            .with_path(request_path(request))
            .with_host(host_header(request))
            .with_trace_id(trace_id(request))),
    }
}

/// The path the caller asked to be proxied, as the upstream would see it.
fn request_path(request: &ProxyRequestContext) -> String {
    let suffix = request.path_suffix.trim_start_matches('/');
    match request.query.first() {
        Some((name, value)) => format!("/{suffix}?{name}={value}"),
        None => format!("/{suffix}"),
    }
}

/// The `Host` the caller sent, if it sent one.
fn host_header(request: &ProxyRequestContext) -> Option<String> {
    request
        .inbound_headers
        .get(http::header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The correlation id the request settled on, if it has one.
fn trace_id(request: &ProxyRequestContext) -> Option<String> {
    request
        .inbound_headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Whether the inbound request carries a WebSocket upgrade.
fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// Builds the problem `instance` for a proxy request.
fn proxy_instance(alias: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(suffix) if !suffix.is_empty() => format!("/oagw/v1/proxy/{alias}/{suffix}"),
        _ => format!("/oagw/v1/proxy/{alias}"),
    }
}

/// Decodes the query string into ordered pairs.
fn parse_query(raw: Option<&str>) -> Vec<(String, String)> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    url::form_urlencoded::parse(raw.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

/// Reads the request body, applying the ceiling and the declared length.
///
/// Reading stops as soon as the limit is crossed, so an oversized body is
/// refused while it is still arriving and never buffered past the limit.
///
/// # Errors
/// Returns [`DomainError::PayloadTooLarge`] when the body crosses the limit and
/// [`DomainError::Validation`] when it disagrees with the declared length.
async fn read_body(
    body: Body,
    declared: Option<u64>,
    config: &OagwConfig,
) -> Result<bytes::Bytes, DomainError> {
    let limit = config.max_body_bytes;
    // The declared length is subtracted to pre-size the buffer, so a declared
    // length wider than `usize` simply leaves no room; `saturating_sub` keeps
    // that outcome, and the narrowing cast is therefore harmless.
    #[allow(clippy::cast_possible_truncation)]
    let room = declared.map_or(usize::MIN, |declared| {
        limit.saturating_sub(declared as usize)
    });
    let mut buffer = Vec::with_capacity(room.min(INITIAL_BODY_ALLOCATION));
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|error| DomainError::DownstreamError(error.to_string()))?;
        if buffer.len() + bytes.len() > limit {
            return Err(DomainError::PayloadTooLarge(format!(
                "request body exceeds the {limit} byte limit"
            )));
        }
        buffer.extend_from_slice(&bytes);
    }
    body_rules::check_body_size(buffer.len(), declared, limit)?;
    Ok(Bytes::from(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_upgrades_are_detected() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::UPGRADE, http::HeaderValue::from_static("WebSocket"));
        assert!(is_websocket_upgrade(&headers));
        headers.insert(http::header::UPGRADE, http::HeaderValue::from_static("h2c"));
        assert!(!is_websocket_upgrade(&headers));
    }

    #[test]
    fn the_instance_names_the_alias_and_suffix() {
        assert_eq!(proxy_instance("api", None), "/oagw/v1/proxy/api");
        assert_eq!(proxy_instance("api", Some("v1/models")), "/oagw/v1/proxy/api/v1/models");
    }

    #[test]
    fn query_pairs_are_decoded_in_order() {
        assert_eq!(parse_query(Some("a=1&b=two")), vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "two".to_owned()),
        ]);
        assert!(parse_query(None).is_empty());
    }

    #[tokio::test]
    async fn an_oversized_body_is_rejected_with_413() {
        let mut config = OagwConfig::default();
        config.max_body_bytes = 10;
        let body = Body::from("this body is much longer than ten bytes");
        let error = read_body(body, None, &config)
            .await
            .expect_err("too large");
        assert_eq!(error.status(), 413);
    }

    #[tokio::test]
    async fn a_declared_body_shorter_than_its_content_length_is_rejected() {
        let mut config = OagwConfig::default();
        config.max_body_bytes = 1024;
        let body = Body::from("short");
        let error = read_body(body, Some(20), &config)
            .await
            .expect_err("mismatch");
        assert_eq!(error.status(), 400);
    }

    #[tokio::test]
    async fn a_body_within_the_limit_is_read_in_full() {
        let mut config = OagwConfig::default();
        config.max_body_bytes = 1024;
        let body = Body::from("a small body");
        let read = read_body(body, Some(12), &config).await.expect("read");
        assert_eq!(&read[..], b"a small body");
    }
}

