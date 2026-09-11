//! REST handler for the proxy data plane.

use std::sync::Arc;

use axum::Extension;
use axum::extract::Request;
use axum::http::Uri;
use axum::response::Response;
use toolkit_security::SecurityContext;
use tracing::field::Empty;

use crate::api::error::{ApiContext, proxy_problem};
use crate::domain::error::DomainError;
use crate::infra::proxy::service::{ProxyRequest, ProxyService};
use crate::infra::tenant_chain;

/// Prefix under which the proxy endpoint is mounted.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// Router path of the proxy endpoint, including the catch-all suffix.
pub const PROXY_ROUTE: &str = "/oagw/v1/proxy/{*rest}";

/// The proxy endpoint: `{METHOD} /oagw/v1/proxy/{alias}[/{path}][?{query}]`.
///
/// Every method is funnelled into one handler so regular calls, CORS
/// preflights and WebSocket upgrades share the same resolution and error
/// semantics.
///
/// # Errors
/// Returns a problem document for every gateway-generated failure.
#[tracing::instrument(skip(ctx, security, request), fields(request_id = Empty))]
pub async fn proxy(
    Extension(ctx): Extension<Arc<ApiContext>>,
    Extension(security): Extension<SecurityContext>,
    uri: Uri,
    mut request: Request,
) -> Response {
    let Some(proxy_path) = uri.path().strip_prefix(PROXY_PREFIX) else {
        return proxy_problem(&DomainError::RouteNotFound, uri.path());
    };
    // Claimed before the request is consumed: hyper resolves the upgrade only
    // once the 101 response has been handed back to the server.
    let client_upgrade = hyper::upgrade::on(&mut request);
    let (parts, body) = request.into_parts();
    if crate::infra::proxy::cors::is_preflight(&parts.method, &parts.headers) {
        tracing::debug!(path = proxy_path, "answering the CORS preflight locally");
        return axum::response::IntoResponse::into_response(
            crate::infra::proxy::cors::preflight_response(&parts.headers, None),
        );
    }
    let tenant_id = security.subject_tenant_id();
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ancestors = tenant_chain::ancestors_of(ctx.tenants.as_ref(), &security, tenant_id).await;
    let upstream = ProxyRequest {
        method: parts.method.clone(),
        proxy_path: proxy_path.to_owned(),
        query: uri.query().map(str::to_owned),
        headers: parts.headers.clone(),
        body,
        security: Arc::new(security.clone()),
        tenant_id,
        remote_ip: remote_ip_of(&parts.headers),
        trace,
        ancestors,
    };
    match proxy_call(&ctx.proxy, upstream).await {
        Ok(response) => start_relay(response, client_upgrade),
        Err(err) => {
            tracing::debug!(error = %err, "proxy request rejected by the gateway");
            proxy_problem(&err, uri.path())
        }
    }
}

/// Hand the upgraded client socket to the relay once the upstream answered.
fn start_relay(
    mut response: axum::response::Response,
    client_upgrade: hyper::upgrade::OnUpgrade,
) -> axum::response::Response {
    let Some(handle) = response
        .extensions_mut()
        .remove::<crate::infra::proxy::tunnel::TunnelHandle>()
    else {
        return response;
    };
    let Some(stream) = handle.take() else {
        return response;
    };
    tokio::spawn(async move {
        if let Ok(client) = client_upgrade.await {
            crate::infra::proxy::tunnel::relay(client, stream).await;
        }
    });
    response
}

/// Run the data plane, collapsing `ProxyService::handle` behind one call site
/// so the handler stays small.
async fn proxy_call(
    proxy: &ProxyService,
    upstream: ProxyRequest,
) -> Result<axum::response::Response, DomainError> {
    proxy.handle(upstream).await
}

/// Best-effort caller address used by the `ip` rate-limit scope.
#[must_use]
pub fn remote_ip_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(axum::http::header::FORWARDED)
        .and_then(|value| value.to_str().ok())
        .map_or_else(|| "0.0.0.0".to_owned(), str::to_owned)
}

#[allow(dead_code, reason = "diagnostic only")]
fn assert_extractors() {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<Arc<crate::api::error::ApiContext>>();
    assert_send_sync::<SecurityContext>();
}
