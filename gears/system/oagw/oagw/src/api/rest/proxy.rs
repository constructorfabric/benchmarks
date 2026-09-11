//! The data plane: `{METHOD} /oagw/v1/proxy/{alias}` and `/{alias}/{*path}`.
//!
//! Realizes `cpt-cf-oagw-flow-ph-proxy-request` and, on the same surface,
//! `cpt-cf-oagw-flow-ps-sse-relay` and `cpt-cf-oagw-flow-ps-ws-upgrade`.
//!
//! Plain exchanges, server-sent-event streams and WebSocket upgrades all take
//! the same path. Nothing here buffers a body, so an event stream reaches the
//! client incrementally; a `101` hands both sides' upgraded transports to a
//! byte relay, so frames, subprotocols and close codes pass through untouched.

use std::sync::Arc;

use axum::extract::{Extension, OriginalUri, Request};
use axum::response::{IntoResponse, Response};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, StatusCode};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;

use crate::domain::error::{DomainError, ERROR_SOURCE_HEADER, ErrorSource, TARGET_HOST_HEADER};
use crate::domain::model::{Scheme, Upstream};
use crate::domain::tenant::Caller;
use crate::domain::{cors, plugins, ratelimit, routing, validate};
use crate::gear::OagwState;
use crate::infra::body::{self, ProxyBody};
use crate::infra::connect;
use crate::infra::headers as hdr;

/// The path prefix the proxy is mounted at.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy";

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// Stamp the gateway error-source header onto a response.
fn mark_source(resp: &mut Response, source: ErrorSource) {
    if let Ok(v) = HeaderValue::from_str(source.as_str()) {
        resp.headers_mut().insert(
            HeaderName::from_static("x-oagw-error-source"),
            v,
        );
    }
}

/// A gateway-produced error response, always marked `gateway`.
///
/// A rate-limit rejection additionally carries a wire `Retry-After` header: the
/// canonical error builder only promotes a retry hint to that header for the
/// service-unavailable category, so a 429 needs it set here.
fn gateway_error(e: DomainError) -> Response {
    let retry_after = match &e {
        DomainError::RateLimited { retry_after_secs } => Some(*retry_after_secs),
        _ => None,
    };
    let mut resp = CanonicalError::from(e).into_response();
    mark_source(&mut resp, ErrorSource::Gateway);
    if let Some(secs) = retry_after
        && let Ok(v) = HeaderValue::from_str(&secs.to_string())
    {
        resp.headers_mut()
            .insert(HeaderName::from_static("retry-after"), v);
    }
    resp
}

/// The proxy entry point.
///
/// Returns a `Response` directly rather than `ApiResult` so that every exit —
/// including the error exits — carries `X-OAGW-Error-Source`.
pub(crate) async fn proxy(
    Extension(state): Extension<Arc<OagwState>>,
    sec: Option<Extension<SecurityContext>>,
    OriginalUri(uri): OriginalUri,
    req: Request,
) -> Response {
    match proxy_inner(state, sec, uri, req).await {
        Ok(r) => r,
        Err(e) => gateway_error(e),
    }
}

