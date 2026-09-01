//! Data Plane REST handlers (proxy, WebSocket upgrade, event stream).
//!
//! These handlers are deliberately thin: they shape the inbound request into a
//! [`RequestMeta`], delegate to [`DataPlaneService`] and map the outcome back
//! to an axum response. Bodies are streamed in both directions and never
//! materialised here.

use std::sync::Arc;

use axum::extract::{Extension, Path};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::services::proxy::{DataPlaneService, ProxyOutcome, ProxyRequest, RequestMeta};
use crate::infra::proxy::service::DataPlaneServiceImpl;

use super::error::{OagwProblem, RequestPath};

/// The shared Data Plane handle injected into the router.
pub type DataPlane = Arc<DataPlaneServiceImpl>;

/// The response type of every Data Plane handler.
pub type ProxyResponse = Response;

/// Normalises a wildcard capture into an upstream-relative path suffix.
///
/// axum's `{*path}` capture may or may not include the leading slash
/// depending on the matcher, so both shapes are folded into `/{rest}`.
#[must_use]
pub fn path_suffix(captured: Option<&str>) -> String {
    let rest = captured.unwrap_or_default().trim_start_matches('/');
    if rest.is_empty() {
        String::new()
    } else {
        format!("/{rest}")
    }
}

/// The upstream-relative request path seen by route matching.
#[must_use]
pub fn request_path(suffix: &str) -> String {
    if suffix.is_empty() {
        "/".to_owned()
    } else {
        suffix.to_owned()
    }
}

/// `true` when the inbound request asks for a protocol upgrade.
#[must_use]
pub fn is_upgrade(headers: &http::HeaderMap) -> bool {
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"));
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.is_empty());
    connection && upgrade
}

/// Renders a relayed upstream outcome as an axum response.
fn render(outcome: ProxyOutcome) -> ProxyResponse {
    let mut response = Response::new(outcome.body);
    *response.status_mut() = outcome.status;
    *response.headers_mut() = outcome.headers;
    response
}

/// Maps a Data Plane failure to its problem document, naming the resolved
/// upstream alias (`host`) and the upstream-relative request path (`path`).
fn problem(alias: &str, suffix: &str, error: DomainError) -> OagwProblem {
    OagwProblem::for_upstream(alias, &request_path(suffix), error)
}

/// Splits an inbound request into owned metadata and its streaming body.
fn split(
    request: http::Request<axum::body::Body>,
    suffix: &str,
    upgrade: bool,
) -> (RequestMeta, axum::body::Body) {
    let (parts, body) = request.into_parts();
    let meta = RequestMeta::new(
        parts.method,
        request_path(suffix),
        parts.uri.query().unwrap_or_default().to_owned(),
        parts.headers,
        upgrade,
    );
    (meta, body)
}

/// Relays a proxy request for any HTTP method, including WebSocket upgrades.
///
/// # Errors
///
/// Any routing, policy, plugin or transport failure, rendered as an RFC 9457
/// problem document carrying `X-OAGW-Error-Source: gateway`.
pub async fn relay(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<DataPlane>,
    RequestPath(instance): RequestPath,
    Path((alias, captured)): Path<(String, String)>,
    request: http::Request<axum::body::Body>,
) -> Result<ProxyResponse, OagwProblem> {
    relay_inner(ctx, svc, alias, Some(captured), instance, request).await
}

/// Relays a proxy request addressed at the alias root (`/proxy/{alias}`).
///
/// # Errors
///
/// Any routing, policy, plugin or transport failure, rendered as an RFC 9457
/// problem document carrying `X-OAGW-Error-Source: gateway`.
pub async fn relay_root(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<DataPlane>,
    RequestPath(instance): RequestPath,
    Path(alias): Path<String>,
    request: http::Request<axum::body::Body>,
) -> Result<ProxyResponse, OagwProblem> {
    relay_inner(ctx, svc, alias, None, instance, request).await
}

/// Shared body of [`relay`] and [`relay_root`].
#[allow(clippy::too_many_arguments)]
async fn relay_inner(
    ctx: SecurityContext,
    svc: DataPlane,
    alias: String,
    captured: Option<String>,
    instance: String,
    request: http::Request<axum::body::Body>,
) -> Result<ProxyResponse, OagwProblem> {
    let suffix = path_suffix(captured.as_deref());
    if is_upgrade(request.headers()) {
        return Ok(upgrade(ctx, svc, &alias, &suffix, instance, request).await);
    }
    let (meta, body) = split(request, &suffix, false);
    let outcome = svc
        .proxy(
            &ctx,
            &alias,
            &suffix,
            ProxyRequest {
                method: meta.method,
                path: meta.path,
                query: meta.query,
                headers: meta.headers,
                body,
                is_upgrade: meta.is_upgrade,
            },
        )
        .await
        .map_err(|error| problem(&alias, &suffix, error).instance(instance))?;
    Ok(render(outcome))
}

/// Relays a WebSocket upgrade and splices the two raw byte streams.
///
/// The upstream `101` is relayed verbatim; afterwards raw bytes are copied in
/// both directions between the client and the upstream upgraded IO.
async fn upgrade(
    ctx: SecurityContext,
    svc: DataPlane,
    alias: &str,
    suffix: &str,
    instance: String,
    request: http::Request<axum::body::Body>,
) -> ProxyResponse {
    let mut request = request;
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (meta, _) = split(request, suffix, true);
    match svc.upgrade(&ctx, alias, suffix, &meta).await {
        Ok((response, upstream_upgrade)) => {
            let client = client_upgrade;
            tokio::spawn(async move { splice(client, upstream_upgrade).await });
            response
        }
        Err(error) => problem(alias, suffix, error)
            .instance(instance)
            .into_response(),
    }
}

/// Copies raw bytes between the client and the upstream after a `101`.
async fn splice(
    client: hyper::upgrade::OnUpgrade,
    upstream: hyper::upgrade::OnUpgrade,
) -> Option<()> {
    let (client_io, upstream_io) = match (client.await, upstream.await) {
        (Ok(client), Ok(upstream)) => (client, upstream),
        _ => return None,
    };
    let mut client = std::pin::pin!(TokioIo::new(client_io));
    let mut upstream = std::pin::pin!(TokioIo::new(upstream_io));
    let moved = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    if let Err(error) = moved {
        tracing::debug!(error = %error, "upgrade relay ended");
    }
    Some(())
}
