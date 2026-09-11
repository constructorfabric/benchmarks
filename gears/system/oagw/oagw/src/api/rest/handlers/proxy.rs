// @cpt-begin:cpt-cf-oagw-dod-proxy-http-response-relay:p1:inst-proxy-handler
//! The data-plane proxy handler.
//!
//! One handler serves plain requests, server-sent-event streams and WebSocket
//! upgrades. It resolves the upstream by alias, matches a route, applies the
//! guards and the policy layer, forwards the request, and relays the response.

use crate::api::rest::error::{problem_response, set_error_source};
use crate::api::rest::state::OagwState;
use crate::domain::error::{DomainError, DomainResult, ErrorKind, ErrorSource};
use crate::domain::model::{CorsConfig, Route, Upstream};
use crate::infra::oauth2::{ClientAuthStyle, TokenCache, effective_ttl, fetch_token};
use crate::infra::plugins::{
    GuardPhase, PluginClass, apply_auth_plugin, apply_guard_plugin, apply_transform_plugin,
    classify_plugin,
};
use crate::infra::proxy::{
    TARGET_HOST_HEADER, apply_guards, apply_request_header_rules, apply_response_header_rules,
    build_outbound_headers, build_upstream_path, build_upstream_url, build_websocket_url,
    check_plaintext_allowed, is_event_stream, is_websocket_upgrade, match_route,
    reject_relative_path_segments, select_endpoint, ssrf_check,
};
use crate::infra::ratelimit::{Admission, RateLimiter};
use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use std::sync::Arc;
use std::time::Duration;
use toolkit_security::SecurityContext;

/// Maximum buffered request body, in bytes.
const MAX_BODY: usize = crate::infra::proxy::MAX_BODY_BYTES;

/// Handle a proxy request whose path carries no suffix.
pub async fn proxy_root(
    ctx: Extension<SecurityContext>,
    state: Extension<Arc<OagwState>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    dispatch(ctx, state, alias, String::new(), request).await
}

/// Handle a proxy request that carries a trailing path suffix.
pub async fn proxy_with_path(
    ctx: Extension<SecurityContext>,
    state: Extension<Arc<OagwState>>,
    Path((alias, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    dispatch(ctx, state, alias, rest, request).await
}

/// Answer an `OPTIONS` request on a suffix-less proxy path.
///
/// A genuine cross-origin preflight is answered permissively; anything else
/// receives a plain `404` gateway problem. Neither case reads the security
/// context, touches the store, or runs any part of the forwarding pipeline:
/// the registration is anonymous specifically so this can run without a
/// resolved tenant.
#[allow(
    clippy::unused_async,
    reason = "axum's Handler trait is only implemented for an async fn; the body never awaits \
              because it deliberately runs no part of the forwarding pipeline"
)]
pub async fn preflight_root(headers: HeaderMap) -> Response {
    answer_options(&headers)
}

/// Answer an `OPTIONS` request on a proxy path that carries a trailing path
/// suffix.
///
/// See [`preflight_root`]; the suffix plays no part in a preflight decision.
#[allow(
    clippy::unused_async,
    reason = "axum's Handler trait is only implemented for an async fn; the body never awaits \
              because it deliberately runs no part of the forwarding pipeline"
)]
pub async fn preflight_with_path(headers: HeaderMap) -> Response {
    answer_options(&headers)
}

/// Answer an `OPTIONS` request without resolving a tenant or an upstream.
fn answer_options(headers: &HeaderMap) -> Response {
    if is_preflight(headers) {
        preflight_response(headers)
    } else {
        problem_response(&DomainError::not_found(
            "this endpoint answers only a cross-origin preflight",
        ))
    }
}

/// Resolve, guard and forward one proxy request.
async fn dispatch(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    alias: String,
    rest: String,
    request: Request,
) -> Response {
    match forward(&ctx, &state, &alias, &rest, request).await {
        Ok(response) => response,
        Err(error) => problem_response(&error),
    }
}

