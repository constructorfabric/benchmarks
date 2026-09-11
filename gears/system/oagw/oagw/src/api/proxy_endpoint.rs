//! The proxy endpoint: an any-method, any-path pass-through into the data plane.
//!
//! It is registered with a hand-built `MethodRouter` rather than through the typed
//! operation builder, because the operation is not a JSON request/response pair: it
//! relays whatever the caller sends and streams whatever the upstream answers with.

use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, FromRequestParts, Request};
use axum::response::{IntoResponse, Response};
use toolkit::api::OpenApiRegistry;
use toolkit_security::SecurityContext;

use crate::proxy::{ProxyRequest, ProxyService};
use crate::security::SecurityContextHolder;

/// The path segment the proxy endpoint hangs off.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy";

/// Registers the proxy endpoint for every method.
pub fn register(router: axum::Router, _openapi: &dyn OpenApiRegistry) -> axum::Router {
    // The braces are doubled because the route templates are built through `format!`.
    let aliased = format!("{PROXY_PREFIX}/{{alias}}");
    let nested = format!("{PROXY_PREFIX}/{{alias}}/{{*path}}");
    let router = router.route(&nested, axum::routing::any(handle));
    router.route(&aliased, axum::routing::any(handle))
}

/// The proxy handler: resolves the alias and relays.
///
/// A WebSocket upgrade is detected before the body is buffered, because an upgraded
/// request has no body to read and the handshake must not wait on one.
///
/// Never returns an error: every failure becomes an RFC 9457 problem document, so a
/// gateway-generated error is never mistaken for an upstream one.
async fn handle(
    Extension(svc): Extension<Arc<ProxyService>>,
    Extension(ctx): Extension<SecurityContext>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();

    let Some((alias, path)) = split_path(parts.uri.path()) else {
        return crate::error::OagwError::new(
            crate::error::ErrorKind::ValidationError,
            "the proxy path must name an upstream alias",
        )
        .into_response();
    };

    // A browser preflight carries no credentials, so there is no tenant context to
    // resolve an upstream with (ADR-0004): it is answered here, permissively and
    // before the caller is resolved. The allowlists are enforced on the actual
    // request that follows.
    if crate::proxy::cors::is_preflight(&parts.method, &parts.headers) {
        return crate::proxy::cors::preflight_response(&parts.headers);
    }

    let chain = match svc.chain_for(&ctx).await {
        Ok(chain) => chain,
        Err(err) => {
            return crate::error::OagwError::new(
                crate::error::ErrorKind::Internal,
                format!("tenant resolution failed: {err}"),
            )
            .into_response();
        }
    };
    let security = SecurityContextHolder::new(ctx, chain.iter().collect());

    // An upgrade carries no body; buffering it would consume the frames the bridge needs.
    let upgrade = if crate::proxy::is_websocket_upgrade(&parts.headers) {
        let mut parts = parts.clone();
        match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(upgrade) => Some(upgrade),
            Err(_) => {
                return crate::error::OagwError::new(
                    crate::error::ErrorKind::ProtocolError,
                    "the caller's websocket upgrade handshake is not valid",
                )
                .into_response();
            }
        }
    } else {
        None
    };

    let body = if upgrade.is_some() {
        bytes::Bytes::new()
    } else {
        match axum::body::to_bytes(body, crate::proxy::MAX_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return crate::error::OagwError::new(
                    crate::error::ErrorKind::PayloadTooLarge,
                    "the request body could not be buffered within the size limit",
                )
                .into_response();
            }
        }
    };

    let mut proxy_request = ProxyRequest {
        method: parts.method,
        alias,
        path,
        query: parts.uri.query().unwrap_or_default().to_owned(),
        headers: parts.headers,
        body,
        client_ip: client_ip(&parts.extensions),
        security,
        upgrade,
    };

    match crate::proxy::relay(&svc, &mut proxy_request).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// The caller's address, taken from the server-side connection info the platform layers
/// onto the request extensions.
#[must_use]
fn client_ip(extensions: &http::Extensions) -> String {
    if let Some(ip) = extensions.get::<std::net::SocketAddr>() {
        return ip.to_string();
    }
    String::new()
}

/// Splits `/proxy/{alias}/{*rest}` into its alias and the rest of the path.
#[must_use]
pub fn split_path(path: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix(PROXY_PREFIX)?;
    let rest = rest.strip_prefix('/')?;
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('/') {
        Some((alias, tail)) => Some((alias.to_owned(), format!("/{tail}"))),
        None => Some((rest.to_owned(), String::new())),
    }
}