#[allow(clippy::too_many_lines, reason = "one linear request path, kept together for readability")]
async fn proxy_inner(
    state: Arc<OagwState>,
    sec: Option<Extension<SecurityContext>>,
    uri: axum::http::Uri,
    req: Request,
) -> Result<Response, DomainError> {
    let caller = Caller::from_context(sec.as_ref().map(|e| &e.0));
    let method = req.method().clone();
    let inbound_headers = req.headers().clone();

    // The alias and suffix come from the original URI so a gateway prefix, if
    // one is configured, does not leak into the match.
    let path = uri.path();
    let rest = path
        .find(PROXY_PREFIX)
        .map_or(path, |i| &path[i + PROXY_PREFIX.len()..]);
    let (alias, suffix) = routing::split_alias_and_suffix(rest);

    // The client address for an `ip`-scoped rate limit: the left-most entry of
    // a forwarded-for header when the edge supplies one, else the peer address
    // the host runtime attached.
    let client_ip = client_address(&inbound_headers, &req);

    let origin = header_str(&inbound_headers, "origin").map(str::to_owned);
    let acrm = header_str(&inbound_headers, "access-control-request-method").map(str::to_owned);

    // A CORS preflight is answered before upstream resolution, without auth or
    // plugin evaluation.
    // @cpt-begin:cpt-cf-oagw-dod-tp-cors-preflight:p1:inst-full
    if cors::is_preflight(method.as_str(), origin.as_deref(), acrm.as_deref()) {
        let (Some(o), Some(m)) = (origin.as_deref(), acrm.as_deref()) else {
            return Err(DomainError::Internal {
                message: "preflight detected without its own headers".to_owned(),
            });
        };
        let acrh = header_str(&inbound_headers, "access-control-request-headers");
        let mut resp = StatusCode::NO_CONTENT.into_response();
        for (k, v) in cors::preflight_headers(&cors::Preflight {
            origin: o,
            method: m,
            headers: acrh,
        }) {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v))
            {
                resp.headers_mut().insert(name, value);
            }
        }
        mark_source(&mut resp, ErrorSource::Gateway);
        return Ok(resp);
    }
    // @cpt-end:cpt-cf-oagw-dod-tp-cors-preflight:p1:inst-full

    if alias.is_empty() {
        return Err(DomainError::validation("alias", "a proxy alias is required"));
    }

    // @cpt-begin:cpt-cf-oagw-dod-ph-alias-resolution:p1:inst-full
    let up = state
        .store
        .find_upstream_by_alias(caller.tenant_id, &alias)
        .ok_or_else(|| DomainError::not_found("upstream", alias.clone()))?;
    if !up.enabled {
        return Err(DomainError::Unavailable {
            message: format!("upstream `{alias}` is disabled"),
        });
    }
    // @cpt-end:cpt-cf-oagw-dod-ph-alias-resolution:p1:inst-full

    // The chain order is: resolve the upstream and the route, charge the rate
    // limit, then evaluate CORS, then the guard chain. Resolution comes first
    // so a request that matches no route is reported as such rather than being
    // pre-empted by a CORS verdict, and the bucket is charged for every request
    // that got as far as a matched route.
    let matched = routing::match_route(
        &state.store.routes_for_upstream(up.id),
        method.as_str(),
        &suffix,
    )?;

    // Policy is evaluated once, before the exchange is established, and the
    // bucket is charged once for the establishing request.
    let rl_decision = charge_rate_limit(&state, &up, &matched, &caller, client_ip.as_deref())?;

    // An actual cross-origin request is screened once the upstream, the route
    // and the rate limit have been settled.
    let mut cors_response_headers: Vec<(&'static str, String)> = Vec::new();
    if let (Some(cfg), Some(o)) = (up.cors.as_ref(), origin.as_deref())
        && cfg.enabled
    {
        match cors::evaluate_actual(cfg, o, method.as_str()) {
            Ok(h) => cors_response_headers = h,
            Err(rej) => {
                let mut resp = (
                    StatusCode::FORBIDDEN,
                    axum::Json(serde_json::json!({
                        "type": rej.problem_type(),
                        "title": "Forbidden",
                        "status": 403,
                        "detail": rej.detail(),
                    })),
                )
                    .into_response();
                mark_source(&mut resp, ErrorSource::Gateway);
                apply_rate_limit_headers(&mut resp, rl_decision.as_ref());
                return Ok(resp);
            }
        }
    }

    // Guards run against the request before anything is forwarded.
    let required_request = required_headers(&up, &matched, "required_request_headers");
    if let Some(missing) = plugins::first_missing_header(&required_request, &header_names(&inbound_headers))
    {
        return Err(DomainError::validation(
            missing.clone(),
            "REQUIRED_HEADER_MISSING",
        ));
    }

    let target_host = header_str(&inbound_headers, TARGET_HOST_HEADER).map(str::to_owned);
    let endpoint = routing::select_endpoint(&up, target_host.as_deref(), &state.round_robin)?
        .clone();

    // Scheme enforcement governs the connection only, never create-time
    // acceptance of the scheme value.
    // @cpt-begin:cpt-cf-oagw-dod-ph-scheme-enforcement:p1:inst-full
    if !validate::connection_permitted(endpoint.scheme, state.config.allow_http_upstream) {
        return Err(DomainError::UpstreamUnreachable {
            message: format!(
                "a plaintext `{}` connection is not permitted; \
                 set allow_http_upstream to enable it",
                endpoint.scheme.url_scheme()
            ),
        });
    }
    // @cpt-end:cpt-cf-oagw-dod-ph-scheme-enforcement:p1:inst-full

    // WebTransport is accepted as a scheme value but is deliberately not served.
    if endpoint.scheme == Scheme::Wt {
        return Err(DomainError::NotImplemented {
            message: "WebTransport upstreams are not served in this configuration".to_owned(),
        });
    }

    if body::declared_length_exceeds_cap(&inbound_headers) {
        return Err(DomainError::PayloadTooLarge);
    }

    let is_upgrade = is_websocket_upgrade(&inbound_headers);
    let authority = endpoint.authority();
    let http_match = matched.route.match_.http.as_ref();
    let query = routing::filter_query(
        http_match.map_or(&[][..], |h| h.query_allowlist.as_slice()),
        uri.query(),
    );
    let target = match query {
        Some(q) => format!("{}?{q}", matched.target_path),
        None => matched.target_path.clone(),
    };

    let out_headers = hdr::build_request_headers(
        &inbound_headers,
        &up.headers.request,
        &authority,
        is_upgrade,
    );

    if is_upgrade {
        return upgrade_exchange(state, req, endpoint.scheme, &endpoint.host, endpoint.effective_port(), &target, out_headers).await;
    }

    // ---- ordinary exchange, including event streams --------------------
    let mut conn = tokio::time::timeout(
        state.config.proxy_timeout(),
        connect::Upstream::connect(endpoint.scheme, &endpoint.host, endpoint.effective_port()),
    )
    .await
    .map_err(|_| DomainError::UpstreamTimeout)??;

    let mut builder = hyper::Request::builder().method(method.clone()).uri(&target);
    if let Some(h) = builder.headers_mut() {
        *h = out_headers;
    }
    let outbound = builder
        .body(ProxyBody::from_axum(req.into_body()))
        .map_err(|e| DomainError::Internal {
            message: e.to_string(),
        })?;

    // The timeout bounds establishing the exchange and receiving the response
    // headers. It deliberately does NOT bound the lifetime of the body that
    // follows, so an event stream is not cut off at proxy_timeout_secs.
    // @cpt-begin:cpt-cf-oagw-dod-ps-timeout-scope:p1:inst-full
    let upstream_resp = tokio::time::timeout(state.config.proxy_timeout(), conn.send(outbound))
        .await
        .map_err(|_| DomainError::UpstreamTimeout)??;
    // @cpt-end:cpt-cf-oagw-dod-ps-timeout-scope:p1:inst-full

    // A response that declares an over-cap length is refused before its status
    // line is relayed; an undeclared one can only be cut off mid-stream, which
    // the counting body does.
    if body::declared_length_exceeds_cap(upstream_resp.headers()) {
        return Err(DomainError::PayloadTooLarge);
    }

    let status = upstream_resp.status();
    let relay_headers =
        hdr::build_response_headers(upstream_resp.headers(), &up.headers.response, false);

    // Guards run against the upstream's response headers; a missing required
    // response header is a 502, distinct from the request phase's 400.
    let required_response = required_headers(&up, &matched, "required_response_headers");
    if let Some(missing) = plugins::first_missing_header(&required_response, &header_names(&relay_headers))
    {
        return Err(DomainError::UpstreamUnreachable {
            message: format!("REQUIRED_HEADER_MISSING: {missing}"),
        });
    }

    let mut resp = Response::builder().status(status);
    if let Some(h) = resp.headers_mut() {
        *h = relay_headers;
    }
    let mut resp = resp
        .body(ProxyBody::from_incoming(upstream_resp.into_body()).into_axum())
        .map_err(|e| DomainError::Internal {
            message: e.to_string(),
        })?;

    // The upstream's own status is relayed unchanged, and it is the upstream
    // that is named as the source even when that status is a 4xx or 5xx.
    mark_source(&mut resp, ErrorSource::Upstream);
    apply_extra_headers(&mut resp, &cors_response_headers);
    apply_rate_limit_headers(&mut resp, rl_decision.as_ref());
    Ok(resp)
}