/// The proxy pipeline.
#[allow(clippy::too_many_lines)]
async fn forward(
    ctx: &SecurityContext,
    state: &Arc<OagwState>,
    alias: &str,
    rest: &str,
    request: Request,
) -> DomainResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let method = request.method().clone();
    let inbound_headers = request.headers().clone();
    let query = request.uri().query().unwrap_or("").to_owned();
    let suffix = if rest.is_empty() {
        String::new()
    } else {
        format!("/{}", rest.trim_start_matches('/'))
    };

    // A `.` or `..` segment could otherwise escape the route's path prefix
    // once an upstream that normalizes dot segments receives it. This runs
    // before route matching so a crafted suffix cannot even influence which
    // route is chosen.
    reject_relative_path_segments(&suffix)?;

    // ---- alias resolution ------------------------------------------------
    let upstream = state
        .store
        .find_upstream_by_alias(tenant_id, alias)
        .ok_or_else(|| {
            DomainError::new(
                ErrorKind::RouteNotFound,
                format!("no upstream is registered for alias `{alias}`"),
            )
        })?;
    if !upstream.enabled {
        return Err(DomainError::new(
            ErrorKind::LinkUnavailable,
            format!("upstream `{alias}` is disabled"),
        ));
    }

    // ---- route matching --------------------------------------------------
    let routes = state.store.routes_for_upstream(tenant_id, upstream.id);
    let route = match_route(&routes, &method, &suffix).ok_or_else(|| {
        DomainError::new(
            ErrorKind::RouteNotFound,
            format!("no route on `{alias}` matches {method} {suffix}"),
        )
    })?;

    let cors = effective_cors(route, &upstream);

    // ---- cross-origin policy on an actual request ------------------------
    if let Some(cors) = cors
        && cors.enabled
        && let Some(origin) = inbound_headers.get(http::header::ORIGIN)
    {
        check_cors_actual(cors, origin, &method)?;
    }

    // ---- guards ----------------------------------------------------------
    apply_guards(route, &method, &suffix, &query)?;
    check_body_limits(&inbound_headers)?;

    // ---- rate limiting ---------------------------------------------------
    let rate_limit = route.rate_limit.as_ref().or(upstream.rate_limit.as_ref());
    let mut rate_headers: Vec<(String, String)> = Vec::new();
    if let Some(config) = rate_limit {
        let key = RateLimiter::scope_key(
            config.scope,
            &upstream.id.to_string(),
            &tenant_id.to_string(),
            &ctx.subject_id().to_string(),
            client_ip(request.extensions()).as_str(),
            &route.id.to_string(),
        );
        match state.rate_limiter.check(&key, config) {
            Admission::Allowed {
                limit,
                remaining,
                reset_after,
            } => {
                rate_headers.push(("x-ratelimit-limit".to_owned(), limit.to_string()));
                rate_headers.push(("x-ratelimit-remaining".to_owned(), remaining.to_string()));
                rate_headers.push(("x-ratelimit-reset".to_owned(), reset_after.to_string()));
            }
            Admission::Degraded { limit } => {
                rate_headers.push(("x-ratelimit-limit".to_owned(), limit.to_string()));
                rate_headers.push(("x-ratelimit-remaining".to_owned(), "0".to_owned()));
            }
            Admission::Rejected { limit, retry_after } => {
                let error = DomainError::new(
                    ErrorKind::RateLimitExceeded,
                    "rate limit exceeded for this scope",
                )
                .with_context(serde_json::json!({ "retry_after_seconds": retry_after }));
                let mut response = problem_response(&error);
                let headers = response.headers_mut();
                insert_str(headers, "retry-after", &retry_after.to_string());
                insert_str(headers, "x-ratelimit-limit", &limit.to_string());
                insert_str(headers, "x-ratelimit-remaining", "0");
                insert_str(headers, "x-ratelimit-reset", &retry_after.to_string());
                return Ok(response);
            }
        }
    }

    // ---- endpoint selection ---------------------------------------------
    let target_host = inbound_headers
        .get(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok());
    let endpoint = select_endpoint(&upstream, target_host, &state.round_robin)?.clone();
    // The check always runs; the policy flag decides whether it rejects.
    ssrf_check(&endpoint, state.config.ssrf_policy.enabled)?;
    check_plaintext_allowed(&endpoint, state.config.allow_http_upstream)?;

    // The request-phase pipeline order is: credential injection, then the
    // guard/transform plugin chain, then the configurable
    // `headers.request.{remove,set,add}` rules, then the dial. Each stage can
    // override what the previous one produced, and a configured rule is
    // therefore the last word, as the contract documents.

    // ---- outbound header construction (base copy only) --------------------
    let mut out_headers =
        build_outbound_headers(&inbound_headers, &endpoint, upstream.headers.as_ref());

    // ---- credential injection --------------------------------------------
    if let Some(auth) = upstream.auth.as_ref()
        && let Some(plugin_ref) = auth.plugin_type.as_deref()
    {
        if let Some(style) = oauth2_style(plugin_ref) {
            inject_client_credentials_token(ctx, state, &auth.config, style, &mut out_headers)
                .await?;
        } else {
            apply_auth_plugin(
                plugin_ref,
                &auth.config,
                ctx,
                state.cred_store.as_ref(),
                &mut out_headers,
            )
            .await?;
        }
    }

    // ---- plugin chain: upstream bindings then route bindings -------------
    let chain = plugin_chain(&upstream, route);
    for plugin_ref in &chain {
        match classify_plugin(plugin_ref) {
            PluginClass::Guard => {
                // The supplied schema declares `plugins.items` as bare
                // identifier strings, so a binding carries no configuration of
                // its own. The upstream's `auth.config` block is therefore the
                // only per-upstream configuration surface the schema offers,
                // and every guard reads from it. A route-bound guard cannot yet
                // carry route-specific configuration for the same reason.
                let config = guard_config(&upstream);
                apply_guard_plugin(plugin_ref, &config, &inbound_headers, GuardPhase::Request)?;
            }
            PluginClass::Transform => apply_transform_plugin(plugin_ref, &mut out_headers)?,
            PluginClass::Auth | PluginClass::Unknown => {
                return Err(DomainError::new(
                    ErrorKind::PluginNotFound,
                    format!("plugin `{plugin_ref}` cannot be bound through plugins.items"),
                ));
            }
        }
    }

    // ---- configurable request header rules --------------------------------
    // Applied last, so a configured `headers.request.set` rule can override an
    // injected credential or a transform's result.
    apply_request_header_rules(&mut out_headers, upstream.headers.as_ref());

    let upstream_path = build_upstream_path(route, &suffix);

    // ---- WebSocket upgrade ------------------------------------------------
    if is_websocket_upgrade(&inbound_headers) {
        let url = build_websocket_url(&endpoint, &upstream_path, &query);
        return websocket_relay(request, url, out_headers);
    }

    // ---- plain and streaming HTTP ----------------------------------------
    let url = build_upstream_url(&endpoint, &upstream_path, &query);
    let body_bytes = axum::body::to_bytes(request.into_body(), MAX_BODY)
        .await
        .map_err(|_| {
            DomainError::new(
                ErrorKind::PayloadTooLarge,
                "request body exceeds the 100MB limit",
            )
        })?;
    check_declared_length_matches_body(&inbound_headers, body_bytes.len())?;

    let mut builder = match method {
        Method::GET => state.http.get(&url),
        Method::POST => state.http.post(&url),
        Method::PUT => state.http.put(&url),
        Method::PATCH => state.http.patch(&url),
        Method::DELETE => state.http.delete(&url),
        Method::HEAD => state.http.head(&url),
        Method::OPTIONS => state.http.options(&url),
        ref other => {
            return Err(DomainError::validation(format!(
                "method {other} is not supported by the proxy"
            )));
        }
    };
    for (name, value) in &out_headers {
        if let Ok(value) = value.to_str() {
            builder = builder.header(name.as_str(), value);
        }
    }
    if !body_bytes.is_empty() {
        builder = builder.body_bytes(body_bytes);
    }

    let timeout = Duration::from_secs(state.config.proxy_timeout_secs.max(1));
    let sent = tokio::time::timeout(timeout, builder.send()).await;
    let upstream_response = match sent {
        Err(_) => {
            return Err(DomainError::new(
                ErrorKind::RequestTimeout,
                "the upstream did not respond within the configured timeout",
            ));
        }
        Ok(Err(error)) => return Err(map_transport_error(&error)),
        Ok(Ok(response)) => response,
    };

    let status = upstream_response.status();
    // A pristine copy, taken before any response header rule can rewrite it,
    // is what the response-phase guards see: the contract requires them to
    // judge what the upstream actually sent, and a configured
    // `headers.response.add` rule must not be able to manufacture a header
    // that satisfies a guard that should have rejected the response.
    let pristine_response_headers = upstream_response.headers().clone();
    let streaming = is_event_stream(&pristine_response_headers);

    for plugin_ref in &chain {
        if classify_plugin(plugin_ref) == PluginClass::Guard {
            let config = guard_config(&upstream);
            apply_guard_plugin(
                plugin_ref,
                &config,
                &pristine_response_headers,
                GuardPhase::Response,
            )?;
        }
    }

    // Only now, after the guards have judged the untouched response, may the
    // configured response header rules mutate the map that is returned.
    let mut response_headers = pristine_response_headers;
    apply_response_header_rules(&mut response_headers, upstream.headers.as_ref());

    // A stream is relayed frame by frame; the body limit and the request
    // timeout bound reaching this point, not the stream's remaining life.
    let body = Body::new(upstream_response.into_body());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    // Content length is recomputed by the transport for a relayed body.
    response.headers_mut().remove(http::header::CONTENT_LENGTH);
    if streaming {
        response.headers_mut().insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    }
    for (name, value) in rate_headers {
        insert_str(response.headers_mut(), &name, &value);
    }
    if let Some(cors) = cors
        && cors.enabled
    {
        add_cors_response_headers(response.headers_mut(), cors, &inbound_headers);
    }
    // The response was produced by the upstream, so it is labelled as such.
    set_error_source(&mut response, ErrorSource::Upstream);
    Ok(response)
}

