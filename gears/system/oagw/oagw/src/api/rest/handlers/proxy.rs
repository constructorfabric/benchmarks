//! `{METHOD} /oagw/v1/proxy/{alias}` and `{METHOD} /oagw/v1/proxy/{alias}/{path}`
//! — the plain-HTTP proxy data plane (`cpt-cf-oagw-feature-http-proxy`,
//! `cpt-cf-oagw-flow-proxy-request-success`).
//!
//! Ties together the stages in `crate::domain::proxy`: CORS preflight
//! short-circuit, resolution, guard evaluation, body validation, the
//! plugin/rate-limit hook seam, endpoint selection, the plaintext-connection
//! policy, header transformation, and upstream invocation.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, FromRequestParts, Request};
use axum::http::header::ORIGIN;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use credstore_sdk::{CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::model::{Endpoint, PathSuffixMode, Route};
use crate::domain::proxy::{
    body, cors, endpoint, guard, headers as header_xform, plugin_seam, stream, upstream, websocket,
};
use crate::domain::resolve::{ResolvedPlan, resolve_proxy_target};
use crate::error::OagwError;
use crate::state::ControlPlaneState;

/// Gear-relative proxy path prefix, stripped to recover the alias and the
/// remaining path suffix (`cpt-cf-oagw-dod-proxy-endpoint-registration`).
const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// A credential-store fallback used only when no real client is layered
/// onto the router (every `oagw/tests/*.rs` integration test builds its own
/// minimal router directly from `register_routes`, without going through
/// `OagwGear::register_rest`). Every operation reports "not found"/"not
/// implemented", which only matters if a test configures an auth binding
/// that actually needs credential resolution without also registering a
/// real credential-store double.
#[derive(Debug, Default)]
struct NoopCredStore;

#[async_trait]
impl CredStoreClientV1 for NoopCredStore {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        Ok(None)
    }
}

/// Resolves the calling tenant id from the optional `SecurityContext`
/// extension, falling back to the nil UUID tenant when it is absent — the
/// same pattern the management handlers use.
fn tenant_id_of(security_ctx: Option<&SecurityContext>) -> Uuid {
    security_ctx
        .map(SecurityContext::subject_tenant_id)
        .unwrap_or_default()
}

/// `true` when `headers[x-forwarded-for]` names a first hop, used for the
/// rate-limit `ip` scope and the `OAuth2` token-cache subject boundary. Never
/// itself security-critical (this feature never enforces access control by
/// IP), so the simplest header alone is sufficient.
fn client_ip_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(|value| value.trim().to_owned())
}

/// `{METHOD} /oagw/v1/proxy/{alias}[/{path}]`
/// (`cpt-cf-oagw-dod-proxy-endpoint-registration`).
// @cpt-begin:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-handler-fn-01
pub async fn proxy_handler(
    Extension(state): Extension<Arc<ControlPlaneState>>,
    Extension(config): Extension<Arc<OagwConfig>>,
    Extension(client): Extension<Arc<reqwest::Client>>,
    security_ctx: Option<Extension<SecurityContext>>,
    credstore: Option<Extension<Arc<dyn CredStoreClientV1>>>,
    request: Request,
) -> Response {
    let (parts, body_stream) = request.into_parts();

    // @cpt-begin:cpt-cf-oagw-dod-cors-preflight:p1:inst-proxy-preflight-shortcircuit-01
    if cors::is_preflight_request(&parts.method, &parts.headers) {
        return cors::build_preflight_response(&parts.headers);
    }
    // @cpt-end:cpt-cf-oagw-dod-cors-preflight:p1:inst-proxy-preflight-shortcircuit-01

    let instance = parts.uri.path().to_owned();
    let security_context = security_ctx.map(|Extension(ctx)| ctx);
    let tenant_id = tenant_id_of(security_context.as_ref());
    let subject_id = security_context
        .as_ref()
        .map(SecurityContext::subject_id)
        .unwrap_or_default();
    let security_context = security_context.unwrap_or_else(SecurityContext::anonymous);
    let credstore: Arc<dyn CredStoreClientV1> = credstore.map_or_else(
        || Arc::new(NoopCredStore) as Arc<dyn CredStoreClientV1>,
        |Extension(client)| client,
    );

    match run_proxy_pipeline(
        &state,
        &config,
        &client,
        tenant_id,
        subject_id,
        &security_context,
        credstore.as_ref(),
        parts,
        body_stream,
    )
    .await
    {
        Ok(response) => response,
        Err(error) => error.with_instance(instance).into_response(),
    }
}
// @cpt-end:cpt-cf-oagw-dod-proxy-endpoint-registration:p1:inst-proxy-handler-fn-01

