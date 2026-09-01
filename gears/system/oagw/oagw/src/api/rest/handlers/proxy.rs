//! Proxy handler — data plane entry point (DESIGN §5).
//!
//! One wildcard route (`any`) handles every verb on
//! `/oagw/v1/proxy/{alias}/{*suffix}`:
//!
//! - CORS preflights (OPTIONS + Origin + `Access-Control-Request-Method`)
//!   are answered locally with a permissive 204 echo (ADR-0004);
//! - WebSocket upgrades are bridged to the upstream after the request-side
//!   plugin chain runs (DESIGN §3.4);
//! - everything else is forwarded through [`DataPlaneService::execute`].
//!
//! [`DataPlaneService::execute`]: crate::domain::services::data_plane::DataPlaneService::execute

use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, RawPathParams, Request};
use axum::http::header::ORIGIN;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;

use crate::domain::dto::{CorsConfig, SharingMode};
use crate::domain::error::{DomainError, ProblemContext};
use crate::domain::services::data_plane::{
    DataPlaneService, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, ProxyRequest,
};
use crate::domain::services::management::ControlPlaneService;
use crate::infra::proxy::cors::{effective_cors, is_preflight, preflight_response};
use crate::infra::proxy::is_body_overflow;
use crate::infra::proxy::websocket::{bridge, connect_upstream};

use super::problem;

/// Fallback CORS config for preflights when the target (or its CORS config)
/// cannot be resolved — permissive echo per ADR-0004, so a preflight never
/// fails because the target is temporarily unresolvable.
fn permissive_preflight_config() -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Inherit,
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

/// Distinguishes a WebSocket upgrade from a plain HTTP exchange.
///
/// axum's [`WebSocketUpgrade`] extractor rejects non-upgrade requests, so we
/// attempt it first and fall back to the HTTP path when it declines — this
/// keeps one wildcard route for every verb.
pub enum MaybeUpgrade {
    /// The request carries a valid WebSocket upgrade.
    Upgrade(WebSocketUpgrade),
    /// All other requests.
    Plain,
}

impl<S: Send + Sync> FromRequestParts<S> for MaybeUpgrade {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match WebSocketUpgrade::from_request_parts(parts, state).await {
            Ok(ws) => Ok(Self::Upgrade(ws)),
            Err(_) => Ok(Self::Plain),
        }
    }
}

/// Proxy exchange entry point.
#[allow(clippy::too_many_arguments)]
pub async fn proxy_handler(
    axum::Extension(data_plane): axum::Extension<Arc<DataPlaneService>>,
    axum::Extension(control): axum::Extension<Arc<ControlPlaneService>>,
    axum::Extension(security): axum::Extension<SecurityContext>,
    raw_path_params: RawPathParams,
    kind: MaybeUpgrade,
    request: Request<Body>,
) -> Response {
    // Captured via `RawPathParams` so the bare-alias form (`/proxy/{alias}`,
    // no `{*suffix}`) and the suffixed form share this handler.
    let alias = raw_param(&raw_path_params, "alias");
    let suffix = raw_param(&raw_path_params, "suffix");
    let path = proxy_path(&suffix);
    let method = request.method().clone();
    let headers = request.headers().clone();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let client_ip = client_ip(&headers);
    let instance = request.uri().path().to_owned();
    let tenant_id = super::tenant_of(&security);

    // CORS preflight (ADR-0004) — always answered locally with a permissive
    // 204 echo. Resolution is best-effort: a preflight must not fail because
    // the target is temporarily unresolvable, and with no CORS config (or a
    // disabled one) the permissive default is echoed.
    if is_preflight(&method, &headers) {
        let origin = header_str(&headers, ORIGIN.as_str()).unwrap_or("*");
        let requested_method =
            header_str(&headers, "access-control-request-method").unwrap_or("GET");
        let requested_headers = header_str(&headers, "access-control-request-headers");
        let resolved = control
            .resolve_proxy_target(&security, tenant_id, &alias, method.as_str(), &path)
            .await
            .ok()
            .and_then(|target| effective_cors(&target.upstream, &target.route).cloned())
            .filter(|c| c.enabled);
        let fallback = permissive_preflight_config();
        let cors_cfg = resolved.as_ref().unwrap_or(&fallback);
        return preflight_response(cors_cfg, origin, requested_method, requested_headers)
            .into_response();
    }

    match kind {
        MaybeUpgrade::Upgrade(ws) => {
            let proxy_req = ProxyRequest {
                security_context: security.clone(),
                tenant_id,
                alias: alias.clone(),
                path: path.clone(),
                method: method.clone(),
                headers: headers.clone(),
                query,
                body: bytes::Bytes::new(),
                client_ip,
            };
            let target = match data_plane.prepare_websocket(&security, &proxy_req).await {
                Ok(target) => target,
                Err(err) => return problem(&err, &instance).into_response(),
            };
            let ws_url = target.url;
            let upstream_headers = target.headers;
            let mut response = ws.on_upgrade(move |socket| async move {
                let Ok(upstream) = connect_upstream(&ws_url, &upstream_headers).await else {
                    return;
                };
                bridge(socket, upstream).await;
            });
            response.headers_mut().insert(
                ERROR_SOURCE_HEADER,
                HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
            );
            response
        }
        MaybeUpgrade::Plain => {
            let body = match axum::body::to_bytes(request.into_body(), data_plane.max_body_bytes())
                .await
            {
                Ok(bytes) => bytes,
                Err(err) if is_body_overflow(&err) => {
                    return problem(
                        &DomainError::PayloadTooLarge {
                            detail: format!(
                                "request body exceeds {} bytes",
                                data_plane.max_body_bytes()
                            ),
                            context: Some(ProblemContext {
                                path: Some(instance.clone()),
                                ..ProblemContext::new()
                            }),
                        },
                        &instance,
                    )
                    .into_response();
                }
                Err(err) => {
                    return problem(
                        &DomainError::StreamAborted {
                            detail: format!("failed to buffer request body: {err}"),
                            context: Some(ProblemContext {
                                path: Some(instance.clone()),
                                ..ProblemContext::new()
                            }),
                        },
                        &instance,
                    )
                    .into_response();
                }
            };

            let proxy_req = ProxyRequest {
                security_context: security.clone(),
                tenant_id,
                alias,
                path,
                method,
                headers,
                query,
                body,
                client_ip,
            };
            match data_plane.execute(&security, &proxy_req).await {
                Ok(exchange) => exchange.response,
                Err(err) => problem(&err, &instance).into_response(),
            }
        }
    }
}

/// The proxy-visible path: the wildcard suffix joined under the root.
fn proxy_path(suffix: &str) -> String {
    if suffix.is_empty() {
        "/".to_owned()
    } else {
        format!("/{suffix}")
    }
}

/// Look up one raw path parameter by name (`""` when absent).
fn raw_param(params: &RawPathParams, name: &str) -> String {
    params
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_owned())
        .unwrap_or_default()
}

/// Client IP for `scope: ip` rate limiting — `X-Forwarded-For` first hop,
/// then `X-Real-IP`.
fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .and_then(|v| v.parse::<IpAddr>().ok())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<IpAddr>().ok())
        })
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}