/// The configuration a guard plugin reads.
///
/// See the note at the guard dispatch site: the schema gives a binding no
/// configuration of its own, so the upstream's `auth.config` block is the only
/// available surface.
fn guard_config(upstream: &Upstream) -> std::collections::BTreeMap<String, serde_json::Value> {
    upstream
        .auth
        .as_ref()
        .map(|a| a.config.clone())
        .unwrap_or_default()
}

/// Compose the plugin chain: upstream bindings run before route bindings.
fn plugin_chain(upstream: &Upstream, route: &Route) -> Vec<String> {
    let mut chain: Vec<String> = upstream
        .plugins
        .as_ref()
        .map(|p| p.items.clone())
        .unwrap_or_default();
    if let Some(route_plugins) = route.plugins.as_ref() {
        chain.extend(route_plugins.items.iter().cloned());
    }
    chain
}

/// Whether the request declares a chunked `Transfer-Encoding`.
fn is_chunked(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::TRANSFER_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"))
}

/// Reject a body that declares an invalid or over-large length, or an
/// ambiguous framing.
fn check_body_limits(headers: &HeaderMap) -> DomainResult<()> {
    let chunked = is_chunked(headers);
    // A request carrying both a `Content-Length` and a chunked
    // `Transfer-Encoding` is the classic precondition for request smuggling:
    // the gateway and the upstream could each pick a different header to
    // believe, and disagree about where the body ends. Reject it outright
    // rather than guessing which one is authoritative.
    if headers.contains_key(http::header::CONTENT_LENGTH) && chunked {
        return Err(DomainError::validation(
            "a request must not carry both Content-Length and a chunked Transfer-Encoding",
        ));
    }
    if let Some(raw) = headers.get(http::header::CONTENT_LENGTH) {
        let text = raw
            .to_str()
            .map_err(|_| DomainError::validation("Content-Length is not a valid integer"))?;
        let declared: usize = text
            .parse()
            .map_err(|_| DomainError::validation("Content-Length is not a valid integer"))?;
        if declared > MAX_BODY {
            return Err(DomainError::new(
                ErrorKind::PayloadTooLarge,
                "request body exceeds the 100MB limit",
            ));
        }
    }
    if headers.contains_key(http::header::TRANSFER_ENCODING) && !chunked {
        return Err(DomainError::validation(
            "only chunked transfer encoding is supported",
        ));
    }
    Ok(())
}