/// Runs every stage of the proxy request lifecycle after the CORS-preflight
/// short-circuit, returning either the final client-facing response or the
/// first gateway error raised along the way
/// (`cpt-cf-oagw-flow-proxy-request-success`, `cpt-cf-oagw-flow-proxy-guard-rejection`).
#[allow(clippy::too_many_arguments)]
async fn run_proxy_pipeline(
    state: &ControlPlaneState,
    config: &OagwConfig,
    client: &reqwest::Client,
    tenant_id: Uuid,
    subject_id: Uuid,
    security_context: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
    mut parts: axum::http::request::Parts,
    body_stream: axum::body::Body,
) -> Result<Response, OagwError> {
    let (alias, full_path) = split_alias_and_path(&parts.uri);
    // @cpt-begin:cpt-cf-oagw-dod-disabled-upstream-rejection:p1:inst-proxy-resolve-01
    let plan = resolve_proxy_target(state, tenant_id, &alias, parts.method.as_str(), &full_path)?;
    // @cpt-end:cpt-cf-oagw-dod-disabled-upstream-rejection:p1:inst-proxy-resolve-01

    let origin = origin_header(&parts.headers);
    run_guards(&plan, &parts, &full_path, origin.as_deref())?;

    // A guard rejection above always wins, so a recognised upgrade request
    // is only routed to the WebSocket pipeline once the ordinary guards
    // have already passed (`cpt-cf-oagw-dod-longlived-pipeline`, last
    // acceptance criterion: a guard violation is rejected before any stream
    // opens or upgrade is negotiated).
    // @cpt-begin:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-proxy-ws-route-01
    if websocket::is_websocket_upgrade(&parts.method, &parts.headers) {
        return run_websocket_pipeline(
            state,
            config,
            &plan,
            &full_path,
            tenant_id,
            subject_id,
            security_context,
            credstore,
            parts,
        )
        .await;
    }
    // @cpt-end:cpt-cf-oagw-dod-ws-upgrade-recognition:p1:inst-proxy-ws-route-01

    let declared_length = body::validate_body_headers(&parts.headers)?;
    let body_bytes = body::read_limited_body(body_stream, declared_length).await?;

    let mut query_overrides = BTreeMap::new();
    let chain_outcome = invoke_request_hook(
        state,
        config,
        &plan,
        tenant_id,
        subject_id,
        security_context,
        credstore,
        &mut parts,
        &body_bytes,
        &mut query_overrides,
    )
    .await?;

    let selected = select_target_endpoint(state, &plan, &parts.headers)?;
    upstream::enforce_plaintext_policy(selected.scheme, config.allow_http_upstream)?;

    let outbound_headers = header_xform::build_outbound_headers(
        &parts.headers,
        &plan.header_plan.request,
        &selected.host,
    )?;
    // Header names the plugin chain added or overwrote must always reach
    // the upstream regardless of the header-transformation plan's
    // passthrough mode (`cpt-cf-oagw-algo-apikey-injection`,
    // `cpt-cf-oagw-algo-request-id-propagation`).
    let outbound_headers =
        force_through_plugin_headers(outbound_headers, &parts.headers, &chain_outcome);
    let target_url = build_target_url_for(&selected, &parts.uri, &full_path, &query_overrides)?;

    let timeout = Duration::from_secs(config.proxy_timeout_secs.max(1));
    // @cpt-begin:cpt-cf-oagw-dod-proxy-timeout:p1:inst-proxy-invoke-01
    let head = upstream::invoke_upstream_head(
        client,
        parts.method,
        target_url,
        outbound_headers,
        body_bytes,
        timeout,
    )
    .await?;
    // @cpt-end:cpt-cf-oagw-dod-proxy-timeout:p1:inst-proxy-invoke-01

    // @cpt-begin:cpt-cf-oagw-dod-sse-detection:p1:inst-proxy-sse-branch-01
    if stream::is_event_stream(head.response.headers()) {
        return stream::build_streaming_response(head, &plan, origin.as_deref(), &chain_outcome)
            .await;
    }
    // @cpt-end:cpt-cf-oagw-dod-sse-detection:p1:inst-proxy-sse-branch-01

    let outcome = upstream::finish_buffered(head).await?;
    build_success_response(outcome, &plan, origin.as_deref(), &chain_outcome)
}

