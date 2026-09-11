//! The data-plane handler: `{METHOD} /oagw/v1/proxy/{alias}[/{path}]`.
//!
//! Every method on these paths lands in one handler, so the gateway never
//! answers `405` for a method a route's allowlist rejects — route matching
//! decides, and answers `404` when it disagrees.
//!
//! A WebSocket upgrade is two forwards: the request head is sent to the
//! upstream first and the caller is answered with `101` only once the upstream
//! has agreed, then the two legs are bridged. Answering `101` before the
//! upstream agreed would strand the client on a tunnel nobody speaks on.

use std::sync::Arc;

use axum::Extension;
use axum::extract::{FromRequestParts, Path, Request};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;

use crate::api::rest::error::GatewayProblem;
use crate::api::rest::extractors::AuthenticatedSubject;
use crate::api::rest::extractors::require_proxy;
use crate::domain::model::Cors;
use crate::domain::services::data_plane::{
    DataPlaneService, DuplexStream, GatewayFailure, ProxyPlan,
};
use crate::infra::plugin::cors;
use crate::infra::proxy::outbound::TARGET_HOST_HEADER;
use crate::infra::proxy::service::GatewayService;

/// Header carrying the caller's correlation identifier, echoed on errors.
const TRACE_ID_HEADER: &str = "x-request-id";

/// Header naming the gateway as the source of the answer.
const ERROR_SOURCE: &str = "x-oagw-error-source";