/// Reject a body whose actual size disagrees with a declared `Content-Length`.
///
/// `check_body_limits` validates the declared value in isolation, before the
/// body is read; this validates it against what was actually buffered, as the
/// body-validation algorithm requires.
fn check_declared_length_matches_body(headers: &HeaderMap, actual_len: usize) -> DomainResult<()> {
    if let Some(raw) = headers.get(http::header::CONTENT_LENGTH)
        && let Ok(text) = raw.to_str()
        && let Ok(declared) = text.parse::<usize>()
        && declared != actual_len
    {
        return Err(DomainError::validation(format!(
            "Content-Length declared {declared} bytes but the request body was {actual_len} bytes"
        )));
    }
    Ok(())
}

/// Map a transport failure onto the catalogue deterministically.
fn map_transport_error(error: &toolkit_http::HttpError) -> DomainError {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("timed out") || text.contains("timeout") {
        return DomainError::new(
            ErrorKind::ConnectionTimeout,
            "upstream connection timed out",
        );
    }
    if text.contains("dns") || text.contains("resolve") {
        return DomainError::new(
            ErrorKind::LinkUnavailable,
            "the upstream host could not be resolved",
        );
    }
    if text.contains("refused") || text.contains("reset") || text.contains("connect") {
        return DomainError::new(
            ErrorKind::DownstreamError,
            "the upstream refused or reset the connection",
        );
    }
    DomainError::new(
        ErrorKind::ProtocolError,
        "the upstream response was not usable",
    )
}

/// Client address for an `ip`-scoped rate limit.
///
/// `X-Forwarded-For` is never consulted: it is caller-supplied, and a caller
/// that varies it on every request would mint a fresh token bucket each time,
/// trivially bypassing the `ip` scope. The gear does not own the listener, so
/// the real peer address is available only when the runtime populates
/// `ConnectInfo<SocketAddr>` in the request extensions (as it does when
/// served directly; a gateway that proxies to this gear over a Unix socket or
/// a test harness may not supply it). When it is unavailable, every request
/// shares a single fixed bucket key, so the limit over-restricts rather than
/// being bypassable — a deliberate trade-off in the safe direction.
fn client_ip(extensions: &http::Extensions) -> String {
    extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map_or_else(
            || "unknown".to_owned(),
            |connect_info| connect_info.0.ip().to_string(),
        )
}

fn insert_str(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::HeaderName::try_from(name),
        HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}
// @cpt-end:cpt-cf-oagw-dod-proxy-http-response-relay:p1:inst-proxy-handler

// @cpt-begin:cpt-cf-oagw-dod-policy-credential-resolution:p2:inst-oauth2-inject
/// Which client-credentials variant a plugin reference names, if any.
fn oauth2_style(plugin_ref: &str) -> Option<ClientAuthStyle> {
    match crate::domain::model::gts_instance(plugin_ref) {
        crate::infra::plugins::AUTH_OAUTH2_FORM => Some(ClientAuthStyle::Form),
        crate::infra::plugins::AUTH_OAUTH2_BASIC => Some(ClientAuthStyle::Basic),
        _ => None,
    }
}