/// Relay a WebSocket upgrade.
///
/// After both sides answer `101`, the two upgraded transports are copied into
/// each other. The gateway never parses a frame, so subprotocol negotiation,
/// ping/pong and close codes propagate untouched, and either side closing tears
/// the other down.
// @cpt-begin:cpt-cf-oagw-dod-ps-ws-bidirectional-frame-relay:p1:inst-full
async fn upgrade_exchange(
    state: Arc<OagwState>,
    req: Request,
    scheme: Scheme,
    host: &str,
    port: u16,
    target: &str,
    out_headers: HeaderMap,
) -> Result<Response, DomainError> {
    let mut conn = tokio::time::timeout(
        state.config.proxy_timeout(),
        connect::Upstream::connect(scheme, host, port),
    )
    .await
    .map_err(|_| DomainError::UpstreamTimeout)??;

    let (parts, incoming_body) = req.into_parts();
    let client_on_upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();

    let mut builder = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(target);
    if let Some(h) = builder.headers_mut() {
        *h = out_headers;
    }
    let outbound = builder
        .body(ProxyBody::from_axum(incoming_body))
        .map_err(|e| DomainError::Internal {
            message: e.to_string(),
        })?;

    let upstream_resp = tokio::time::timeout(state.config.proxy_timeout(), conn.send(outbound))
        .await
        .map_err(|_| DomainError::UpstreamTimeout)??;

    let status = upstream_resp.status();
    let relay_headers = hdr::build_response_headers(upstream_resp.headers(), &Default::default(), true);

    if status != StatusCode::SWITCHING_PROTOCOLS {
        // The upstream refused the upgrade; its own status is relayed rather
        // than a synthetic one.
        let mut resp = Response::builder().status(status);
        if let Some(h) = resp.headers_mut() {
            *h = relay_headers;
        }
        let mut resp = resp
            .body(ProxyBody::from_incoming(upstream_resp.into_body()).into_axum())
            .map_err(|e| DomainError::Internal {
                message: e.to_string(),
            })?;
        mark_source(&mut resp, ErrorSource::Upstream);
        return Ok(resp);
    }

    // The upstream has switched protocols. Only answer 101 if the client side
    // can actually be upgraded too — otherwise the caller would be told the
    // protocol switched while no relay ever starts.
    let Some(client_on_upgrade) = client_on_upgrade else {
        drop(hyper::upgrade::on(upstream_resp));
        return Err(DomainError::UpstreamUnreachable {
            message: "the client connection cannot be upgraded, so the accepted \
                      upstream upgrade cannot be relayed"
                .to_owned(),
        });
    };

    let upstream_on_upgrade = hyper::upgrade::on(upstream_resp);
    {
        tokio::spawn(async move {
            let (client, upstream) =
                match tokio::try_join!(client_on_upgrade, upstream_on_upgrade) {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::debug!(error = %e, "oagw websocket upgrade failed");
                        return;
                    }
                };
            let mut client = hyper_util::rt::TokioIo::new(client);
            let mut upstream = hyper_util::rt::TokioIo::new(upstream);
            if let Err(e) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                tracing::debug!(error = %e, "oagw websocket relay ended");
            }
        });
    }

    let mut resp = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    if let Some(h) = resp.headers_mut() {
        *h = relay_headers;
    }
    let mut resp = resp
        .body(axum::body::Body::empty())
        .map_err(|e| DomainError::Internal {
            message: e.to_string(),
        })?;
    mark_source(&mut resp, ErrorSource::Upstream);
    Ok(resp)
}
// @cpt-end:cpt-cf-oagw-dod-ps-ws-bidirectional-frame-relay:p1:inst-full