/// `{METHOD} /oagw/v1/proxy/{alias}` — an exchange that ends at the alias.
pub async fn proxy_root(
    Extension(svc): Extension<Arc<GatewayService>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    exchange(svc, alias, String::new(), request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path}` — an exchange with a remainder.
pub async fn proxy_path(
    Extension(svc): Extension<Arc<GatewayService>>,
    Path((alias, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    // The wildcard capture omits the slash that separates it from the alias,
    // while routes are registered as absolute paths; restore it so the data
    // plane matches against the request path the caller actually sent.
    let path = if path.is_empty() || path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    exchange(svc, alias, path, request).await
}

async fn exchange(
    svc: Arc<GatewayService>,
    alias: String,
    path: String,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    // A preflight is answered before anything else: a browser sends no
    // credentials on one (WHATWG Fetch), so there is usually no subject and no
    // tenant to resolve a route with. Echoing what was asked for is permissive
    // by design — origin and method enforcement happens on the actual request
    // that follows, which does carry credentials (ADR 0004).
    if cors::is_preflight(&parts.method, &parts.headers) {
        let policy = preflight_policy(&svc, &mut parts, &alias, &path).await;
        return cors::preflight_answer(policy.as_ref(), &parts.headers).into_response();
    }
    let subject = match AuthenticatedSubject::from_request_parts(&mut parts, &()).await {
        Ok(extracted) => extracted.into_context(),
        Err(problem) => return problem.into_response(),
    };
    // The proxy is the gateway's whole point, so it carries its own permission:
    // a principal that may configure the gateway is not thereby allowed to send
    // traffic through it.
    if let Err(error) = require_proxy(&subject) {
        return problem(error, &instance_for(&alias, &path), None).into_response();
    }

    let target_host = header_value(&parts.headers, TARGET_HOST_HEADER);
    let trace_id = header_value(&parts.headers, TRACE_ID_HEADER);
    let method = parts.method.as_str().to_owned();
    let wants_upgrade = GatewayService::is_upgrade(&parts.headers);
    let instance = instance_for(&alias, &path);

    let plan = match svc
        .plan(&subject, &method, &alias, &path, target_host.as_deref())
        .await
    {
        Ok(plan) => plan,
        Err(error) => return problem(error, &instance, trace_id.as_deref()).into_response(),
    };

    if wants_upgrade {
        return upgrade(svc, subject, parts, plan, &instance, trace_id.as_deref()).await;
    }

    // CORS is the gateway's own business: an actual cross-origin request the
    // configuration does not allow never leaves the building (ADR 0004). The
    // preflight that preceded it was answered at the top of this handler, where
    // no tenant context is needed. The policy is configured per route; an
    // upstream carries none of its own.
    let policy = plan.route.cors.as_ref();
    let origin = header_value(&parts.headers, cors::ORIGIN);
    if let Some(config) = policy
        && cors::is_cross_origin(&parts.headers)
        && let Err(error) = cors::check_request(config, &parts.method, &parts.headers)
    {
        return problem(error, &instance, trace_id.as_deref()).into_response();
    }

    let request = http::Request::from_parts(parts, body);
    let mut response = match svc.forward(&subject, &plan, request).await {
        Ok(response) => response,
        Err(failure) => return render(failure, &instance, trace_id.as_deref()),
    };
    if let Some(config) = policy {
        cors::apply_response_headers(config, origin.as_deref(), response.headers_mut());
    }
    response
}

/// The CORS configuration a preflight is answered with, when it can be read.
///
/// Resolving a route needs a tenant, and a browser's preflight presents no
/// credential, so the common case is `None` and the answer is permissive. A
/// caller that did present a usable subject gets the operator's own policy
/// back — its max age and its credentials flag — which is all that is known
/// without touching an upstream.
async fn preflight_policy(
    svc: &GatewayService,
    parts: &mut http::request::Parts,
    alias: &str,
    path: &str,
) -> Option<Cors> {
    let subject = AuthenticatedSubject::from_request_parts(parts, &())
        .await
        .ok()?
        .into_context();
    let method = parts.method.as_str().to_owned();
    let target_host = header_value(&parts.headers, TARGET_HOST_HEADER);
    svc.plan(&subject, &method, alias, path, target_host.as_deref())
        .await
        .ok()?
        .route
        .cors
}

/// Forward an upgrade request and bridge the two legs once the upstream agrees.
async fn upgrade(
    svc: Arc<GatewayService>,
    subject: toolkit_security::SecurityContext,
    parts: http::request::Parts,
    plan: ProxyPlan,
    instance: &str,
    trace_id: Option<&str>,
) -> Response {
    // Reassemble the request so hyper can hand us the client's half of the
    // tunnel; the head itself travels to the upstream.
    let mut request = http::Request::from_parts(parts, axum::body::Body::empty());
    let on_upgrade = hyper::upgrade::on(&mut request);
    let (head, _) = request.into_parts();

    let handshake = match svc.open_tunnel(&subject, &plan, &head).await {
        Ok(handshake) => handshake,
        Err(error) => return problem(error, instance, trace_id).into_response(),
    };

    let Some(upstream) = handshake.stream else {
        // The upstream refused the upgrade; its answer stands, with the
        // error-source header marking it as an upstream response.
        return http::Response::builder()
            .status(handshake.status)
            .header(ERROR_SOURCE, "upstream")
            .body(axum::body::Body::empty())
            .map_or_else(
                |_| http::StatusCode::BAD_GATEWAY.into_response(),
                |response| stamp_headers(response, &handshake.headers),
            );
    };

    let response = http::Response::builder()
        .status(http::StatusCode::SWITCHING_PROTOCOLS)
        .body(axum::body::Body::empty())
        .map_or_else(
            |_| http::StatusCode::BAD_GATEWAY.into_response(),
            |response| stamp_headers(response, &handshake.headers),
        );
    tokio::spawn(bridge(svc, on_upgrade, upstream));
    response
}

/// Copy `headers` onto `response`, keeping the built-in ones when a value is
/// not representable.
fn stamp_headers(
    mut response: http::Response<axum::body::Body>,
    headers: &http::HeaderMap,
) -> Response {
    let target = response.headers_mut();
    for (name, value) in headers {
        target.insert(name.clone(), value.clone());
    }
    response
}

/// Bridge the caller's upgraded connection to the upstream's.
async fn bridge(
    svc: Arc<GatewayService>,
    on_upgrade: hyper::upgrade::OnUpgrade,
    upstream: DuplexStream,
) {
    let client: DuplexStream = match on_upgrade.await {
        Ok(upgraded) => Box::new(TokioIo::new(upgraded)),
        Err(_) => return,
    };
    if let Err(error) = svc.bridge(client, upstream).await {
        tracing::warn!(error = %error, "websocket bridge aborted");
    }
}

/// Render a data-plane failure as the problem document the chain produced.
fn render(failure: GatewayFailure, instance: &str, trace_id: Option<&str>) -> Response {
    problem(failure.error, instance, trace_id)
        .with_headers(failure.headers)
        .into_response()
}

fn problem(
    error: crate::domain::error::DomainError,
    instance: &str,
    trace_id: Option<&str>,
) -> GatewayProblem {
    let mut document = GatewayProblem::new(error).with_instance(instance.to_owned());
    if let Some(trace_id) = trace_id {
        document = document.with_trace_id(trace_id.to_owned());
    }
    document
}

fn header_value(headers: &http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

fn instance_for(alias: &str, path: &str) -> String {
    let remainder = path.trim_start_matches('/');
    if remainder.is_empty() {
        format!("/oagw/v1/proxy/{alias}")
    } else {
        format!("/oagw/v1/proxy/{alias}/{remainder}")
    }
}
