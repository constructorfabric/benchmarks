//! REST handler for the proxy path (DESIGN §3.5 "Proxy API").
//!
//! The handler is deliberately thin: it splits the request path into the alias
//! and the path to forward, validates the suffix it is about to forward, hands
//! everything to [`DataPlaneService`], and maps the resulting gateway error —
//! if any — onto the problem+json surface. No routing, no header logic and no
//! forwarding decision is taken here: those are domain decisions the REST
//! transport must not be able to bypass.
//!
//! One order is this handler's to decide, and ADR-0004 decides it: a CORS
//! preflight is answered *before* the caller is authenticated, because a browser
//! preflight carries no credentials (WHATWG Fetch) and a 401 would end the
//! conversation before it started. See [`DataPlaneService::preflight`].
//!
//! The one thing it *does* decide is the request's correlation id: the host
//! gateway sets no inbound correlation header, so one is generated per request
//! and carried through the audit record and every gateway error it produces
//! (DESIGN §4.3, ADR-0007 `trace_id`).

use uuid::Uuid;

use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::{Extension, OriginalUri, Path};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use toolkit_security::SecurityContext;

use crate::domain::headers::is_websocket_upgrade;
use crate::domain::routing::validate_path_suffix;
use crate::domain::services::data_plane::{DataPlaneService, ProxyRequest};
use crate::error::OagwError;
use crate::tenant_context::CallerContext;

/// The proxy path prefix, without the trailing alias segment.
pub(crate) const PROXY_PREFIX: &str = "/oagw/v1/proxy";

/// `ANY {METHOD} /oagw/v1/proxy/{alias}` — the alias itself, no sub-path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn proxy_alias(
    Extension(service): Extension<std::sync::Arc<DataPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    Path(alias): Path<String>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    body: Body,
) -> Response {
    proxy(
        service,
        security,
        alias,
        String::new(),
        uri,
        method,
        headers,
        upgrade,
        body,
    )
    .await
}

/// `ANY {METHOD} /oagw/v1/proxy/{alias}/{*path}` — the alias plus a sub-path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn proxy_alias_with_path(
    Extension(service): Extension<std::sync::Arc<DataPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    Path((alias, path)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    method: Method,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    body: Body,
) -> Response {
    proxy(
        service, security, alias, path, uri, method, headers, upgrade, body,
    )
    .await
}

/// The shared handler body.
#[allow(clippy::too_many_arguments)]
async fn proxy(
    service: std::sync::Arc<DataPlaneService>,
    security: Option<Extension<SecurityContext>>,
    alias: String,
    path: String,
    uri: http::Uri,
    method: Method,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    body: Body,
) -> Response {
    // One correlation id per request, minted before anything can fail: it is
    // stamped on the 400 below, on the 401 and on every gateway error the data
    // plane produces for this request, so a caller can correlate them.
    let request_id = new_request_id();
    let request_path = uri.path().to_owned();

    // The suffix as it was *sent*, still percent-encoded, and the suffix as the
    // `Path` extractor decoded it. They must describe the same segments, or the
    // request is rejected before any routing or dialing happens.
    let raw_path = raw_suffix(uri.path());

    if let Err(error) = validate_path_suffix(raw_path, &path) {
        return error
            .with_instance(request_path)
            .with_trace_id(request_id)
            .to_response();
    }

    // A preflight is answered *before* the caller is authenticated, and this
    // order is deliberate (ADR-0004 "Preflight Request Handling"): a browser
    // preflight carries no credentials (WHATWG Fetch), so requiring a
    // `SecurityContext` first would turn every cross-origin call into a 401
    // before CORS could ever be negotiated. The answer is data-free — it echoes
    // only the `Access-Control-Request-*` headers the caller sent — so an
    // unauthenticated caller learns nothing but that the gateway speaks CORS.
    // Origin and method are enforced on the actual request that follows, which
    // *is* authenticated.
    if let Some(answer) = service.preflight(&method, &headers) {
        return answer;
    }

    let Some(context) = security.as_ref() else {
        return unauthenticated(&request_path, request_id);
    };

    let caller = CallerContext::from(&context.0);
    if caller.tenant_id().is_none() {
        return unauthenticated(&request_path, request_id);
    }

    // A WebSocket handshake leaves the gateway through a different path, but it
    // is *detected* here, by the gateway's own rule, and not by the extractor:
    // `WebSocketUpgrade` also rejects requests that are merely shaped like an
    // upgrade, and a rejection must not be allowed to silently turn into the
    // ordinary proxy path — where `Connection` and `Upgrade` are stripped as the
    // hop-by-hop headers they are. The extractor is consulted only once the
    // gateway has decided this *is* an upgrade, so a handshake that is not
    // well formed is reported as the gateway's 400 (below) and never forwarded
    // as a bodyless `GET`.
    let handshake = if is_websocket_upgrade(&method, &headers) {
        Some(match upgrade {
            Ok(upgrade) => upgrade,
            Err(rejection) => return upgrade_rejected(&request_path, request_id, rejection),
        })
    } else {
        None
    };

    let request = ProxyRequest {
        alias,
        method,
        stripped_path: format!("/{path}"),
        query: uri.query().unwrap_or_default().to_owned(),
        headers,
        body,
        request_path,
        request_id,
    };

    // The upgrade path is entered *before* anything is returned to the caller:
    // it is the data plane that decides everything about it, and the response —
    // a signed `101` with the spliced socket, or a problem document — comes back
    // from there.
    let result = match handshake {
        Some(upgrade) => {
            service
                .proxy_upgrade(&context.0, &caller, request, upgrade)
                .await
        }
        None => service.proxy(&context.0, &caller, request).await,
    };

    match result {
        Ok(response) => response,
        Err(error) => render_gateway_error(error),
    }
}