/// Resolve a client-credentials bearer token and inject it.
///
/// A live cached token is reused, so a second request within the token's
/// lifetime does not call the identity provider again. A failed fetch is not
/// cached.
async fn inject_client_credentials_token(
    ctx: &SecurityContext,
    state: &Arc<OagwState>,
    config: &std::collections::BTreeMap<String, serde_json::Value>,
    style: ClientAuthStyle,
    headers: &mut HeaderMap,
) -> DomainResult<()> {
    let key = TokenCache::cache_key(
        &ctx.subject_tenant_id().to_string(),
        &ctx.subject_id().to_string(),
        style,
        config,
    );
    let token = if let Some(cached) = state.token_cache.get(&key) {
        cached
    } else {
        let endpoint = config
            .get("token_endpoint")
            .or_else(|| config.get("issuer_url"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::AuthenticationFailed,
                    "the auth plugin config names no token_endpoint or issuer_url",
                )
            })?;
        let client_id = resolve_reference(config, "client_id_ref", ctx, state).await?;
        let client_secret = resolve_reference(config, "client_secret_ref", ctx, state).await?;
        let scopes = config.get("scopes").and_then(serde_json::Value::as_str);
        let (token, expires_in) = fetch_token(
            &state.http,
            endpoint,
            &client_id,
            &client_secret,
            scopes,
            style,
        )
        .await?;
        let ttl = effective_ttl(state.config.token_cache_ttl_secs, expires_in);
        state.token_cache.put(key, token.clone(), ttl);
        token
    };
    let value = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        DomainError::new(
            ErrorKind::AuthenticationFailed,
            "the resolved token is not a valid header value",
        )
    })?;
    headers.insert(http::header::AUTHORIZATION, value);
    Ok(())
}

/// Read a credential either inline or from the credential store.
async fn resolve_reference(
    config: &std::collections::BTreeMap<String, serde_json::Value>,
    key: &str,
    ctx: &SecurityContext,
    state: &Arc<OagwState>,
) -> DomainResult<String> {
    let raw = config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            DomainError::new(
                ErrorKind::AuthenticationFailed,
                format!("the auth plugin config names no `{key}`"),
            )
        })?;
    let Some(reference) = raw.strip_prefix("cred://") else {
        // A value that is not a reference is used as-is.
        return Ok(raw.to_owned());
    };
    let store = state.cred_store.as_ref().ok_or_else(|| {
        DomainError::new(ErrorKind::SecretNotFound, "credential store is unavailable")
    })?;
    let secret_ref = credstore_sdk::models::SecretRef::new(reference)
        .map_err(|e| DomainError::validation(format!("invalid credential reference: {e}")))?;
    match store.get(ctx, &secret_ref).await {
        Ok(Some(found)) => String::from_utf8(found.value.as_bytes().to_vec())
            .map_err(|_| DomainError::new(ErrorKind::SecretNotFound, "secret is not valid UTF-8")),
        Ok(None) => Err(DomainError::new(
            ErrorKind::SecretNotFound,
            format!("secret `{reference}` was not found"),
        )),
        Err(_) => Err(DomainError::new(
            ErrorKind::AuthenticationFailed,
            "credential store rejected the request",
        )),
    }
}
// @cpt-end:cpt-cf-oagw-dod-policy-credential-resolution:p2:inst-oauth2-inject

// @cpt-begin:cpt-cf-oagw-dod-policy-cors-preflight:p2:inst-cors
/// The cross-origin policy that governs a request.
///
/// A route's own `cors` block overrides the upstream's when the route sets
/// one; otherwise the upstream's policy applies.
#[must_use]
pub fn effective_cors<'a>(route: &'a Route, upstream: &'a Upstream) -> Option<&'a CorsConfig> {
    route.cors.as_ref().or(upstream.cors.as_ref())
}

