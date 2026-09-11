//! Top-level proxy request orchestration: wires every algorithm in this
//! module into the `cpt-cf-oagw-flow-proxy-request-forwarded` steps (and
//! its five sibling flows), including the fixed-order extension-point call
//! sites entries 2.6-2.9 attach to.

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use toolkit_http::HttpClient;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::plugins::binding::PluginBinding;
use crate::plugins::execute::{self, PostCallOutcome, PreCallOutcome};
use crate::plugins::plan::assemble_chain;
use crate::plugins::runtime::chain_runtime;
use crate::policy::cors::CorsOutcome;
use crate::policy::ratelimit::RateLimitOutcome;
use crate::policy::ratelimit::headers::RateLimitHeaders;
use crate::policy::ratelimit::key::{ResourceRef, ScopeContext};
use crate::policy::{cors, ratelimit};
use crate::proxy::body::{self, BodyError};
use crate::proxy::constants::{MAX_BODY_BYTES, PROXY_PATH_PREFIX, TARGET_HOST_HEADER};
use crate::proxy::context::{self, ParseErrorKind};
use crate::proxy::endpoint::{self, RoundRobinState, SelectedEndpoint};
use crate::proxy::errors::{self, ErrorFields};
use crate::proxy::forward::{self, ForwardError};
use crate::proxy::guards;
use crate::proxy::headers as hdr;
use crate::proxy::hierarchy::TenantHierarchyProvider;
use crate::proxy::merge::{self, EffectiveConfig};
use crate::proxy::observe::{self, ObserveOutcome};
use crate::proxy::resolve::{self, AliasResolution};
use crate::proxy::route_match::{self, RouteResolution};
use crate::proxy::stream;
use crate::store::OagwState;

/// Everything the engine needs beyond the request itself, injected as
/// extensions by `crate::api::rest::proxy`.
pub(crate) struct ProxyDeps<'a> {
    pub state: &'a OagwState,
    pub hierarchy: &'a dyn TenantHierarchyProvider,
    pub http_client: &'a HttpClient,
    pub round_robin: &'a RoundRobinState,
}

/// Per-request bookkeeping carried through to
/// `cpt-cf-oagw-algo-proxy-observe-request` at every exit point.
struct RequestScope {
    request_id: String,
    start: Instant,
    tenant_id: Uuid,
    principal_id: Uuid,
    method: String,
    host: Option<String>,
    path: Option<String>,
    /// RF-004: the matched route's fixed `http_match.path` template, set
    /// once a route is matched -- the bounded-cardinality value
    /// `ProxyMetrics` is keyed on, distinct from `path` above (the
    /// guard-resolved, caller-suffix-bearing outbound path, still used for
    /// the audit-log record and error `instance`/`path` fields).
    route_template: Option<String>,
    /// RF-003: headers computed by an *allowed* rate-limit evaluation,
    /// applied onto every response this request ultimately produces in
    /// [`Self::finish`] -- the "attach `X-RateLimit-*` to the allowed
    /// response" half of the documented contract that
    /// `crate::policy::ratelimit::RateLimitOutcome::Continue` previously
    /// had no way to carry.
    rate_limit_headers: Option<RateLimitHeaders>,
}