/// Negotiates and completes a WebSocket upgrade recognised on the proxy
/// path: runs the single request-phase plugin/rate-limit hook FIRST, then
/// selects the endpoint and enforces the plaintext policy exactly as the
/// plain path does (CODE2-F-003 — the same rate-limit admission, guard, and
/// target-host-selection precedence as `run_proxy_pipeline`, rather than the
/// reverse order this pipeline used to run them in), rebuilds the handshake
/// headers, negotiates with the upstream FIRST, and only then completes the
/// caller-facing upgrade (`cpt-cf-oagw-flow-websocket-session`,
/// `cpt-cf-oagw-algo-upgrade-negotiation`, `cpt-cf-oagw-dod-ws-upstream-first-upgrade`,
/// `cpt-cf-oagw-dod-longlived-pipeline`).
// @cpt-begin:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-pipeline-fn-01
#[allow(clippy::too_many_arguments)]
async fn run_websocket_pipeline(
    state: &ControlPlaneState,
    config: &OagwConfig,
    plan: &ResolvedPlan,
    full_path: &str,
    tenant_id: Uuid,
    subject_id: Uuid,
    security_context: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
    mut parts: axum::http::request::Parts,
) -> Result<Response, OagwError> {
    // Neither transport sends a request body, so the body-validation and
    // buffering stage is skipped entirely for an upgrade request
    // (`cpt-cf-oagw-dod-longlived-pipeline`, step `inst-pipe-09`); the
    // request hook still runs once, with an empty body — and, as on the
    // plain-HTTP path, before endpoint selection and the plaintext-policy
    // check, so rate-limit admission and guard rejections take precedence
    // over a target-host rejection (CODE2-F-003).
    let empty_body = Bytes::new();
    let mut query_overrides = BTreeMap::new();
    let chain_outcome = invoke_request_hook(
        state,
        config,
        plan,
        tenant_id,
        subject_id,
        security_context,
        credstore,
        &mut parts,
        &empty_body,
        &mut query_overrides,
    )
    .await?;

    let selected = select_target_endpoint(state, plan, &parts.headers)?;
    // @cpt-begin:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-ws-plaintext-policy-01
    upstream::enforce_plaintext_policy(selected.scheme, config.allow_http_upstream)?;
    // @cpt-end:cpt-cf-oagw-dod-plaintext-connection-policy:p1:inst-ws-plaintext-policy-01

    let outbound_headers = websocket::build_handshake_headers(
        &parts.headers,
        &plan.header_plan.request,
        &selected.host,
    )?;
    let outbound_headers =
        force_through_plugin_headers(outbound_headers, &parts.headers, &chain_outcome);

    let port = selected
        .port
        .unwrap_or_else(|| selected.scheme.standard_port());
    let query_suffix = merge_query_overrides(parts.uri.query(), &query_overrides)
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let path_and_query = format!("{full_path}{query_suffix}");
    let target_url =
        websocket::build_ws_target_url(selected.scheme, &selected.host, port, &path_and_query)?;

    let timeout = Duration::from_secs(config.proxy_timeout_secs.max(1));
    let upstream_socket =
        websocket::negotiate_upstream(target_url, outbound_headers, timeout).await?;

    // The response-phase plugin hook runs once on the response head, the
    // same way the buffered and streamed HTTP paths call it
    // (`cpt-cf-oagw-dod-longlived-pipeline`); axum builds the actual `101`
    // response itself below, so this call has no client-visible headers to
    // mutate and exists solely to preserve the single-invocation contract.
    // A guard rejection here cannot be surfaced as an error response (the
    // upgrade already succeeded), so it is logged and otherwise ignored.
    let mut synthetic_response_headers = HeaderMap::new();
    if let Err(error) = invoke_response_hook(
        plan,
        StatusCode::SWITCHING_PROTOCOLS,
        &mut synthetic_response_headers,
        &chain_outcome,
    ) {
        tracing::warn!(
            error = %error.to_problem().detail,
            "oagw: response-phase guard rejected a websocket upgrade after negotiation; ignoring"
        );
    }

    // @cpt-begin:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-client-upgrade-01
    let upgrade = WebSocketUpgrade::from_request_parts(&mut parts, &())
        .await
        .map_err(|_| {
            OagwError::protocol_error("client request is not a completable WebSocket upgrade")
        })?;

    Ok(upgrade.on_upgrade(move |client_socket| websocket::relay(client_socket, upstream_socket)))
    // @cpt-end:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-client-upgrade-01
}
// @cpt-end:cpt-cf-oagw-dod-ws-upstream-first-upgrade:p1:inst-ws-pipeline-fn-01