/// Whether the inbound request asks for a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(h: &HeaderMap) -> bool {
    let upgrade_is_ws = header_str(h, "upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let connection_has_upgrade = header_str(h, "connection").is_some_and(|v| {
        v.split(',')
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
    });
    upgrade_is_ws && connection_has_upgrade
}

/// The caller's address, for an `ip`-scoped rate limit.
fn client_address(h: &HeaderMap, req: &Request) -> Option<String> {
    if let Some(first) = header_str(h, "x-forwarded-for")
        .and_then(|fwd| fwd.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(first.to_owned());
    }
    if let Some(real) = header_str(h, "x-real-ip")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(real.to_owned());
    }
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
}

fn header_names(h: &HeaderMap) -> Vec<String> {
    h.keys().map(|k| k.as_str().to_ascii_lowercase()).collect()
}

/// The required-header guard's configured list, when that guard is bound.
fn required_headers(up: &Upstream, matched: &routing::Matched, key: &str) -> Vec<String> {
    const GUARD_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    let bound = up.plugins.items.iter().any(|i| i == GUARD_ID)
        || matched.route.plugins.items.iter().any(|i| i == GUARD_ID);
    if !bound {
        return Vec::new();
    }
    let raw = up
        .auth
        .config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    plugins::parse_required_headers(raw.as_deref())
}