/// The 400 for a request that *is* an upgrade but not a well-formed handshake.
///
/// RFC 6455 §4.1 lets a server reject a handshake with any error status, and the
/// gateway's error surface is problem+json — never the plain-text body axum's
/// rejection would render. The reason axum computed is carried as the problem
/// `detail`; it names a header the caller sent, so there is nothing in it that
/// is not already the caller's own request.
fn upgrade_rejected(
    request_path: &str,
    request_id: String,
    rejection: WebSocketUpgradeRejection,
) -> Response {
    OagwError::validation(format!(
        "the request is a WebSocket handshake, but not a well-formed one: {rejection}"
    ))
    .with_instance(request_path)
    .with_trace_id(request_id)
    .to_response()
}

/// The raw (percent-encoded) path suffix of a proxy request: everything after
/// the proxy prefix and the alias segment.
///
/// The `Path` extractor decodes what it captures, so an encoded separator
/// (`%2f`) reaches the domain as a segment boundary that the router never saw;
/// the raw request line is the only place that encoding is still observable.
fn raw_suffix(path: &str) -> &str {
    let rest = path.strip_prefix(PROXY_PREFIX).unwrap_or_default();
    let rest = rest.strip_prefix('/').unwrap_or_default();
    match rest.split_once('/') {
        Some((_alias, suffix)) => suffix,
        None => "",
    }
}

/// A fresh correlation id for the request.
///
/// There is no inbound correlation header to reuse (the host gateway sets
/// none), so one is generated per request; a later slice may propagate an
/// upstream `X-Request-ID` instead (DESIGN §3.1 request-id plugin).
fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

/// The 401 for a proxy request with no authenticated tenant (fail closed).
fn unauthenticated(request_path: &str, request_id: String) -> Response {
    OagwError::authentication_required()
        .with_instance(request_path)
        .with_trace_id(request_id)
        .to_response()
}

/// Render a gateway error from the data plane.
///
/// [`OagwError::to_response`] already emits `application/problem+json`, the
/// `X-OAGW-Error-Source: gateway` marker and any `Retry-After`; the data plane
/// has attached the `instance`, the `trace_id` and the `upstream_id` / `host` /
/// `path` extensions (DESIGN §3.3).
fn render_gateway_error(error: OagwError) -> Response {
    error.to_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proxy_prefix_is_the_documented_gear_relative_path() {
        assert_eq!(PROXY_PREFIX, "/oagw/v1/proxy");
        assert!(!PROXY_PREFIX.starts_with("/api"));
    }

    #[test]
    fn the_raw_suffix_is_taken_after_the_alias() {
        for (path, expected) in [
            ("/oagw/v1/proxy/api.openai.com/v1/models", "v1/models"),
            ("/oagw/v1/proxy/api.openai.com", ""),
            ("/oagw/v1/proxy/api.openai.com/", ""),
            ("/oagw/v1/proxy/vendor.com%3A8443/v1", "v1"),
            ("/oagw/v1/proxy/a%2Fb/v1", "v1"),
            ("/oagw/v1/proxy/v1/public%2fprivate", "public%2fprivate"),
            ("/oagw/v1/proxy/vendor.com/v1%2Fx", "v1%2Fx"),
            ("/other", ""),
            ("", ""),
        ] {
            assert_eq!(raw_suffix(path), expected, "raw suffix of {path}");
        }
    }

    #[test]
    fn a_traversal_in_the_raw_or_decoded_suffix_is_rejected() {
        for (raw, decoded) in [
            ("v1/public/../private", "/v1/public/../private"),
            ("v1/%2e%2e/private", "/v1/../private"),
            ("v1/public%2fprivate", "/v1/public/private"),
            ("v1//private", "/v1//private"),
        ] {
            let error = validate_path_suffix(raw, decoded).expect_err(raw);
            assert_eq!(error.status().as_u16(), 400, "{raw}");
            assert_eq!(error.kind(), crate::error::OagwErrorKind::ValidationError);
        }

        assert!(
            validate_path_suffix("v1/models", "/v1/models").is_ok(),
            "an ordinary path suffix is forwarded"
        );
    }

    #[test]
    fn every_request_gets_a_correlation_id() {
        let first = new_request_id();
        let second = new_request_id();

        assert!(!first.is_empty());
        assert_ne!(first, second, "ids are per request");
        assert!(
            Uuid::parse_str(&first).is_ok(),
            "the id is a UUID, so a caller can correlate it"
        );
    }
}