impl RequestScope {
    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-expose
    fn error_fields(&self, instance: &str) -> ErrorFields {
        // `trace_id` here is the same `request_id` `observe_completion` logs,
        // so a client-visible error can be joined to its audit log line.
        ErrorFields {
            instance: instance.to_owned(),
            upstream_id: None,
            host: self.host.clone(),
            path: self.path.clone(),
            trace_id: Some(self.request_id.clone()),
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-expose

    /// The context DECOMPOSITION entry 2.6's `stream::StreamAuditContext`
    /// needs to independently record a `StreamAborted` condition, possibly
    /// long after this request's own [`Self::finish`] call has already run
    /// and returned the initial commit/handshake response to the caller.
    fn stream_audit_context(&self, instance: &str) -> stream::StreamAuditContext {
        stream::StreamAuditContext {
            request_id: self.request_id.clone(),
            tenant_id: Some(self.tenant_id.to_string()),
            principal_id: Some(self.principal_id.to_string()),
            host: self.host.clone(),
            path: self.path.clone(),
            method: Some(self.method.clone()),
            instance: instance.to_owned(),
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-duration
    // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-host-path
    fn finish(&self, mut response: Response, error_type: Option<&str>) -> Response {
        // RF-003: decorate every response this request produces with the
        // `X-RateLimit-*` headers computed when its budget was evaluated
        // (allowed or not -- a caller benefits from seeing its remaining
        // budget even on a downstream failure that happens afterward).
        if let Some(headers) = &self.rate_limit_headers {
            headers.apply(response.headers_mut());
        }
        let status = response.status().as_u16();
        let duration_ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        // `self.host`/`self.path` are set from the upstream alias and the
        // effective outbound path, never the inbound `/oagw/v1/proxy/...`
        // path, keeping label/log cardinality bounded.
        observe::observe_completion(
            &self.request_id,
            &ObserveOutcome {
                tenant_id: Some(self.tenant_id.to_string()),
                principal_id: Some(self.principal_id.to_string()),
                host: self.host.clone(),
                path: self.path.clone(),
                route_template: self.route_template.clone(),
                method: Some(self.method.clone()),
                status: Some(status),
                duration_ms: Some(duration_ms),
                error_type: error_type.map(str::to_owned),
            },
        );
        // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-host-path
        // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-duration
        // @cpt-begin:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-in-flight
        if let Some(host) = &self.host {
            observe::ProxyMetrics::global().exit_in_flight(host);
        }
        // @cpt-end:cpt-cf-oagw-algo-proxy-observe-request:p2:inst-proxy-obs-in-flight
        response
    }
}

/// `cpt-cf-oagw-flow-proxy-request-forwarded` and its sibling flows: the
/// single Data Plane request path entry point.
// @cpt-flow:cpt-cf-oagw-flow-proxy-request-forwarded:p1
// @cpt-dod:cpt-cf-oagw-dod-proxy-endpoint-registration:p1
// @cpt-dod:cpt-cf-oagw-dod-proxy-cross-cutting:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-extension-points:p2
// @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-receive
// @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-dispatch
// This single entry point is also step 1 of both DECOMPOSITION entry 2.6
// flows: the Application Developer's SSE-producing request
// (`cpt-cf-oagw-flow-stream-sse-consumption` `inst-stream-sse-consumption-01`)
// and their `Upgrade: websocket` request
// (`cpt-cf-oagw-flow-stream-websocket-session` `inst-stream-websocket-session-01`)
// both arrive here identically to a plain request -- as does DECOMPOSITION
// entry 2.9's credentialed-proxy flow's own step 1
// (`cpt-cf-oagw-flow-plugin-credentialed-proxy` `inst-flow-cred-proxy-01`).
// @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-01
// @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-01
// @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-01
pub(crate) async fn handle_proxy_request(
    deps: ProxyDeps<'_>,
    ctx: &SecurityContext,
    req: Request<Body>,
) -> Response {
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-01
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-01
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-01
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-dispatch
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-receive
    // DECOMPOSITION entry 2.6's WebSocket branch below moves `parts` into
    // `stream::forward_and_upgrade_websocket`, which extracts
    // `axum::extract::ws::WebSocketUpgrade` from it (removing
    // `hyper::upgrade::OnUpgrade` from `parts.extensions`) -- reached only
    // after every step through endpoint selection runs identically to a
    // plain request.
    let (parts, body) = req.into_parts();
    let instance = parts.uri.path().to_owned();

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-correlate
    let request_id = observe::correlate(&parts.headers);
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-correlate

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-preflight-hook
    if let CorsOutcome::ShortCircuit(response) =
        cors::preflight_fast_path(&parts.method, &parts.headers)
    {
        return response;
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-preflight-hook

    let mut scope = RequestScope {
        request_id: request_id.clone(),
        start: Instant::now(),
        tenant_id: ctx.subject_tenant_id(),
        principal_id: ctx.subject_id(),
        method: observe::normalize_method(&parts.method).to_owned(),
        host: None,
        path: None,
        route_template: None,
        rate_limit_headers: None,
    };

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-parse
    let rest = parts
        .uri
        .path()
        .strip_prefix(PROXY_PATH_PREFIX)
        .unwrap_or("");
    let query = parts.uri.query();
    let parsed = match context::parse_request(
        scope.tenant_id,
        scope.principal_id,
        parts.method.clone(),
        rest,
        query,
        &parts.headers,
    ) {
        Ok(parsed) => {
            scope.principal_id = parsed.principal_id;
            parsed
        }
        Err(error) => {
            let fields = scope.error_fields(&instance);
            let response = match error.kind {
                ParseErrorKind::BadAlias => errors::bad_alias(&fields, &error.detail),
                ParseErrorKind::Validation => errors::validation_error(&fields, &error.detail),
            };
            return scope.finish(response, Some("ValidationError"));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-parse

    // `cpt-cf-oagw-flow-stream-sse-consumption` step 2 / `cpt-cf-oagw-flow-stream-websocket-session`
    // steps 2-4: alias resolution, route matching and config merge run here
    // identically to a plain request; header processing and opening the
    // outbound connection follow below in `forward_and_relay`/
    // `forward_and_upgrade_websocket` (`inst-stream-sse-consumption-02`). A
    // resolution failure returned here is also
    // `inst-stream-websocket-session-03`/`-04`: no upgrade has been
    // attempted yet, so the identical ordinary gateway error a plain
    // request would receive is rendered.
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-02
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-resolve
    let (alias_res, route_res) = match resolve_and_match(&deps, &parsed) {
        // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-05
        Ok(pair) => pair,
        // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-05
        Err(response) => {
            // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-03
            let is_disabled = response.status().as_u16() == 503;
            let error_type = if is_disabled {
                "LinkUnavailable"
            } else {
                "RouteNotFound"
            };
            // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-observe
            // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-return
            // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-observe
            // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-return
            // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-04
            return scope.finish(response, Some(error_type));
            // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-04
            // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-return
            // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-observe
            // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-return
            // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-observe
            // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-03
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-resolve

    let selected_upstream = alias_res.selected().upstream.clone();
    scope.host = selected_upstream.alias.clone();

    // `cpt-cf-oagw-flow-plugin-credentialed-proxy` step 2
    // (`inst-flow-cred-proxy-02`): the upstream, route and merged effective
    // configuration this entry consumes (the single `auth` binding plus the
    // concatenated guard/transform bindings), resolved above and merged
    // here.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-02
    let proxy_timeout_secs = deps.state.store.config().proxy_timeout_secs;
    let effective =
        merge::merge_config(&alias_res.chain, &route_res.route, ctx, proxy_timeout_secs);
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-02
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-02

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-policy-hook
    if let CorsOutcome::ShortCircuit(response) =
        cors::validate_origin_and_method(effective.cors.as_ref(), &parsed.headers, &parts.method)
    {
        return scope.finish(response, Some("CorsRejected"));
    }
    // RF-003: drive the real rate-limit engine with the fully-merged
    // `EffectiveRateLimit` (scope/strategy/burst/algorithm/cost) and a
    // genuine `ScopeContext` -- the resolved Upstream/Route identity, the
    // caller's tenant/principal, and the best-effort client IP -- instead
    // of the previous hardcoded `scope: tenant`/`strategy: reject` adapter.
    let scope_ctx = ScopeContext {
        contributing_resource: match &effective.rate_limit {
            Some(plan) if plan.from_route => {
                ResourceRef::Route(route_res.route.id.unwrap_or_default())
            }
            _ => ResourceRef::Upstream(selected_upstream.id.unwrap_or_default()),
        },
        route_id: route_res.route.id.unwrap_or_default(),
        tenant_id: scope.tenant_id,
        principal_id: scope.principal_id,
        client_ip: client_ip_from_headers(&parsed.headers),
    };
    match ratelimit::evaluate_budget(
        effective.rate_limit.as_ref().map(|plan| &plan.effective),
        &scope_ctx,
    ) {
        RateLimitOutcome::ShortCircuit(response) => {
            return scope.finish(response, Some("RateLimitExceeded"));
        }
        RateLimitOutcome::Continue(headers) => scope.rate_limit_headers = headers,
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-policy-hook

    let http_match = match &route_res.route.route_match.http {
        Some(http) => http,
        None => {
            let response = errors::route_not_found(&scope.error_fields(&instance));
            return scope.finish(response, Some("RouteNotFound"));
        }
    };
    // RF-004: the bounded-cardinality metrics label -- the route's own
    // fixed template, never the guard-resolved outbound path computed
    // below (which folds in the caller-supplied `path_suffix`).
    scope.route_template = Some(http_match.path.clone());
    let path_expr = route_match::normalize_path_expr(&parsed.path_suffix);

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-guards
    let guarded = match guards::apply_guard_rules(
        http_match,
        route_res.matched_prefix_len,
        &parsed.path_suffix,
        &path_expr,
        &parsed.query_pairs,
    ) {
        Ok(guarded) => guarded,
        Err(error) => {
            let response = errors::validation_error(&scope.error_fields(&instance), &error.0);
            return scope.finish(response, Some("ValidationError"));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-guards
    scope.path = Some(guarded.outbound_path.clone());

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-validate-body
    let body_bytes = match validate_and_buffer_body(&parsed.headers, body).await {
        Ok(bytes) => bytes,
        Err(response) => {
            let error_type = if response.status().as_u16() == 413 {
                "PayloadTooLarge"
            } else {
                "ValidationError"
            };
            return scope.finish(response, Some(error_type));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-validate-body

    if let Some(host) = &scope.host {
        observe::ProxyMetrics::global().enter_in_flight(host);
    }

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-select-endpoint
    let target_host_header = parsed
        .headers
        .get(TARGET_HOST_HEADER)
        .and_then(|v| v.to_str().ok());
    let selection =
        match endpoint::select_endpoint(&selected_upstream, target_host_header, deps.round_robin) {
            Ok(selection) => selection,
            Err(target_error) => {
                let mut fields = scope.error_fields(&instance);
                fields.upstream_id = selected_upstream.id.map(|id| id.to_string());
                let alias = selected_upstream.alias.clone().unwrap_or_default();
                let response = errors::target_host_error(&fields, &alias, &target_error);
                return scope.finish(response, Some("TargetHostError"));
            }
        };
    // @cpt-begin:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-metrics
    observe::ProxyMetrics::global().record_endpoint_selected(
        scope.host.as_deref().unwrap_or("unknown"),
        &selection.endpoint.host,
        selection.method,
    );
    // @cpt-end:cpt-cf-oagw-algo-proxy-select-endpoint:p2:inst-proxy-ep-metrics
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-select-endpoint

    let inbound_headers = parsed.headers.clone();

    // @cpt-flow:cpt-cf-oagw-flow-stream-websocket-session:p1
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-02
    // Every step above this point (alias resolution, route matching,
    // config merge, CORS/rate-limit, guards) already ran identically to a
    // plain request; a resolution failure returned early through one of
    // the branches above and never reaches this point at all
    // (`inst-stream-websocket-session-03`/`-04`).
    if stream::is_websocket_upgrade_request(&parsed.method, &inbound_headers) {
        return forward_and_upgrade_websocket(
            deps,
            &mut scope,
            &instance,
            &selected_upstream,
            &effective,
            &inbound_headers,
            selection,
            guarded,
            parts,
        )
        .await;
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-02

    forward_and_relay(
        deps,
        &mut scope,
        &instance,
        &selected_upstream,
        &effective,
        ctx,
        &inbound_headers,
        &parsed.method,
        selection,
        guarded,
        body_bytes,
    )
    .await
}

/// `cpt-cf-oagw-algo-proxy-read-resolved-config`: resolve the alias and
/// match the route in one call, mapping every non-match outcome to the
/// documented flow (`cpt-cf-oagw-flow-proxy-alias-unresolved` /
/// `cpt-cf-oagw-flow-proxy-upstream-disabled`).
// @cpt-algo:cpt-cf-oagw-algo-proxy-read-resolved-config:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-config-read-path:p2
// @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-cp-call
// @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-cp-merge
// `Response`'s `Err` payload trips `clippy::result_large_err` on a
// management-plane-style, never-hot-looped call; boxing it would only add
// an allocation for no benefit, matching the precedent in
// `api::rest::upstreams::handlers`'s own `OagwError`/`OagwProblem` case.
#[allow(clippy::result_large_err)]
fn resolve_and_match(
    deps: &ProxyDeps<'_>,
    parsed: &context::ProxyRequestContext,
) -> Result<(AliasResolution, RouteResolution), Response> {
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-cp-merge
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-cp-call

    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-keys
    // The documented cache key set: `upstream:{tenant_id}:{alias}` (route
    // keying is folded into `route_match::match_route`'s own tenant-scoped
    // walk below).
    let cache_key = format!("upstream:{}:{}", parsed.tenant_id, parsed.alias);
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-keys

    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-if-hit
    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-hit
    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-else
    // This round's Data-Plane L1 *is* the shared `ConfigStore`'s lock-free
    // `DashMap` access (`cpt-cf-oagw-dod-config-store-contract`, entry 2.1):
    // every read is already O(1) and always reflects the latest
    // Control-Plane write in-process, so there is no separate hit/miss
    // state to track, and no distinct "populate on miss" step exists below
    // -- every read through `cache_key` is structurally a hit against live
    // data, never a stale one.
    tracing::trace!(%cache_key, "oagw: resolving proxy request against the live config store");
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-else
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-hit
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-if-hit

    // @cpt-flow:cpt-cf-oagw-flow-proxy-alias-unresolved:p1
    // @cpt-flow:cpt-cf-oagw-flow-proxy-upstream-disabled:p1
    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-receive
    // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-receive
    // Steps 1-5 of `cpt-cf-oagw-flow-proxy-request-forwarded` (parse, the
    // preflight hook, correlation) already ran in `handle_proxy_request`
    // before this function is called.
    let instance = format!("{PROXY_PATH_PREFIX}{}", parsed.alias);
    let fields = ErrorFields {
        instance,
        ..Default::default()
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-receive
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-receive

    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-resolve
    // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-resolve
    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-if-no-upstream
    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-map-no-upstream
    let alias_res =
        resolve::resolve_alias(deps.state, deps.hierarchy, parsed.tenant_id, &parsed.alias)
            .ok_or_else(|| errors::route_not_found(&fields))?;
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-map-no-upstream
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-if-no-upstream
    // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-resolve
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-resolve

    // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-if-disabled
    // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-abandon
    // @cpt-begin:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-map
    if !alias_res.effective_enabled {
        return Err(errors::upstream_disabled(&fields));
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-map
    // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-abandon
    // @cpt-end:cpt-cf-oagw-flow-proxy-upstream-disabled:p1:inst-proxy-dis-if-disabled

    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-if-no-route
    // @cpt-begin:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-map-no-route
    let route_res = route_match::match_route(
        deps.state,
        &alias_res.chain,
        &parsed.method,
        &parsed.path_suffix,
    )
    .ok_or_else(|| errors::route_not_found(&fields))?;
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-map-no-route
    // @cpt-end:cpt-cf-oagw-flow-proxy-alias-unresolved:p1:inst-proxy-nf-if-no-route

    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-populate
    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-no-response-cache
    // @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-return
    // No separate population step exists (the value just computed *is* the
    // live store's current state, see `inst-proxy-read-if-hit`), and this
    // function returns only configuration -- never an upstream response
    // body or status -- per `cpt-cf-oagw-principle-no-cache`.
    Ok((alias_res, route_res))
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-return
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-no-response-cache
    // @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-populate
}

/// Explicit cache-invalidation entry point
/// (`cpt-cf-oagw-algo-proxy-read-resolved-config`'s
/// `inst-proxy-read-invalidation`): a no-TTL cache never depends on
/// expiry for correctness, so a Control-Plane configuration write must be
/// able to flush it explicitly. This round has no separate Data-Plane
/// cache to flush (`resolve_and_match` always reads the live
/// `ConfigStore`, per `inst-proxy-read-if-hit`'s reasoning), so this is a
/// deliberate no-op that documents -- and reserves -- the entry point a
/// future round's real cache would wire a configuration write to call.
// @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-invalidation
#[allow(dead_code)]
pub(crate) fn flush_resolved_config_cache() {}
// @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-invalidation

// @cpt-begin:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-phase-metric
// Recorded as a `phase` label value on `oagw_request_duration_seconds` so
// resolution cost stays separable from upstream time; this round's
// resolution cost is the `resolve_and_match` call itself, timed by the
// caller's overall `RequestScope` duration since there is no separate
// cache-hit/miss branch to distinguish (`inst-proxy-read-if-hit`).
#[allow(dead_code)]
const RESOLUTION_PHASE_LABEL: &str = "resolve";
// @cpt-end:cpt-cf-oagw-algo-proxy-read-resolved-config:p2:inst-proxy-read-phase-metric

/// `cpt-cf-oagw-algo-proxy-validate-body`: header pre-check before any byte
/// is buffered, then buffer up to the hard limit and verify the actual
/// length.
#[allow(clippy::result_large_err)]
async fn validate_and_buffer_body(headers: &HeaderMap, body: Body) -> Result<Bytes, Response> {
    let instance_fields = ErrorFields::default();
    let declared = body::precheck_headers(headers)
        .map_err(|error| body_error_response(&instance_fields, error))?;

    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-te-meter
    // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-limit-fixed
    // `to_bytes` meters the stream as it reads (chunked or not) and aborts
    // with a `LengthLimitError` as soon as `MAX_BODY_BYTES` -- the fixed
    // platform constraint no configuration can raise -- is exceeded, so an
    // oversized body is never fully buffered.
    match to_bytes(body, MAX_BODY_BYTES).await {
        // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-limit-fixed
        // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-te-meter
        Ok(bytes) => {
            // @cpt-begin:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-passthrough
            body::check_actual_length(declared, bytes.len())
                .map_err(|error| body_error_response(&instance_fields, error))?;
            Ok(bytes)
            // @cpt-end:cpt-cf-oagw-algo-proxy-validate-body:p2:inst-proxy-body-passthrough
        }
        Err(read_error) => {
            let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&read_error);
            let too_large = loop {
                match source {
                    Some(cause) if cause.is::<http_body_util::LengthLimitError>() => break true,
                    Some(cause) => source = cause.source(),
                    None => break false,
                }
            };
            if too_large {
                Err(errors::payload_too_large(&instance_fields))
            } else {
                // Uncoded-condition assignment: a request-body read failure
                // that is not a size-limit violation (e.g. a client abort
                // mid-upload) maps to `400 ValidationError`.
                Err(errors::validation_error(
                    &instance_fields,
                    "failed to read the request body",
                ))
            }
        }
    }
}

/// RF-003: best-effort client IP for `scope: ip` rate-limit buckets. This
/// gear has no direct socket-level `ConnectInfo` extraction wired through
/// this round, so the first hop of a caller-supplied `X-Forwarded-For` is
/// the only signal available; `None` (folded to `ScopeSubject::Ip`'s empty
/// default by `crate::policy::ratelimit::key`) when absent.
fn client_ip_from_headers(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = raw.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

fn body_error_response(fields: &ErrorFields, error: BodyError) -> Response {
    match error {
        BodyError::TooLarge => errors::payload_too_large(fields),
        BodyError::Validation(detail) => errors::validation_error(fields, &detail),
    }
}

/// The plaintext-upstream-connection gate
/// (`cpt-cf-oagw-dod-proxy-plaintext-gate`): checked before any socket is
/// opened, so a refusal never contacts the upstream.
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-allowed
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-layering
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-tls-unaffected
// @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-ssrf-policy
fn plaintext_gate_refuses(
    deps: &ProxyDeps<'_>,
    endpoint: &crate::model::upstream::Endpoint,
) -> bool {
    let config = deps.state.store.config();
    // Read but gate no behaviour this feature implements: the DNS-resolution
    // and IP-pinning controls `ssrf_policy.enabled` would gate are out of
    // scope for this round (`inst-proxy-fw-ssrf-policy`).
    let _ssrf_policy_enabled = config.ssrf_policy.enabled;
    // TLS schemes are unaffected by `allow_http_upstream`
    // (`inst-proxy-fw-tls-unaffected`); this gate does not re-validate
    // whether the scheme was legal to declare on the upstream record, which
    // the management layer already settled (`inst-proxy-fw-plaintext-layering`).
    // A plaintext scheme is allowed through when `allow_http_upstream` is
    // `true` (`inst-proxy-fw-plaintext-allowed`).
    endpoint::is_plaintext_scheme(endpoint.scheme) && !config.allow_http_upstream
}
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-ssrf-policy
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-tls-unaffected
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-layering
// @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-allowed

/// Steps 11-17 of `cpt-cf-oagw-flow-proxy-request-forwarded`: header
/// transform, the pre-call plugin hook, forwarding, relay, the post-call
/// plugin hook and the CORS response-header hook.
#[allow(clippy::too_many_arguments)]
async fn forward_and_relay(
    deps: ProxyDeps<'_>,
    scope: &mut RequestScope,
    instance: &str,
    upstream: &Arc<crate::model::upstream::Upstream>,
    effective: &EffectiveConfig,
    ctx: &SecurityContext,
    inbound_headers: &HeaderMap,
    method: &Method,
    selection: SelectedEndpoint,
    guarded: guards::GuardedRequest,
    body_bytes: Bytes,
) -> Response {
    let endpoint = selection.endpoint;

    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-plaintext
    // @cpt-begin:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-refused
    if plaintext_gate_refuses(&deps, &endpoint) {
        let mut fields = scope.error_fields(instance);
        fields.upstream_id = upstream.id.map(|id| id.to_string());
        let response = errors::forward_error(&fields, &ForwardError::PlaintextRefused);
        return scope.finish(response, Some("ProtocolError"));
    }
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-plaintext-refused
    // @cpt-end:cpt-cf-oagw-algo-proxy-forward-request:p2:inst-proxy-fw-if-plaintext

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-transform-headers
    let authority = endpoint::endpoint_authority(&endpoint);
    let outbound_headers = hdr::transform_request_headers(
        inbound_headers,
        effective.headers.as_ref(),
        &authority,
        Some(body_bytes.len()),
        false,
    );
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-transform-headers

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-plugin-request-hook
    // RF-001: the real chain -- Auth, then Guards, then Transform
    // `on_request` -- over the fully-merged bindings, replacing the two
    // narrow adapters (`plugins::chain::run_pre_call`/`run_post_call`,
    // retired) that only ever resolved identifiers and ran the zero-config
    // `request_id` transform.
    // @cpt-flow:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-03
    let runtime = chain_runtime(deps.state.store.config().token_cache_capacity);
    let auth_binding = effective.auth.as_ref().map(|auth| {
        PluginBinding::new(
            auth.auth_type.clone().unwrap_or_default(),
            auth.config.clone(),
        )
    });
    let plan = match assemble_chain(
        auth_binding.as_ref(),
        &effective.plugins,
        &[],
        &runtime.registries,
    ) {
        Ok(plan) => plan,
        Err(err) => {
            let response = execute::resolution_failure_response(&err);
            return scope.finish(response, Some("PluginRejected"));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-03

    let gear_config = deps.state.store.config();
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-04
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-05
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-06
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-07
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-08
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-09
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-10
    let (outbound_headers, effective_request_id) = match execute::execute_pre_call(
        &plan,
        ctx,
        &scope.request_id,
        outbound_headers,
        runtime.credstore.as_ref(),
        &runtime.token_cache,
        gear_config.token_cache_ttl_secs,
        effective.timeout_secs,
    )
    .await
    {
        PreCallOutcome::Continue {
            headers,
            request_id,
        } => (headers, request_id),
        PreCallOutcome::ShortCircuit(response) => {
            return scope.finish(response, Some("PluginRejected"));
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-10
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-09
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-08
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-07
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-06
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-05
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-04
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-plugin-request-hook

    let scheme = if endpoint::is_plaintext_scheme(endpoint.scheme) {
        "http"
    } else {
        "https"
    };
    let url = forward::compose_url(
        scheme,
        &endpoint.host,
        endpoint.port,
        &guarded.outbound_path,
        &guarded.outbound_query,
    );

    // @cpt-dod:cpt-cf-oagw-dod-proxy-error-source-split:p1
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-forward
    // `cpt-cf-oagw-flow-plugin-credentialed-proxy` step 9
    // (`inst-flow-cred-proxy-11`): forward the credentialed, guarded,
    // transformed request to the upstream service.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-11
    let forward_result = forward::forward_request(
        deps.http_client,
        method,
        &url,
        &endpoint.host,
        endpoint.port,
        outbound_headers,
        body_bytes,
        effective.timeout_secs,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-11
    // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-forward

    let outcome = match forward_result {
        Ok(outcome) => outcome,
        Err(forward_error) => {
            // RF-001 `inst-flow-cred-proxy-14`/`-15`: the upstream call
            // itself failed -- run the `on_error` transforms over the
            // error context (a no-op today: neither built-in transform
            // declares `on_error`), leaving the error's status and GTS
            // `type` exactly as raised.
            // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-14
            // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-15
            let _ = execute::execute_on_error(&plan);
            // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-15
            // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-14
            let mut fields = scope.error_fields(instance);
            fields.upstream_id = upstream.id.map(|id| id.to_string());
            fields.host = Some(endpoint.host.clone());
            let error_name = forward_error_name(&forward_error);
            let response = errors::forward_error(&fields, &forward_error);
            return scope.finish(response, Some(error_name));
        }
    };

    match outcome {
        forward::ForwardOutcome::Buffered(upstream_response) => {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-relay
            let mut response = relay_response(upstream_response, effective.headers.as_ref());
            // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-relay

            // @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-headers
            cors::inject_response_headers(effective.cors.as_ref(), inbound_headers, &mut response);
            // @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-headers

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-plugin-response-hook
            // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-12
            // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-13
            let (response, error_type) = apply_post_call(&plan, &effective_request_id, response);
            // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-13
            // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-12
            // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-plugin-response-hook

            // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-observe
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-return
            // `cpt-cf-oagw-flow-plugin-credentialed-proxy` step 13
            // (`inst-flow-cred-proxy-17`): return the upstream's response to
            // the developer unchanged except for the transforms already
            // applied by `apply_post_call` above.
            // @cpt-begin:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-17
            scope.finish(response, error_type)
            // @cpt-end:cpt-cf-oagw-flow-plugin-credentialed-proxy:p1:inst-flow-cred-proxy-17
            // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-return
            // @cpt-end:cpt-cf-oagw-flow-proxy-request-forwarded:p1:inst-proxy-fwd-observe
        }
        // @cpt-flow:cpt-cf-oagw-flow-stream-sse-consumption:p1
        // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-04
        // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-13
        // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-14
        forward::ForwardOutcome::Stream(streaming) => {
            let audit_ctx = scope.stream_audit_context(instance);
            match stream::relay_event_stream(streaming, effective.headers.as_ref(), audit_ctx).await
            {
                stream::SseOutcome::Streaming(mut response) => {
                    cors::inject_response_headers(
                        effective.cors.as_ref(),
                        inbound_headers,
                        &mut response,
                    );
                    let (response, error_type) =
                        apply_post_call(&plan, &effective_request_id, response);
                    scope.finish(response, error_type)
                }
                stream::SseOutcome::PreCommitAborted(response) => {
                    scope.finish(response, Some("StreamAborted"))
                }
            }
        } // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-14
          // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-13
          // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-04
    }
}

/// WebSocket counterpart to [`forward_and_relay`]: applies the identical
/// plaintext gate and header-transform steps
/// (`cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation`
/// `inst-stream-websocket-negotiation-02`), then hands off to
/// `stream::forward_and_upgrade_websocket` for the handshake and frame
/// relay. Never runs the plugin pre/post-call hooks or the CORS
/// response-header injection: those are connection-open-time checks
/// already applied above (`cpt-cf-oagw-feature-proxy-streaming`'s
/// cross-cutting Security note), and a `101` response carries no
/// meaningful CORS surface.
#[allow(clippy::too_many_arguments)]
async fn forward_and_upgrade_websocket(
    deps: ProxyDeps<'_>,
    scope: &mut RequestScope,
    instance: &str,
    upstream: &Arc<crate::model::upstream::Upstream>,
    effective: &EffectiveConfig,
    inbound_headers: &HeaderMap,
    selection: SelectedEndpoint,
    guarded: guards::GuardedRequest,
    parts: axum::http::request::Parts,
) -> Response {
    let endpoint = selection.endpoint;

    if plaintext_gate_refuses(&deps, &endpoint) {
        let mut fields = scope.error_fields(instance);
        fields.upstream_id = upstream.id.map(|id| id.to_string());
        let response = errors::forward_error(&fields, &ForwardError::PlaintextRefused);
        return scope.finish(response, Some("ProtocolError"));
    }

    let authority = endpoint::endpoint_authority(&endpoint);
    let outbound_headers = hdr::transform_request_headers(
        inbound_headers,
        effective.headers.as_ref(),
        &authority,
        None,
        true,
    );

    let audit_ctx = scope.stream_audit_context(instance);
    let outcome = stream::forward_and_upgrade_websocket(
        parts,
        outbound_headers,
        endpoint,
        &guarded.outbound_path,
        &guarded.outbound_query,
        effective.timeout_secs,
        audit_ctx,
    )
    .await;

    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-16
    match outcome {
        stream::WebSocketOutcome::Upgraded(response) => scope.finish(response, None),
        stream::WebSocketOutcome::HandshakeFailed(response, error_name) => {
            scope.finish(response, Some(error_name))
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-16
}

/// RF-001: run the post-call half of the plugin chain (response-phase
/// guards, then `on_response` transforms) over an already-relayed
/// response, mapping `PostCallOutcome::ShortCircuit`'s `502` rejection onto
/// the same `(Response, error_type)` shape [`RequestScope::finish`]'s every
/// other exit point uses, so a response-phase guard rejection is
/// observable exactly like any other proxy-path failure.
fn apply_post_call(
    plan: &crate::plugins::plan::ExecutionPlan,
    request_id: &str,
    mut response: Response,
) -> (Response, Option<&'static str>) {
    match execute::execute_post_call(plan, request_id, &mut response) {
        PostCallOutcome::Continue => (response, None),
        PostCallOutcome::ShortCircuit(rejection) => (rejection, Some("PluginRejected")),
    }
}

fn forward_error_name(error: &ForwardError) -> &'static str {
    match error {
        ForwardError::PlaintextRefused => "ProtocolError",
        ForwardError::ConnectionTimeout => "ConnectionTimeout",
        ForwardError::RequestTimeout => "RequestTimeout",
        ForwardError::IdleTimeout => "IdleTimeout",
        ForwardError::LinkUnavailable => "LinkUnavailable",
        ForwardError::ProtocolError => "ProtocolError",
        ForwardError::DownstreamBodyTooLarge => "DownstreamError",
    }
}

/// `cpt-cf-oagw-algo-proxy-relay-response`: carry the upstream status
/// unchanged, transform the response headers, recompute `Content-Length`
/// to the delivered bytes and set the upstream error-source header.
// @cpt-algo:cpt-cf-oagw-algo-proxy-relay-response:p2
// @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-status
// @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-body
// @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-error-source
// @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-no-envelope
// @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-no-store
fn relay_response(
    upstream: forward::UpstreamResponse,
    headers_config: Option<&crate::model::upstream::HeadersConfig>,
) -> Response {
    let mut response_headers = hdr::transform_response_headers(&upstream.headers, headers_config);
    response_headers.remove(axum::http::header::CONTENT_LENGTH);
    if let Ok(value) = axum::http::HeaderValue::from_str(&upstream.body.len().to_string()) {
        response_headers.insert(axum::http::header::CONTENT_LENGTH, value);
    }
    response_headers.insert(
        axum::http::HeaderName::from_static(crate::error::ERROR_SOURCE_HEADER_NAME),
        axum::http::HeaderValue::from_static(crate::proxy::constants::ERROR_SOURCE_UPSTREAM),
    );

    // @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-return
    // @cpt-begin:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-stream-hook
    // DECOMPOSITION entry 2.6: this function only ever receives
    // `forward::ForwardOutcome::Buffered` (a complete, non-event-stream
    // response) -- `forward_and_relay`'s `match` on the outcome is this
    // hook's real branch point, dispatching a `Stream` outcome to
    // `stream::relay_event_stream` for incremental relay and the
    // `StreamAborted` handling instead of calling this function at all.
    let mut response = (upstream.status, upstream.body).into_response();
    *response.headers_mut() = response_headers;
    response
    // @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-stream-hook
    // @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-return
}
// @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-no-store
// @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-no-envelope
// @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-error-source
// @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-body
// @cpt-end:cpt-cf-oagw-algo-proxy-relay-response:p2:inst-proxy-relay-status

/// End-to-end tests for DECOMPOSITION entry 2.6 (streaming and protocol
/// upgrades), driven through the real `engine::handle_proxy_request` entry
/// point exactly as `crate::api::rest::proxy`'s own inline test module
/// drives entry 2.5 -- this module is not reachable from an external
/// `tests/*.rs` crate (`crate::proxy::*` is `pub(crate)`), so these live
/// here, the file that owns the extension points this feature fills.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod streaming_tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::route::{HttpMatch, HttpMethod, PathSuffixMode, Route, RouteMatch};
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig, Upstream};
    use crate::proxy::forward::LazyHttpClient;
    use crate::proxy::hierarchy::NoTenantHierarchy;
    use axum::body::Body as AxumBody;
    use axum::http::{Request as HttpRequest, StatusCode};
    use axum::routing::get;
    use axum::{Extension, Router};
    use http_body_util::BodyExt;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn ctx(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap()
    }

    fn build_state(proxy_timeout_secs: u32) -> Arc<OagwState> {
        Arc::new(OagwState::new(OagwConfig {
            proxy_timeout_secs,
            allow_http_upstream: true,
            ..OagwConfig::default()
        }))
    }

    async fn test_handle(
        Extension(state): Extension<Arc<OagwState>>,
        Extension(hierarchy): Extension<Arc<dyn TenantHierarchyProvider>>,
        Extension(http_client): Extension<Arc<LazyHttpClient>>,
        Extension(round_robin): Extension<Arc<RoundRobinState>>,
        Extension(ctx): Extension<SecurityContext>,
        req: Request<AxumBody>,
    ) -> Response {
        let deps = ProxyDeps {
            state: &state,
            hierarchy: hierarchy.as_ref(),
            http_client: http_client.client(),
            round_robin: &round_robin,
        };
        handle_proxy_request(deps, &ctx, req).await
    }

    /// Router wiring mirroring `crate::api::rest::proxy::register_routes`
    /// (that module is outside this entry's file-ownership list, so this is
    /// this feature's own equivalent test-only wiring, not a duplicate of
    /// production routing logic).
    ///
    /// `tenant_id` is injected as a fixed `SecurityContext` extension layer
    /// rather than per-request: a real hyper connection (used by the
    /// WebSocket tests below, so that `hyper::upgrade::OnUpgrade` is
    /// actually present) has no way for a test to attach a Rust-typed
    /// extension to an individual inbound request the way `oneshot`'s
    /// hand-built `Request` can.
    fn build_router(state: Arc<OagwState>, tenant_id: Uuid) -> Router {
        Router::new()
            .route("/oagw/v1/proxy/{*rest}", get(test_handle))
            .layer(Extension(ctx(tenant_id)))
            .layer(Extension(Arc::new(RoundRobinState::new())))
            .layer(Extension(Arc::new(LazyHttpClient::default())))
            .layer(Extension(
                Arc::new(NoTenantHierarchy) as Arc<dyn TenantHierarchyProvider>
            ))
            .layer(Extension(state))
    }

    fn seed_upstream(
        state: &OagwState,
        tenant_id: Uuid,
        alias: &str,
        scheme: EndpointScheme,
        port: u16,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme,
                    host: "127.0.0.1".to_owned(),
                    port,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    /// A WebSocket-fronting upstream needs `passthrough: All` (or an
    /// allowlist naming them) for `Sec-WebSocket-*` to reach the upstream:
    /// per `cpt-cf-oagw-algo-stream-websocket-upgrade-negotiation` step 2,
    /// those headers "pass through normally under the configured
    /// passthrough mode like any other request header" -- they are not
    /// exempted from that gate the way `Connection`/`Upgrade` are.
    fn seed_websocket_upstream(state: &OagwState, tenant_id: Uuid, alias: &str, port: u16) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(alias.to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Ws,
                    host: "127.0.0.1".to_owned(),
                    port,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: Some(crate::model::upstream::HeadersConfig {
                request: crate::model::upstream::RequestHeaderRules {
                    passthrough: crate::model::upstream::PassthroughMode::All,
                    ..Default::default()
                },
                ..Default::default()
            }),
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    fn seed_route(state: &OagwState, upstream_id: Uuid, path: &str, methods: Vec<HttpMethod>) {
        let route = Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Uuid::new_v4(),
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods,
                    path: path.to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: Some(1),
        };
        state
            .store
            .routes()
            .insert(route.id.unwrap(), Arc::new(route));
    }

    fn get_request(uri: &str) -> HttpRequest<AxumBody> {
        HttpRequest::builder()
            .method("GET")
            .uri(uri)
            .body(AxumBody::empty())
            .unwrap()
    }

    fn http_chunk(data: &[u8]) -> Vec<u8> {
        let mut out = format!("{:x}\r\n", data.len()).into_bytes();
        out.extend_from_slice(data);
        out.extend_from_slice(b"\r\n");
        out
    }

    /// A raw chunked-encoding `text/event-stream` upstream: writes each of
    /// `chunks` with `delay` between them, then a `0\r\n\r\n` terminator
    /// unless `abrupt_close` is set, in which case the connection is
    /// dropped instead of terminating the chunked body -- simulating an
    /// upstream abort mid-stream (`cpt-cf-oagw-algo-stream-abort-handling`).
    async fn spawn_sse_upstream(chunks: Vec<Vec<u8>>, delay: Duration, abrupt_close: bool) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            // `cpt-cf-oagw-algo-proxy-forward-request`'s `probe_connect`
            // opens and immediately drops its own throwaway connection
            // before the real request's connection follows; loop past that
            // empty probe (an immediate `Ok(0)` read) to reach the actual
            // HTTP request instead of answering the probe itself.
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 4096];
                match socket.read(&mut buf).await {
                    Ok(0) | Err(_) => continue,
                    Ok(_) => {}
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await
                    .unwrap();
                socket.flush().await.unwrap();
                for (index, chunk) in chunks.iter().enumerate() {
                    if index > 0 {
                        tokio::time::sleep(delay).await;
                    }
                    if socket.write_all(&http_chunk(chunk)).await.is_err() {
                        return;
                    }
                    let _ = socket.flush().await;
                }
                if abrupt_close {
                    drop(socket);
                } else {
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                    let _ = socket.shutdown().await;
                }
                return;
            }
        });
        port
    }

    /// SSE incrementality proof (`cpt-cf-oagw-dod-stream-sse-detect-and-forward`,
    /// acceptance criterion 1): the response commits well before the
    /// upstream has finished sending every event, and the complete body is
    /// still delivered once the upstream does finish.
    // @cpt-usecase:cpt-cf-oagw-usecase-sse-streaming:p1
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-05
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-06
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-09
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-10
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-15
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sse_response_is_delivered_incrementally_not_only_after_upstream_completes() {
        let port = spawn_sse_upstream(
            vec![b"data: first\n\n".to_vec(), b"data: second\n\n".to_vec()],
            Duration::from_millis(350),
            false,
        )
        .await;
        let state = build_state(5);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id, "sse-svc", EndpointScheme::Http, port);
        seed_route(&state, upstream_id, "/events", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let start = std::time::Instant::now();
        let response = router
            .oneshot(get_request("/oagw/v1/proxy/sse-svc/events"))
            .await
            .unwrap();
        let committed_after = start.elapsed();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert!(
            committed_after < Duration::from_millis(200),
            "the response must commit before the upstream's second (delayed) chunk, took {committed_after:?}"
        );

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let total_elapsed = start.elapsed();
        assert_eq!(body.as_ref(), b"data: first\n\ndata: second\n\n");
        assert!(
            total_elapsed >= Duration::from_millis(320),
            "draining the full body must still wait for the delayed second chunk, took {total_elapsed:?}"
        );
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-15
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-10
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-09
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-06
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-05

    /// A response whose upstream aborts before it ever sends the first
    /// chunk (`Some(Err(_))` on the very first poll) is a pre-commit abort:
    /// `502` `StreamAborted` with `X-OAGW-Error-Source: gateway`
    /// (acceptance criterion 5).
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-11
    // @cpt-begin:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-12
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sse_upstream_abort_before_any_byte_is_committed_renders_stream_aborted_502() {
        let port = spawn_sse_upstream(Vec::new(), Duration::from_millis(0), true).await;
        let state = build_state(5);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(
            &state,
            tenant_id,
            "sse-precommit",
            EndpointScheme::Http,
            port,
        );
        seed_route(&state, upstream_id, "/events", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let response = router
            .oneshot(get_request("/oagw/v1/proxy/sse-precommit/events"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response
                .headers()
                .get(crate::error::ERROR_SOURCE_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(crate::error::ERROR_SOURCE_GATEWAY)
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
        );
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-12
    // @cpt-end:cpt-cf-oagw-flow-stream-sse-consumption:p1:inst-stream-sse-consumption-11

    /// A response whose upstream aborts **after** the first chunk has
    /// already been committed cannot change its status line: the client
    /// connection ends without a second response, and the abort is
    /// recorded in the error metrics (acceptance criterion 6).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sse_upstream_abort_after_commit_ends_the_body_without_a_second_response() {
        let port = spawn_sse_upstream(
            vec![b"data: only\n\n".to_vec()],
            Duration::from_millis(0),
            true,
        )
        .await;
        let state = build_state(5);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(
            &state,
            tenant_id,
            "sse-postcommit",
            EndpointScheme::Http,
            port,
        );
        seed_route(&state, upstream_id, "/events", vec![HttpMethod::Get]);
        let before = observe::ProxyMetrics::global().errors_total(
            "sse-postcommit",
            "/events",
            "StreamAborted",
        );
        let router = build_router(state, tenant_id);

        let response = router
            .oneshot(get_request("/oagw/v1/proxy/sse-postcommit/events"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let collected = response.into_body().collect().await;
        assert!(
            collected.is_err(),
            "a body that aborts mid-stream must surface as a body-read error, not a clean end"
        );

        // Give the mid-stream error frame's audit/metric side effect a
        // moment to run (it fires on the same poll that ends the body).
        tokio::time::sleep(Duration::from_millis(20)).await;
        let after = observe::ProxyMetrics::global().errors_total(
            "sse-postcommit",
            "/events",
            "StreamAborted",
        );
        assert_eq!(after, before + 1);
    }

    /// A completed WebSocket echo round-trip: text and binary frames sent
    /// by the client are relayed to the upstream and echoed back
    /// unmodified, and a client-initiated close code is observed by the
    /// upstream (acceptance criteria 3 and 4). This needs a real hyper
    /// connection (not `oneshot`) so `hyper::upgrade::OnUpgrade` is
    /// actually present on the inbound request.
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-09
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-10
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-11
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-12
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-13
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_echo_round_trip_relays_text_binary_and_propagates_close_code() {
        use tokio_tungstenite::tungstenite::Message as TsMessage;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

        // The fake upstream: echoes text/binary frames, and reports every
        // message it receives (including the client-initiated close and
        // its code) on `observed_tx` -- observing the close server-side is
        // the direct proof that the client's close code was propagated,
        // without depending on `tokio-tungstenite`'s own strict two-sided
        // close-handshake bookkeeping on the *client* connection (this
        // gear's own `cpt-cf-oagw-algo-stream-websocket-frame-relay` step 3
        // only requires propagating the frame once, then closing both
        // sides -- it does not complete a round-trip close handshake).
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((socket, _)) = upstream_listener.accept().await else {
                return;
            };
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            use futures_util::{SinkExt, StreamExt};
            while let Some(Ok(msg)) = ws.next().await {
                let is_close = matches!(msg, TsMessage::Close(_));
                let _ = observed_tx.send(msg.clone());
                if is_close || ws.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let state = build_state(5);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_websocket_upstream(&state, tenant_id, "ws-svc", upstream_port);
        seed_route(&state, upstream_id, "/socket", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(gateway_listener, router).await.unwrap();
        });

        // `build_router`'s fixed `SecurityContext` extension layer (for
        // `tenant_id`) stands in for the platform auth middleware a real
        // hyper connection would otherwise need to inject the tenant.
        let ws_url = format!("ws://{gateway_addr}/oagw/v1/proxy/ws-svc/socket");
        let (mut client_ws, response) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        use futures_util::{SinkExt, StreamExt};
        client_ws.send(TsMessage::text("hello")).await.unwrap();
        let echoed = client_ws.next().await.unwrap().unwrap();
        assert_eq!(echoed, TsMessage::text("hello"));
        assert_eq!(observed_rx.recv().await.unwrap(), TsMessage::text("hello"));

        client_ws
            .send(TsMessage::Binary(bytes::Bytes::from_static(
                b"\x01\x02\x03",
            )))
            .await
            .unwrap();
        let echoed = client_ws.next().await.unwrap().unwrap();
        assert_eq!(
            echoed,
            TsMessage::Binary(bytes::Bytes::from_static(b"\x01\x02\x03"))
        );
        assert_eq!(
            observed_rx.recv().await.unwrap(),
            TsMessage::Binary(bytes::Bytes::from_static(b"\x01\x02\x03"))
        );

        client_ws
            .send(TsMessage::Close(Some(TsCloseFrame {
                code: CloseCode::from(4001),
                reason: "done".into(),
            })))
            .await
            .unwrap();
        match observed_rx.recv().await.unwrap() {
            TsMessage::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4001),
            other => {
                panic!("upstream must observe the client's close code, got {other:?}")
            }
        }
    }

    /// The other direction of close-code propagation: an upstream-initiated
    /// close (with its code) is observed by the client (acceptance
    /// criterion 4, upstream-initiated half).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_upstream_initiated_close_code_is_observed_by_the_client() {
        use tokio_tungstenite::tungstenite::Message as TsMessage;
        use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
        use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((socket, _)) = upstream_listener.accept().await else {
                return;
            };
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            use futures_util::SinkExt;
            let _ = ws
                .send(TsMessage::Close(Some(TsCloseFrame {
                    code: CloseCode::from(4321),
                    reason: "upstream done".into(),
                })))
                .await;
        });

        let state = build_state(5);
        let tenant_id = Uuid::new_v4();
        let upstream_id =
            seed_websocket_upstream(&state, tenant_id, "ws-upstream-close", upstream_port);
        seed_route(&state, upstream_id, "/socket", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(gateway_listener, router).await.unwrap();
        });

        let ws_url = format!("ws://{gateway_addr}/oagw/v1/proxy/ws-upstream-close/socket");
        let (mut client_ws, response) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        use futures_util::StreamExt;
        match client_ws.next().await.unwrap().unwrap() {
            TsMessage::Close(Some(frame)) => assert_eq!(u16::from(frame.code), 4321),
            other => panic!("expected the upstream's close code to be propagated, got {other:?}"),
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-13
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-12
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-11
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-10
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-09

    /// A handshake failure (upstream refuses / non-`101`) before completion
    /// yields an ordinary RFC 9457 gateway error, not a raw connection drop
    /// (acceptance criterion 7).
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-06
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-07
    // @cpt-begin:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-08
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_handshake_failure_before_completion_yields_an_ordinary_gateway_error() {
        // A fake upstream that refuses the handshake outright (closes the
        // connection immediately instead of ever writing a response).
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((socket, _)) = upstream_listener.accept().await {
                drop(socket);
            }
        });

        let state = build_state(2);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(
            &state,
            tenant_id,
            "ws-refusing",
            EndpointScheme::Ws,
            upstream_port,
        );
        seed_route(&state, upstream_id, "/socket", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(gateway_listener, router).await.unwrap();
        });

        let ws_url = format!("ws://{gateway_addr}/oagw/v1/proxy/ws-refusing/socket");
        let error = tokio_tungstenite::connect_async(ws_url).await.unwrap_err();
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                assert!(response.status().is_client_error() || response.status().is_server_error());
                assert_eq!(
                    response
                        .headers()
                        .get(crate::error::ERROR_SOURCE_HEADER_NAME)
                        .and_then(|v| v.to_str().ok()),
                    Some(crate::error::ERROR_SOURCE_GATEWAY)
                );
                let body = response.body().clone().unwrap_or_default();
                let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert!(
                    json["type"]
                        .as_str()
                        .unwrap()
                        .starts_with("gts.cf.core.errors.err.v1~")
                );
            }
            other => {
                panic!("expected an ordinary HTTP error response, got a transport error: {other:?}")
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-08
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-07
    // @cpt-end:cpt-cf-oagw-flow-stream-websocket-session:p1:inst-stream-websocket-session-06

    /// `cpt-cf-oagw-dod-stream-timeout-exemption`: a `proxy_timeout_secs`
    /// far shorter than the SSE stream's total delivery time does not sever
    /// it once the response is committed (acceptance criterion 8, SSE half).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn proxy_timeout_does_not_sever_a_committed_sse_stream() {
        let port = spawn_sse_upstream(
            vec![b"data: a\n\n".to_vec(), b"data: b\n\n".to_vec()],
            Duration::from_millis(1200),
            false,
        )
        .await;
        // A one-second deadline that would sever the stream if it were
        // (incorrectly) applied to the whole transfer instead of only the
        // pre-commit phase.
        let state = build_state(1);
        let tenant_id = Uuid::new_v4();
        let upstream_id =
            seed_upstream(&state, tenant_id, "sse-timeout", EndpointScheme::Http, port);
        seed_route(&state, upstream_id, "/events", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let response = router
            .oneshot(get_request("/oagw/v1/proxy/sse-timeout/events"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = tokio::time::timeout(Duration::from_secs(3), response.into_body().collect())
            .await
            .expect("the stream must eventually complete rather than hang")
            .unwrap()
            .to_bytes();
        assert_eq!(body.as_ref(), b"data: a\n\ndata: b\n\n");
    }

    /// `cpt-cf-oagw-dod-stream-timeout-exemption`'s WebSocket half: a
    /// session that stays open well past `proxy_timeout_secs` is not
    /// unilaterally closed (acceptance criterion 8, WebSocket half).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn proxy_timeout_does_not_sever_an_upgraded_websocket_session() {
        use tokio_tungstenite::tungstenite::Message as TsMessage;

        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((socket, _)) = upstream_listener.accept().await else {
                return;
            };
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            use futures_util::{SinkExt, StreamExt};
            // Hold the session open well past the one-second
            // `proxy_timeout_secs` below before ever sending a frame.
            tokio::time::sleep(Duration::from_millis(1300)).await;
            if let Some(Ok(msg)) = ws.next().await {
                let _ = ws.send(msg).await;
            }
        });

        let state = build_state(1);
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_websocket_upstream(&state, tenant_id, "ws-timeout", upstream_port);
        seed_route(&state, upstream_id, "/socket", vec![HttpMethod::Get]);
        let router = build_router(state, tenant_id);

        let gateway_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_addr = gateway_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(gateway_listener, router).await.unwrap();
        });

        let ws_url = format!("ws://{gateway_addr}/oagw/v1/proxy/ws-timeout/socket");
        let (mut client_ws, response) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);

        use futures_util::{SinkExt, StreamExt};
        client_ws
            .send(TsMessage::text("still-alive"))
            .await
            .unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(3), client_ws.next())
            .await
            .expect("the session must survive past proxy_timeout_secs, not be severed")
            .unwrap()
            .unwrap();
        assert_eq!(echoed, TsMessage::text("still-alive"));
    }
}