/// Whether a request is a cross-origin preflight.
#[must_use]
pub fn is_preflight(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::ORIGIN)
        && headers.contains_key(http::header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Answer a preflight permissively, without resolving an upstream.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();
    if let Some(origin) = headers.get(http::header::ORIGIN) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get(http::header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(http::header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(request_headers) = headers.get(http::header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(
            http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            request_headers.clone(),
        );
    }
    out.insert(
        http::header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("86400"),
    );
    out.insert(
        http::header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    set_error_source(&mut response, ErrorSource::Gateway);
    response
}

/// Validate an actual cross-origin request against the upstream policy.
///
/// Origin matching is exact, and both port- and protocol-sensitive.
///
/// # Errors
/// Returns a validation error when the origin or the method is not permitted.
pub fn check_cors_actual(
    cors: &CorsConfig,
    origin: &HeaderValue,
    method: &Method,
) -> DomainResult<()> {
    let Ok(origin) = origin.to_str() else {
        return Err(DomainError::validation(
            "Origin is not a valid header value",
        ));
    };
    let allowed = cors
        .allowed_origins
        .iter()
        .any(|candidate| candidate == "*" || candidate == origin);
    if !allowed {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            format!("origin `{origin}` is not allowed"),
        )
        .with_context(serde_json::json!({
            "error_code": "cors.origin_not_allowed",
        })));
    }
    if !cors.allowed_methods.iter().any(|m| m == method.as_str()) {
        return Err(DomainError::new(
            ErrorKind::ValidationError,
            format!("method {method} is not allowed for a cross-origin request"),
        )
        .with_context(serde_json::json!({
            "error_code": "cors.method_not_allowed",
        })));
    }
    Ok(())
}

/// Add the cross-origin headers to a relayed response.
pub fn add_cors_response_headers(
    headers: &mut HeaderMap,
    cors: &CorsConfig,
    request_headers: &HeaderMap,
) {
    if let Some(origin) = request_headers.get(http::header::ORIGIN) {
        headers.insert(http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if cors.allow_credentials {
        headers.insert(
            http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            HeaderValue::from_static("true"),
        );
    }
    if !cors.expose_headers.is_empty()
        && let Ok(value) = HeaderValue::from_str(&cors.expose_headers.join(", "))
    {
        headers.insert(http::header::ACCESS_CONTROL_EXPOSE_HEADERS, value);
    }
    headers.insert(http::header::VARY, HeaderValue::from_static("Origin"));
}
// @cpt-end:cpt-cf-oagw-dod-policy-cors-preflight:p2:inst-cors

// @cpt-begin:cpt-cf-oagw-dod-proxy-streaming-ws-relay:p1:inst-ws
/// Complete a WebSocket upgrade with the client and relay to the upstream.
///
/// The handshake reply carries `Sec-WebSocket-Accept`, which axum derives from
/// the client's `Sec-WebSocket-Key`. `Upgrade` and `Connection` are ordinarily
/// stripped as hop-by-hop headers, so the upstream dial reconstructs them
/// deliberately through the client library rather than forwarding them.
///
/// The proxy timeout bounds reaching the upstream's `101` reply; it does not
/// bound the lifetime of the established session.
fn websocket_relay(
    request: Request,
    upstream_url: String,
    forwarded: HeaderMap,
) -> DomainResult<Response> {
    use axum::extract::FromRequestParts;
    use axum::extract::ws::WebSocketUpgrade;

    let (mut parts, _body) = request.into_parts();
    // The extractor is infallible over already-parsed parts, so the future
    // resolves immediately and never blocks here.
    let upgrade = futures_util::future::FutureExt::now_or_never(
        WebSocketUpgrade::from_request_parts(&mut parts, &()),
    )
    .and_then(Result::ok)
    .ok_or_else(|| DomainError::new(ErrorKind::ProtocolError, "invalid WebSocket upgrade"))?;

    // The `101` reply to the client is committed by `on_upgrade` below,
    // before the upstream dial (inside the callback) even starts, so the
    // upstream's actual subprotocol choice cannot be known yet and cannot be
    // echoed. The best available negotiation is client-side only: offer back
    // the client's own requested list, so `WebSocketUpgrade` selects (and the
    // `101` echoes) the client's first-preference protocol.
    let requested_protocols: Vec<String> = upgrade
        .requested_protocols()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let upgrade = if requested_protocols.is_empty() {
        upgrade
    } else {
        upgrade.protocols(requested_protocols)
    };

    let mut response = upgrade.on_upgrade(move |client| async move {
        if let Err(error) = relay_websocket(client, upstream_url, forwarded).await {
            tracing::debug!(error = %error, "websocket relay ended");
        }
    });
    set_error_source(&mut response, ErrorSource::Gateway);
    Ok(response)
}

/// Pump frames between the client and the upstream until either side closes.
async fn relay_websocket(
    client: axum::extract::ws::WebSocket,
    upstream_url: String,
    forwarded: HeaderMap,
) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = upstream_url.as_str().into_client_request()?;
    // Carry the transformed headers, minus the three the handshake itself
    // owns. `Sec-WebSocket-Protocol` and `Sec-WebSocket-Extensions` are not
    // handshake-owned: they carry the caller's subprotocol and extension
    // negotiation and must reach the upstream dial.
    for (name, value) in &forwarded {
        let lower = name.as_str().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            // The handshake owns these three, and `Sec-WebSocket-Extensions`
            // is negotiated per connection (RFC 6455 section 9). This relay
            // terminates and re-originates both legs, so forwarding an
            // extension would let the upstream compress frames the client
            // never agreed to decompress.
            "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-accept"
                | "sec-websocket-extensions"
        ) || matches!(lower.as_str(), "host" | "connection" | "upgrade")
        {
            continue;
        }
        request.headers_mut().insert(name.clone(), value.clone());
    }
    let (upstream, _response) = tokio_tungstenite::connect_async(request).await?;

    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    // A single loop waits on whichever side produces the next event, so both
    // directions make progress concurrently.
    loop {
        tokio::select! {
            from_client = client_rx.next() => {
                match from_client {
                    Some(Ok(message)) => {
                        let closing = matches!(message, axum::extract::ws::Message::Close(_));
                        upstream_tx.send(to_tungstenite(message)).await?;
                        if closing {
                            break;
                        }
                    }
                    // The client closed or errored; close the upstream side.
                    Some(Err(_)) | None => break,
                }
            }
            from_upstream = upstream_rx.next() => {
                match from_upstream {
                    Some(Ok(message)) => {
                        let closing = message.is_close();
                        if let Some(message) = to_axum(message) {
                            client_tx.send(message).await?;
                        }
                        if closing {
                            break;
                        }
                    }
                    // The upstream closed or errored; close the client side.
                    Some(Err(_)) | None => break,
                }
            }
        }
    }
    drop(upstream_tx.close().await);
    drop(client_tx.close().await);
    Ok(())
}

/// Convert an inbound client frame into its upstream form.
fn to_tungstenite(message: axum::extract::ws::Message) -> tokio_tungstenite::tungstenite::Message {
    use axum::extract::ws::Message as Axum;
    use tokio_tungstenite::tungstenite::Message as Tung;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    match message {
        Axum::Text(text) => Tung::Text(text.as_str().into()),
        Axum::Binary(bytes) => Tung::Binary(bytes),
        Axum::Ping(bytes) => Tung::Ping(bytes),
        Axum::Pong(bytes) => Tung::Pong(bytes),
        Axum::Close(frame) => Tung::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
    }
}

/// Convert an upstream frame into its client form.
fn to_axum(message: tokio_tungstenite::tungstenite::Message) -> Option<axum::extract::ws::Message> {
    use axum::extract::ws::{CloseFrame, Message as Axum};
    use tokio_tungstenite::tungstenite::Message as Tung;

    Some(match message {
        Tung::Text(text) => Axum::Text(text.as_str().into()),
        Tung::Binary(bytes) => Axum::Binary(bytes),
        Tung::Ping(bytes) => Axum::Ping(bytes),
        Tung::Pong(bytes) => Axum::Pong(bytes),
        Tung::Close(frame) => Axum::Close(frame.map(|f| CloseFrame {
            code: f.code.into(),
            reason: f.reason.as_str().into(),
        })),
        // A raw frame has no client-facing equivalent.
        Tung::Frame(_) => return None,
    })
}
// @cpt-end:cpt-cf-oagw-dod-proxy-streaming-ws-relay:p1:inst-ws

#[cfg(test)]
mod tests {
    use super::{
        check_body_limits, check_cors_actual, check_declared_length_matches_body, client_ip,
        effective_cors, is_preflight, preflight_response, preflight_root, preflight_with_path,
    };
    use crate::domain::model::{
        CorsConfig, Endpoint, HttpMatch, MatchConfig, PROTOCOL_HTTP, PathSuffixMode, Route, Scheme,
        ServerConfig, SharingMode, Upstream,
    };
    use axum::extract::ConnectInfo;
    use http::{HeaderMap, HeaderValue, Method};
    use uuid::Uuid;

    fn cors(origins: &[&str], methods: &[&str]) -> CorsConfig {
        CorsConfig {
            sharing: SharingMode::Private,
            enabled: true,
            allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            allowed_methods: methods.iter().map(|m| (*m).to_owned()).collect(),
            expose_headers: vec![],
            allow_credentials: false,
        }
    }

    #[test]
    fn a_preflight_needs_both_marker_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://a.test"),
        );
        assert!(!is_preflight(&headers));
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        assert!(is_preflight(&headers));
    }

    #[test]
    fn a_preflight_answers_permissively() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://a.test"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        let response = preflight_response(&headers);
        assert_eq!(response.status(), 204);
        assert_eq!(
            response
                .headers()
                .get(http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .expect("origin echoed"),
            "https://a.test"
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::ACCESS_CONTROL_MAX_AGE)
                .expect("max age"),
            "86400"
        );
    }

    #[test]
    fn origin_matching_is_exact() {
        let policy = cors(&["https://a.test"], &["GET"]);
        assert!(
            check_cors_actual(
                &policy,
                &HeaderValue::from_static("https://a.test"),
                &Method::GET
            )
            .is_ok()
        );
        // A different port is a different origin.
        assert!(
            check_cors_actual(
                &policy,
                &HeaderValue::from_static("https://a.test:8443"),
                &Method::GET
            )
            .is_err()
        );
        // A different protocol is a different origin.
        assert!(
            check_cors_actual(
                &policy,
                &HeaderValue::from_static("http://a.test"),
                &Method::GET
            )
            .is_err()
        );
    }

    #[test]
    fn a_disallowed_method_is_rejected() {
        let policy = cors(&["https://a.test"], &["GET"]);
        let err = check_cors_actual(
            &policy,
            &HeaderValue::from_static("https://a.test"),
            &Method::DELETE,
        )
        .expect_err("method rejected");
        assert_eq!(err.context["error_code"], "cors.method_not_allowed");
    }

    #[test]
    fn a_non_numeric_content_length_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("abc"),
        );
        assert!(check_body_limits(&headers).is_err());
    }

    #[test]
    fn an_oversize_declared_body_is_rejected_before_buffering() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_LENGTH,
            HeaderValue::from_static("209715200"),
        );
        let err = check_body_limits(&headers).expect_err("too large");
        assert_eq!(err.status(), 413);
    }

    #[test]
    fn an_unsupported_transfer_encoding_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        assert!(check_body_limits(&headers).is_err());
        headers.insert(
            http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        assert!(check_body_limits(&headers).is_ok());
    }

    #[test]
    fn content_length_and_chunked_transfer_encoding_together_is_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("10"));
        headers.insert(
            http::header::TRANSFER_ENCODING,
            HeaderValue::from_static("chunked"),
        );
        let err = check_body_limits(&headers).expect_err("ambiguous framing rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn a_declared_content_length_must_match_the_actual_body() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));
        assert!(check_declared_length_matches_body(&headers, 5).is_ok());
        let err = check_declared_length_matches_body(&headers, 4).expect_err("mismatch rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn no_declared_content_length_is_not_checked_against_the_body() {
        let headers = HeaderMap::new();
        assert!(check_declared_length_matches_body(&headers, 12345).is_ok());
    }

    #[test]
    fn client_ip_uses_connect_info_when_present() {
        let mut extensions = http::Extensions::new();
        let addr: std::net::SocketAddr = "203.0.113.7:12345".parse().expect("addr");
        extensions.insert(ConnectInfo(addr));
        assert_eq!(client_ip(&extensions), "203.0.113.7");
    }

    #[test]
    fn client_ip_falls_back_to_a_fixed_key_without_connect_info() {
        // A caller-supplied `X-Forwarded-For` must never be consulted, so an
        // absent `ConnectInfo` falls back to a single shared key rather than
        // any inbound header.
        let extensions = http::Extensions::new();
        assert_eq!(client_ip(&extensions), "unknown");
    }

    #[tokio::test]
    async fn a_genuine_preflight_is_answered_by_the_dedicated_handler() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_static("https://a.test"),
        );
        headers.insert(
            http::header::ACCESS_CONTROL_REQUEST_METHOD,
            HeaderValue::from_static("POST"),
        );
        let response = preflight_root(headers.clone()).await;
        assert_eq!(response.status(), 204);
        let response = preflight_with_path(headers).await;
        assert_eq!(response.status(), 204);
    }

    #[tokio::test]
    async fn a_non_preflight_options_gets_a_plain_404() {
        // No `Origin` / `Access-Control-Request-Method` marker headers: this
        // is not a genuine preflight, and must not reach any tenant logic.
        let response = preflight_root(HeaderMap::new()).await;
        assert_eq!(response.status(), 404);
        let response = preflight_with_path(HeaderMap::new()).await;
        assert_eq!(response.status(), 404);
    }

    fn endpoint(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: 443,
        }
    }

    fn upstream_with_cors(cors: Option<CorsConfig>) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "api".to_owned(),
            enabled: true,
            server: ServerConfig {
                endpoints: vec![endpoint("api.example.com")],
            },
            protocol: PROTOCOL_HTTP.to_owned(),
            tags: vec![],
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors,
        }
    }

    fn route_with_cors(cors: Option<CorsConfig>) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1".to_owned(),
                    query_allowlist: vec![],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            tags: vec![],
            plugins: None,
            rate_limit: None,
            cors,
        }
    }

    #[test]
    fn a_routes_own_cors_policy_overrides_the_upstreams() {
        let route_cors = cors(&["https://route.test"], &["GET"]);
        let upstream_cors = cors(&["https://upstream.test"], &["GET"]);
        let route = route_with_cors(Some(route_cors.clone()));
        let upstream = upstream_with_cors(Some(upstream_cors));
        let resolved = effective_cors(&route, &upstream).expect("route cors wins");
        assert_eq!(resolved.allowed_origins, route_cors.allowed_origins);
    }

    #[test]
    fn the_upstreams_cors_policy_is_used_when_the_route_has_none() {
        let upstream_cors = cors(&["https://upstream.test"], &["GET"]);
        let route = route_with_cors(None);
        let upstream = upstream_with_cors(Some(upstream_cors.clone()));
        let resolved = effective_cors(&route, &upstream).expect("upstream cors used");
        assert_eq!(resolved.allowed_origins, upstream_cors.allowed_origins);
    }

    #[test]
    fn no_cors_policy_anywhere_resolves_to_none() {
        let route = route_with_cors(None);
        let upstream = upstream_with_cors(None);
        assert!(effective_cors(&route, &upstream).is_none());
    }
}