/// Charge the effective rate limit once for this request.
fn charge_rate_limit(
    state: &OagwState,
    up: &Upstream,
    matched: &routing::Matched,
    caller: &Caller,
    client_ip: Option<&str>,
) -> Result<Option<ratelimit::Decision>, DomainError> {
    // A route limit takes precedence over the upstream's; when neither the
    // route nor the upstream configures one, no rate limiting applies.
    let Some(rl) = matched.route.rate_limit.as_ref().or(up.rate_limit.as_ref()) else {
        return Ok(None);
    };
    let key = ratelimit::scope_key(
        rl.scope,
        &caller.tenant_id.to_string(),
        caller.subject_id.map(|s| s.to_string()).as_deref(),
        client_ip,
        Some(&matched.route.id.to_string()),
        &up.id.to_string(),
    );
    let d = state.limiter.check(&key, rl);
    if d.allowed {
        Ok(Some(d))
    } else {
        Err(DomainError::RateLimited {
            retry_after_secs: d.retry_after_secs,
        })
    }
}

fn apply_extra_headers(resp: &mut Response, extra: &[(&'static str, String)]) {
    for (k, v) in extra {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v))
        {
            resp.headers_mut().insert(name, value);
        }
    }
}

/// Attach `X-RateLimit-*` to an ordinary response. A `101` carries none, since
/// the exchange leaves HTTP semantics behind at that point.
fn apply_rate_limit_headers(resp: &mut Response, d: Option<&ratelimit::Decision>) {
    let Some(d) = d else { return };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        return;
    }
    let set = |resp: &mut Response, name: &'static str, value: String| {
        if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(&value))
        {
            resp.headers_mut().insert(n, v);
        }
    };
    set(resp, "x-ratelimit-limit", d.limit.to_string());
    set(resp, "x-ratelimit-remaining", d.remaining.to_string());
    set(resp, "x-ratelimit-reset", d.reset_secs.to_string());
}

/// Re-exported for tests: the error-source header name.
pub const SOURCE_HEADER: &str = ERROR_SOURCE_HEADER;

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_websocket_upgrade_needs_both_headers() {
        assert!(is_websocket_upgrade(&headers(&[
            ("upgrade", "websocket"),
            ("connection", "Upgrade")
        ])));
        // Browsers commonly send a token list.
        assert!(is_websocket_upgrade(&headers(&[
            ("upgrade", "websocket"),
            ("connection", "keep-alive, Upgrade")
        ])));
        assert!(!is_websocket_upgrade(&headers(&[("upgrade", "websocket")])));
        assert!(!is_websocket_upgrade(&headers(&[("connection", "Upgrade")])));
        assert!(!is_websocket_upgrade(&headers(&[
            ("upgrade", "h2c"),
            ("connection", "Upgrade")
        ])));
        assert!(!is_websocket_upgrade(&HeaderMap::new()));
    }

    #[test]
    fn header_names_are_lowercased() {
        let n = header_names(&headers(&[("X-Abc", "1")]));
        assert_eq!(n, vec!["x-abc".to_owned()]);
    }

    #[test]
    fn the_source_header_name_is_the_documented_one() {
        assert_eq!(SOURCE_HEADER, "x-oagw-error-source");
    }
}