/// Splits a gear-relative proxy request path into its alias and the
/// remaining path suffix (empty for the bare `/oagw/v1/proxy/{alias}` form).
fn split_alias_and_path(uri: &Uri) -> (String, String) {
    let rest = uri.path().strip_prefix(PROXY_PREFIX).unwrap_or("");
    match rest.split_once('/') {
        Some((alias, suffix)) => (alias.to_owned(), format!("/{suffix}")),
        None => (rest.to_owned(), String::new()),
    }
}

fn origin_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Evaluates the CORS-actual-request, query-allowlist, and path-suffix
/// guards, in that fixed order (`cpt-cf-oagw-algo-guard-evaluation`).
// @cpt-begin:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-proxy-guards-01
// @cpt-begin:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-proxy-guards-01
// @cpt-begin:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-proxy-guards-01
fn run_guards(
    plan: &ResolvedPlan,
    parts: &axum::http::request::Parts,
    full_path: &str,
    origin: Option<&str>,
) -> Result<(), OagwError> {
    guard::enforce_cors_actual_request(
        plan.effective_cors.as_ref(),
        origin,
        parts.method.as_str(),
    )?;
    guard::enforce_query_allowlist(parts.uri.query(), route_query_allowlist(&plan.route))?;
    guard::enforce_path_suffix(
        route_path(&plan.route),
        full_path,
        route_suffix_mode(&plan.route),
    )
}
// @cpt-end:cpt-cf-oagw-dod-cors-request-enforcement:p1:inst-proxy-guards-01
// @cpt-end:cpt-cf-oagw-dod-path-suffix-guard:p1:inst-proxy-guards-01
// @cpt-end:cpt-cf-oagw-dod-query-allowlist-guard:p1:inst-proxy-guards-01

fn route_query_allowlist(route: &Route) -> &[String] {
    route
        .match_config
        .http
        .as_ref()
        .map_or(&[], |http| http.query_allowlist.as_slice())
}

fn route_path(route: &Route) -> &str {
    route
        .match_config
        .http
        .as_ref()
        .map_or("", |http| http.path.as_str())
}

fn route_suffix_mode(route: &Route) -> PathSuffixMode {
    route
        .match_config
        .http
        .as_ref()
        .map_or_else(PathSuffixMode::default, |http| http.path_suffix_mode)
}

/// Invokes the single request-phase plugin/rate-limit hook
/// (`cpt-cf-oagw-dod-plugin-hook-points`), building the runtime context from
/// the resolved plan, the calling identity, and the shared engine state
/// (`credstore`, `state`'s token cache and rate limiter, `config`'s timeout
/// and token-cache knobs).
#[allow(clippy::too_many_arguments)]
async fn invoke_request_hook(
    state: &ControlPlaneState,
    config: &OagwConfig,
    plan: &ResolvedPlan,
    tenant_id: Uuid,
    subject_id: Uuid,
    security_context: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
    parts: &mut axum::http::request::Parts,
    body_bytes: &Bytes,
    query_overrides: &mut BTreeMap<String, String>,
) -> Result<plugin_seam::RequestChainOutcome, OagwError> {
    let client_ip = client_ip_of(&parts.headers);
    let mut ctx = plugin_seam::RequestHookContext {
        effective_plugins: &plan.effective_plugins,
        effective_auth: plan.effective_auth.as_ref(),
        effective_rate_limit: plan.effective_rate_limit.as_ref(),
        method: &parts.method,
        headers: &mut parts.headers,
        body: body_bytes,
        query: query_overrides,
        tenant_id,
        subject_id,
        client_ip: client_ip.as_deref(),
        upstream_id: plan.upstream.id,
        route_id: plan.route.id,
        security_context,
        credstore,
        token_cache: state.token_cache(),
        rate_limiter: state.rate_limiter(),
        proxy_timeout: Duration::from_secs(config.proxy_timeout_secs.max(1)),
        token_cache_ttl_ceiling: Duration::from_secs(config.token_cache_ttl_secs),
        oauth_http_config_override: None,
    };
    plugin_seam::invoke_request_hooks(&mut ctx).await
}

/// Reads `X-OAGW-Target-Host` and selects the target endpoint
/// (`cpt-cf-oagw-dod-target-host-selection`).
fn select_target_endpoint(
    state: &ControlPlaneState,
    plan: &ResolvedPlan,
    headers: &HeaderMap,
) -> Result<Endpoint, OagwError> {
    let target_header = headers
        .get(&endpoint::TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok());
    endpoint::select_endpoint(state, plan.upstream.id, &plan.endpoints, target_header)
}

/// Forces every header named in `chain_outcome.forced_request_headers`
/// through to `outbound` from the (already plugin-mutated) `source` header
/// map, regardless of the upstream's configured passthrough mode
/// (`cpt-cf-oagw-algo-header-transformation` otherwise forwards nothing by
/// default): a plugin-injected credential or correlation identifier must
/// always reach the upstream, the same way `Content-Type` is always
/// forwarded independent of passthrough (`cpt-cf-oagw-algo-apikey-injection`,
/// `cpt-cf-oagw-algo-oauth2-token-acquisition`, `cpt-cf-oagw-algo-request-id-propagation`).
fn force_through_plugin_headers(
    mut outbound: HeaderMap,
    source: &HeaderMap,
    chain_outcome: &plugin_seam::RequestChainOutcome,
) -> HeaderMap {
    for name in &chain_outcome.forced_request_headers {
        if let Some(value) = source.get(name) {
            outbound.insert(name.clone(), value.clone());
        }
    }
    outbound
}

/// Merges the api-key query-placement overrides into the inbound query
/// string, replacing any inbound value of the same name
/// (`cpt-cf-oagw-algo-apikey-injection`).
fn merge_query_overrides(
    original: Option<&str>,
    overrides: &BTreeMap<String, String>,
) -> Option<String> {
    if overrides.is_empty() {
        return original.map(str::to_owned);
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    if let Some(original) = original {
        for (key, value) in form_urlencoded::parse(original.as_bytes()) {
            if !overrides.contains_key(key.as_ref()) {
                serializer.append_pair(&key, &value);
            }
        }
    }
    for (key, value) in overrides {
        serializer.append_pair(key, value);
    }
    let encoded = serializer.finish();
    if encoded.is_empty() {
        None
    } else {
        Some(encoded)
    }
}

/// Builds the outbound target URL for the selected endpoint, the resolved
/// path, and the inbound query string merged with any api-key
/// query-placement overrides (`cpt-cf-oagw-algo-upstream-invocation`,
/// `cpt-cf-oagw-algo-apikey-injection`).
fn build_target_url_for(
    selected: &Endpoint,
    uri: &Uri,
    full_path: &str,
    query_overrides: &BTreeMap<String, String>,
) -> Result<url::Url, OagwError> {
    let port = selected
        .port
        .unwrap_or_else(|| selected.scheme.standard_port());
    let query_suffix = merge_query_overrides(uri.query(), query_overrides)
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let path_and_query = format!("{full_path}{query_suffix}");
    upstream::build_target_url(selected.scheme, &selected.host, port, &path_and_query)
}

/// Applies the response header plan and CORS response headers, invokes the
/// response-phase plugin hook, stamps `X-OAGW-Error-Source: upstream`, and
/// returns the upstream status and body unmodified
/// (`cpt-cf-oagw-dod-header-plan-application`, `cpt-cf-oagw-dod-error-mapping`).
///
/// # Errors
///
/// Returns the mapped [`OagwError`] when a response-phase guard binding
/// rejects (`cpt-cf-oagw-dod-required-headers-guard`): the upstream's
/// response is discarded in favour of the problem document, so the
/// rejection is never mistaken for an upstream-sourced failure.
// @cpt-begin:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-success-response-01
fn build_success_response(
    outcome: upstream::UpstreamOutcome,
    plan: &ResolvedPlan,
    origin: Option<&str>,
    chain_outcome: &plugin_seam::RequestChainOutcome,
) -> Result<Response, OagwError> {
    let mut response_headers = outcome.headers;
    header_xform::strip_hop_by_hop_response_headers(&mut response_headers);
    header_xform::apply_response_header_plan(&mut response_headers, &plan.header_plan.response);
    header_xform::apply_cors_response_headers(
        &mut response_headers,
        plan.effective_cors.as_ref(),
        origin,
    );
    invoke_response_hook(plan, outcome.status, &mut response_headers, chain_outcome)?;

    let mut response = (outcome.status, outcome.body).into_response();
    *response.headers_mut() = response_headers;
    crate::error::stamp_upstream_source(&mut response);
    Ok(response)
}
// @cpt-end:cpt-cf-oagw-dod-error-mapping:p1:inst-proxy-success-response-01

fn invoke_response_hook(
    plan: &ResolvedPlan,
    status: StatusCode,
    headers: &mut HeaderMap,
    outcome: &plugin_seam::RequestChainOutcome,
) -> Result<(), OagwError> {
    let mut ctx = plugin_seam::ResponseHookContext {
        effective_plugins: &plan.effective_plugins,
        status,
        headers,
        outcome,
    };
    plugin_seam::invoke_response_hooks(&mut ctx)
}
