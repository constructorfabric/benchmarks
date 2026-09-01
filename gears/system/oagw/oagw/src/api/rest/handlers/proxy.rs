//! Data-plane handlers of the OAGW proxy surface.
//!
//! `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}]` runs the orchestration of
//! DESIGN section 3.5, in the documented order:
//!
//! 0. a CORS preflight (`OPTIONS` + `Origin` +
//!    `Access-Control-Request-Method`) is answered locally with `204`
//!    (ADR-0004): browser preflights carry no credentials, so they have no
//!    tenant context, no per-request auth and no upstream round-trip. The
//!    configured answer is used when the upstream and a route resolve without a
//!    [`SecurityContext`]; every other preflight gets the permissive answer of
//!    [`crate::domain::cors::unresolved_preflight`];
//! 1. extract the [`SecurityContext`] the platform authenticated;
//! 2. resolve the upstream by alias over the tenant chain (descendant →
//!    ancestor, closest match wins); a disabled upstream is a `503`, an
//!    unknown alias a `404`;
//! 3. resolve the route: method allowlist plus longest path prefix over the
//!    same chain, enabled routes only; a `grpc` upstream answers `501`;
//! 4. merge the effective configuration root → child per sharing mode
//!    (upstream below route);
//! 5. CORS: an actual cross-origin request is validated and rejected with
//!    `403` (its preflight was answered in step 0);
//! 6. rate limit, with the ADR-0003 response headers — before the body is
//!    read, because the decision needs no payload;
//! 7. the request body: a declared `Content-Length` over the cap is a `413`
//!    *before* a single byte is buffered (DESIGN "Body Validation Rules":
//!    reject before buffering), then the body is read and re-validated;
//! 8. the ADR-0002 plugin chain `authenticate` → `guard_request` →
//!    `transform_request`; a binding naming a disabled plugin resource is
//!    skipped, an enabled-but-unresolvable one is a `503`;
//! 9. endpoint selection (ADR-0001) and the outbound call — the headers and
//!    query parameters the chain injected (PRD §5.2 credential injection) ride
//!    on it, overriding their client counterparts — then `guard_response` →
//!    `transform_response` and the response header rules;
//! 10. the proxy log line, the metrics and the ADR-0007
//!     `X-OAGW-Error-Source` stamp (`upstream` on every upstream answer,
//!     `gateway` on every error).
//!
//! ## Residual platform limitation (documented)
//!
//! ADR-0004 wants an unauthenticated preflight to be answered without any
//! authentication. The proxy operations are declared `.authenticated()` in
//! [`crate::api::rest::routes`], which the platform's gateway middleware
//! enforces before the request reaches this handler, so an unauthenticated
//! preflight is answered here only when the platform lets it through (the
//! gear's own router has no per-verb auth gate). The step-0 answer is
//! therefore complete for authenticated callers — including the permissive
//! fallback when the alias does not resolve — and the unauthenticated case is
//! covered as far as the platform allows. Platform files are out of scope for
//! this gear.
//!
//! Nothing here returns `Err` to the framework: every failure goes through
//! [`Failed`], which runs the chain's `transform_error` phase and renders an
//! RFC 9457 problem document with the same header, so a client can always tell
//! whether the gateway or the upstream produced a response.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Extension;
use axum::extract::{ConnectInfo, Path, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::Service;
use crate::domain::audit::{ProxyExchange, ProxyOutcome, log_proxy};
use crate::domain::cors::{
    EffectiveCorsConfig, PreflightResponse, actual_response_headers, evaluate_preflight,
    is_preflight, merge_cors, unresolved_preflight, validate_actual_request,
};
use crate::domain::error::{
    ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM, OagwError, ProblemBody,
};
use crate::domain::metrics::{MetricsRegistry, host_label, route_label};
use crate::domain::model::{
    CorsConfig, Endpoint, HttpMethod, PathSuffixMode, Protocol, RateLimitConfig,
    ResolvedProxyTarget, Route, Scheme, Upstream,
};
use crate::domain::plugin::{
    BodyPayload, CorsOutcome, ErrorContext, PluginChain, RequestContext, ResponseContext,
};
use crate::domain::rate_limit::{
    EffectiveRateLimit, QUEUE_MAX_WAIT, RateLimitReservation, RateLimitScopeValues,
    RateLimiterRegistry, resolve_effective_rate_limit,
};
use crate::infra::plugin::{DisabledPlugins, PluginRegistry};
use crate::infra::proxy::{
    MAX_REQUEST_BODY_BYTES, ProxyBody, ProxyEngine, ProxyRequest, ProxyResponse,
    TARGET_HOST_HEADER, is_websocket_upgrade, validate_declared_body_size,
};
use crate::infra::storage::RegistryStore;

/// GTS error type of the `501` a `grpc` upstream answers with.
const NOT_IMPLEMENTED_TYPE: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.not_implemented.v1";

/// Message `http_body_util::LengthLimitError` renders, which
/// [`axum::body::to_bytes`] surfaces for a body past its limit (see
/// [`body_read_error`]).
const LENGTH_LIMIT_ERROR_MESSAGE: &str = "length limit exceeded";

/// Everything the data-plane handlers need, injected as one `Extension`.
#[derive(Clone)]
pub struct DataPlane {
    /// Registry, validator and tenant hierarchy shared with the management
    /// surface.
    pub service: Service,
    /// Outbound transport (ADR-0006).
    pub engine: Arc<ProxyEngine>,
    /// Metrics the proxy records into.
    pub metrics: Arc<MetricsRegistry>,
    /// Plugin factory (ADR-0002).
    pub plugins: Arc<PluginRegistry>,
    /// Rate-limit buckets (ADR-0003).
    pub rate_limiters: Arc<RateLimiterRegistry>,
}

/// Inbound request the extractors already disassembled.
struct Inbound {
    method: Method,
    alias: String,
    path_suffix: String,
    query: String,
    headers: HeaderMap,
    body: axum::body::Body,
    security: Option<SecurityContext>,
    /// Address of the peer socket, when the transport records it.
    peer: Option<SocketAddr>,
    /// Upgrade handle of the client socket, present when the caller asked for
    /// a protocol switch.
    upgrade: Option<hyper::upgrade::OnUpgrade>,
}

/// Refunds a granted ADR-0003 `queue` reservation unless the request completes.
///
/// The reservation debits the bucket before the body is read, so every later
/// step — the body cap, the plugin chain, the outbound call — runs on budget
/// that is already spent. Each of those steps returns early through a `?`, and
/// each of them must put the tokens back: a failed request must not consume
/// budget for nothing. Holding the reservation in a guard turns that from ten
/// separate call sites into one: the refund happens where the orchestration
/// unwinds, and only [`ReservationGuard::disarm`] — the success path — keeps it.
struct ReservationGuard {
    registry: Arc<RateLimiterRegistry>,
    reservation: Option<RateLimitReservation>,
}

impl ReservationGuard {
    /// A guard that holds nothing yet.
    fn new(registry: Arc<RateLimiterRegistry>) -> Self {
        Self {
            registry,
            reservation: None,
        }
    }

    /// Takes over the reservation the limiter granted, if it granted one.
    fn arm(&mut self, reservation: Option<RateLimitReservation>) {
        self.reservation = reservation;
    }

    /// The request completed: the debit is its own, nothing is refunded.
    fn disarm(&mut self) {
        self.reservation = None;
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if let Some(reservation) = self.reservation.take() {
            self.registry.release(reservation, Instant::now());
        }
    }
}

/// A failed orchestration, together with the plugin chain to run the error
/// phase over (`None` when the failure happened before the chain was built).
struct Failed {
    error: OagwError,
    plugins: Option<PluginChain>,
    /// Extra response headers of the failure, e.g. the ADR-0003 rate-limit
    /// budget of a rejected request.
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl Failed {
    /// Wraps an error produced before the chain existed.
    fn early(error: OagwError) -> Self {
        Self {
            error,
            plugins: None,
            headers: Vec::new(),
        }
    }

    /// Wraps an error produced after `plugins` was built.
    fn late(error: OagwError, plugins: &PluginChain) -> Self {
        Self {
            error,
            plugins: Some(plugins.clone()),
            headers: Vec::new(),
        }
    }

    /// Attaches response headers to the failure.
    fn with_headers(mut self, headers: Vec<(HeaderName, HeaderValue)>) -> Self {
        self.headers = headers;
        self
    }
}

/// The answer of a successful orchestration, still missing its proxy stamp.
type Answer = (u16, Response);

/// `GET|POST|… /oagw/v1/proxy/{alias}` — proxies a request without a suffix.
///
/// All failures are rendered as `application/problem+json`; the handler never
/// returns an `Err` to the framework, so the ADR-0007 stamp and the proxy log
/// line always land.
pub async fn proxy_alias(
    method: Method,
    Extension(plane): Extension<DataPlane>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    dispatch(
        &plane,
        Inbound {
            method,
            alias,
            path_suffix: String::new(),
            query: parts.uri.query().map(str::to_owned).unwrap_or_default(),
            headers: parts.headers,
            body,
            security: parts.extensions.get::<SecurityContext>().cloned(),
            peer: peer_of(&parts.extensions),
            upgrade: parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned(),
        },
    )
    .await
}

/// `GET|POST|… /oagw/v1/proxy/{alias}/{*path_suffix}` — proxies a request whose
/// path continues past the alias.
///
/// All failures are rendered as `application/problem+json`; see
/// [`proxy_alias`].
pub async fn proxy_alias_path(
    method: Method,
    Extension(plane): Extension<DataPlane>,
    Path((alias, path_suffix)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    dispatch(
        &plane,
        Inbound {
            method,
            alias,
            path_suffix,
            query: parts.uri.query().map(str::to_owned).unwrap_or_default(),
            headers: parts.headers,
            body,
            security: parts.extensions.get::<SecurityContext>().cloned(),
            peer: peer_of(&parts.extensions),
            upgrade: parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned(),
        },
    )
    .await
}

/// Runs the orchestration and renders the answer, whatever the outcome.
///
/// Every exchange — a proxied answer, a gateway error, a rejected request — is
/// reported twice: as the metrics of DESIGN §4.2 and as the structured audit
/// record of DESIGN §4.3 ([`log_proxy`]). Both are labelled with the *matched
/// route pattern*, never with the path the client asked for.
async fn dispatch(plane: &DataPlane, inbound: Inbound) -> Response {
    let started = Instant::now();
    let alias = inbound.alias.clone();
    let verb = inbound.method.as_str().to_owned();
    let security = inbound.security.clone();
    let mut exchange = ProxyExchange::new(&alias, &verb);
    exchange.request_id = header_str(&inbound.headers, "x-request-id");
    if let Some(security) = security.as_ref() {
        exchange.tenant_id = Some(security.subject_tenant_id().to_string());
        exchange.principal_id = Some(security.subject_id().to_string());
    }
    plane.metrics.inc_in_flight(&alias);

    let (status, source, answer) = match orchestrate(plane, inbound, &mut exchange).await {
        Ok((status, answer)) => (status, ERROR_SOURCE_UPSTREAM, answer),
        Err(failed) => {
            let Failed {
                error,
                plugins,
                headers,
            } = failed;
            let status = error.status().as_u16();
            let error = run_error_phase(plugins, error).await;
            let error_type = error.gts_type();
            exchange.error_type = Some(error_type.clone());
            exchange.error_message = Some(error.detail().to_owned());
            exchange.outcome = ProxyOutcome::Gateway;
            plane.metrics.record_error(
                host_label(&exchange.route, &alias),
                &exchange.route,
                &error_type,
            );
            let mut answer = error.into_response_with_source(ERROR_SOURCE_GATEWAY);
            for (name, value) in headers {
                answer.headers_mut().insert(name, value);
            }
            (status, ERROR_SOURCE_GATEWAY, answer)
        }
    };

    let elapsed = started.elapsed();
    exchange.status = status;
    exchange.duration_ms = elapsed.as_millis() as u64;
    // Every dimension the client controls is collapsed onto a fixed literal
    // once nothing resolved: see the `Cardinality` section of the metrics
    // module. `host_label` reads the *final* route, so a request that never
    // matched anything reports `unmatched` on both axes.
    let metric_host = host_label(&exchange.route, &alias);
    plane
        .metrics
        .record_request(metric_host, &verb, &exchange.route, status);
    plane.metrics.record_duration(
        metric_host,
        &exchange.route,
        crate::domain::metrics::PHASE_TOTAL,
        elapsed.as_secs_f64(),
    );
    plane.metrics.dec_in_flight(&alias);
    tracing::info!(
        target: "oagw.proxy",
        alias = %alias,
        route = %exchange.route,
        method = %verb,
        status,
        source,
        elapsed_ms = elapsed.as_millis() as u64,
        "proxy request completed"
    );
    log_proxy(&exchange);
    answer
}

/// Runs the nine steps.
async fn orchestrate(
    plane: &DataPlane,
    inbound: Inbound,
    exchange: &mut ProxyExchange,
) -> Result<Answer, Failed> {
    let Inbound {
        method,
        alias,
        path_suffix,
        query,
        headers,
        body,
        security,
        peer,
        upgrade: downstream_upgrade,
    } = inbound;

    // The client-visible location of the request, for problem documents. The
    // *metrics* are labelled with the matched route pattern instead (see step
    // 3), because the client path would mint one metric series per resource.
    let request_path = format!("/{alias}{path_suffix}");

    let origin = header_str(&headers, "origin");
    let request_method = header_str(&headers, "access-control-request-method");
    let request_headers = header_str(&headers, "access-control-request-headers");
    let preflight = is_preflight(
        method.as_str(),
        origin.as_deref(),
        request_method.as_deref(),
    );

    // -- 0. the CORS preflight (ADR-0004) ---------------------------------
    //
    // A browser preflight carries no credentials, so it arrives without a
    // tenant context and must never be answered with `401` or `404`. It is
    // answered from the registry when an upstream and a route resolve without
    // a `SecurityContext`; every other preflight gets the permissive answer
    // that grants nothing. Origin and method enforcement happens on the actual
    // request, in step 5.
    if preflight {
        let target = resolve_preflight_target(
            plane,
            security.as_ref(),
            &alias,
            &method,
            &path_suffix,
            &request_path,
        )
        .await;
        let configured = match target.filter(|target| target.upstream.enabled) {
            Some(target) => {
                let cors = merged_cors(&target.upstream, target.route.as_deref());
                preflight_answer(
                    cors.as_ref(),
                    origin.as_deref(),
                    request_method.as_deref(),
                    request_headers.as_deref(),
                )
                .ok()
            }
            None => None,
        };
        return match configured {
            Some(answer) => Ok(answer),
            None => fallback_preflight(request_method.as_deref(), request_headers.as_deref()),
        };
    }

    // -- 1. the authenticated caller -------------------------------------
    let Some(security) = security else {
        return Err(Failed::early(
            OagwError::authentication_failed(
                "the proxy surface requires an authenticated security context",
            )
            .with_alias(&alias)
            .with_path(&request_path),
        ));
    };

    // -- 2. the upstream --------------------------------------------------
    let chain = plane.service.tenant_chain(&security).await;
    let target = match lookup_dp_cache(
        plane,
        security.subject_tenant_id(),
        &alias,
        &method,
        &path_suffix,
    ) {
        Some(target) => target,
        None => {
            let target = resolve_target(
                plane,
                &chain,
                &alias,
                &method,
                &path_suffix,
                preflight,
                &request_path,
            )?;
            plane.service.store().store_dp_cache(
                security.subject_tenant_id(),
                &alias,
                method.as_str(),
                &path_suffix,
                Arc::clone(&target),
            );
            target
        }
    };
    if !target.upstream.enabled {
        return Err(Failed::early(
            OagwError::link_unavailable(format!("upstream '{alias}' is disabled"))
                .with_alias(&alias)
                .with_upstream_id(target.upstream.id)
                .with_path(&request_path),
        ));
    }
    let upstream = Arc::clone(&target.upstream);

    // -- 3. the route -----------------------------------------------------
    let Some(route) = target.route.as_deref().map(Arc::new) else {
        return Err(Failed::early(
            OagwError::route_not_found(format!(
                "no route of upstream '{alias}' matches {} {request_path}",
                method.as_str()
            ))
            .with_alias(&alias)
            .with_upstream_id(upstream.id)
            .with_path(&request_path),
        ));
    };
    // The metric label of the matched route: its configured path, plus the
    // method, so two routes that share a path with disjoint method sets stay
    // distinguishable and a path suffix can never mint a series.
    exchange.route = route_label(
        method.as_str(),
        route.r#match.http.as_ref().map(|http| http.path.as_str()),
    );

    // -- 4. the merged configuration --------------------------------------
    let cors = merged_cors(&upstream, Some(&route));
    let rate_limit = effective_rate_limit(&upstream, Some(&route));

    let mut context = RequestContext::builder()
        .method(method.as_str())
        .target_host(
            upstream
                .server
                .endpoints
                .first()
                .map_or_else(String::new, |endpoint: &Endpoint| endpoint.host.clone()),
        )
        .path(
            route
                .r#match
                .http
                .as_ref()
                .map_or_else(String::new, |http| http.path.clone()),
        )
        .path_suffix(path_suffix.clone())
        .query(query.clone())
        .headers(headers.clone())
        .tenant_id(security.subject_tenant_id())
        .subject_id(security.subject_id())
        .security(Arc::new(security.clone()))
        .upstream_id(upstream.id)
        .alias(alias.clone())
        .request_id(header_str(&headers, "x-request-id").unwrap_or_default())
        .peer_ip(client_identity(&headers, peer).unwrap_or_default())
        .route_id(route.id)
        .cors(CorsOutcome::Disabled)
        .build();
    exchange.host = context.target_host.clone();

    // -- 5. CORS ----------------------------------------------------------
    if let Some(cors) = cors.as_ref() {
        if let Err(error) = validate_actual_request(cors, origin.as_deref(), method.as_str()) {
            return Err(Failed::early(error.with_path(&request_path)));
        }
        context.cors = CorsOutcome::Allowed {
            headers: actual_response_headers(cors, origin.as_deref()),
        };
    }

    // -- 6. rate limit ----------------------------------------------------
    //
    // Before the body is read: the decision needs no payload, so an exhausted
    // budget is answered without buffering the request (DESIGN "Body
    // Validation Rules": reject before buffering). The `ip` scope keys on the
    // peer socket address, never on a client-controlled header alone.
    let mut reservation = ReservationGuard::new(Arc::clone(&plane.rate_limiters));
    if let Some(limit) = rate_limit.as_ref() {
        let scope_values = RateLimitScopeValues {
            tenant_id: Some(security.subject_tenant_id().to_string()),
            subject_id: Some(security.subject_id().to_string()),
            peer_ip: client_identity(&headers, peer),
            route_id: Some(route.id.to_string()),
        };
        let decision = plane.rate_limiters.check(
            upstream.id,
            limit,
            &scope_values,
            Instant::now(),
            epoch_now(),
        );
        plane.metrics.set_rate_limit_usage_ratio(
            &alias,
            &exchange.route,
            usage_ratio(limit, decision.remaining),
        );
        if decision.is_limited() {
            plane
                .metrics
                .record_rate_limit_exceeded(&alias, &exchange.route);
            let headers = decision.headers();
            return Err(Failed::early(decision.into_error()).with_headers(headers));
        }
        // ADR-0003 `strategy: queue`: the limiter reserved a token, so the
        // request waits for it instead of being rejected. The wait is bounded
        // twice — by the limiter's own bound and by the proxy budget — and is
        // spent here, before the body is read, so a queued request still does
        // not buffer its payload.
        //
        // The debit stays on the bucket from here to the end of the
        // orchestration: the guard refunds it if any of the steps below fails,
        // and only the success path at the end of step 9 keeps it.
        reservation.arm(plane.rate_limiters.reservation(
            upstream.id,
            limit,
            &scope_values,
            &decision,
        ));
        wait_for_reserved_token(decision.queue_wait, plane.engine.proxy_timeout()).await;
        context.rate_limit = Some(decision);
    }

    // -- 7. the request body ----------------------------------------------
    //
    // A declared size over the cap is rejected before the body is buffered, so
    // a 100 MiB upload never reaches memory; the post-read checks keep guarding
    // the mismatch cases a declared header alone cannot catch.
    validate_declared_body_size(&headers).map_err(Failed::early)?;
    context.body = read_payload(&headers, body).await.map_err(Failed::early)?;
    exchange.request_size = context
        .body
        .buffered_len()
        .map_or(0, |length| u64::try_from(length).unwrap_or(u64::MAX));

    // -- 8. the plugin chain ----------------------------------------------
    let disabled = disabled_plugins(plane, &chain, &upstream, &route);
    let plugins = plane
        .plugins
        .build_chain(&upstream, Some(&route), &disabled)
        .map_err(|error| Failed::early(error.with_alias(&alias).with_path(&request_path)))?;
    if let Err(error) = plugins.authenticate(&mut context).await {
        return Err(Failed::late(error, &plugins));
    }
    if let Err(error) = plugins.guard_request(&context).await {
        return Err(Failed::late(error, &plugins));
    }
    if let Err(error) = plugins.transform_request(&mut context).await {
        return Err(Failed::late(error, &plugins));
    }

    // -- 9. the outbound call ---------------------------------------------
    let pinned = context
        .header(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let selection = plane
        .engine
        .select_endpoint(&upstream, pinned.as_deref())
        .map_err(|error| {
            Failed::late(error.with_alias(&alias).with_path(&request_path), &plugins)
        })?;

    let upgrade = is_websocket_upgrade(&headers)
        && matches!(selection.endpoint.scheme, Scheme::Ws | Scheme::Wss);
    let outbound = outbound_path(&route, &path_suffix);
    exchange.outbound_path = outbound.clone();
    exchange.endpoint = ProxyEngine::render_endpoint(&selection.endpoint);
    let proxy_request = ProxyRequest {
        method: method.clone(),
        path: outbound,
        query: query.clone(),
        headers: context.headers.clone(),
        injected_headers: context.injected_headers.clone(),
        injected_query: context.injected_query.clone(),
        body: request_body(&context.body),
        target_host: Some(selection.endpoint.host.clone()),
        upgrade,
        downstream_upgrade: upgrade.then(|| downstream_upgrade.clone()).flatten(),
    };

    let response = plane
        .engine
        .send(&upstream, Some(&route), &selection.endpoint, proxy_request)
        .await
        .map_err(|error| Failed::late(error, &plugins))?;
    let response = plane
        .engine
        .prepare_response(&upstream, Some(&route), response)
        .map_err(|error| Failed::late(error, &plugins))?;

    let mut response_context = ResponseContext::builder()
        .status(response.status)
        .headers(response.headers.clone())
        .body(BodyPayload::Streaming)
        .request_id(context.request_id.clone().unwrap_or_default())
        .build();
    if let Err(error) = plugins.guard_response(&response_context).await {
        return Err(Failed::late(error, &plugins));
    }
    if let Err(error) = plugins.transform_response(&mut response_context).await {
        return Err(Failed::late(error, &plugins));
    }

    // The upstream answered, whatever status it produced: the exchange is an
    // `upstream` outcome and the client gets the status it gave us.
    exchange.outcome = ProxyOutcome::Upstream;
    exchange.response_size = declared_body_size(&response.headers);
    let rate_limit_headers = context
        .rate_limit
        .as_ref()
        .map(|decision| decision.headers())
        .unwrap_or_default();
    let cors_headers = cors_outcome_headers(&context.cors);
    let status = response.status.as_u16();
    let mut answer = render_upstream(
        response,
        response_context.headers,
        rate_limit_headers.into_iter().chain(cors_headers).collect(),
    );
    // A protocol switch hands the client socket to the caller: hyper finds the
    // upgrade handle in the answer extensions.
    if status == http::StatusCode::SWITCHING_PROTOCOLS.as_u16()
        && let Some(handle) = downstream_upgrade
    {
        answer.extensions_mut().insert(handle);
    }
    // The exchange completed, whatever status the upstream gave: the budget the
    // queue reservation spent is now this request's, and is not refunded.
    reservation.disarm();
    Ok((status, answer))
}

/// The response headers of a CORS-annotated request.
fn cors_outcome_headers(outcome: &CorsOutcome) -> Vec<(HeaderName, HeaderValue)> {
    match outcome {
        CorsOutcome::Allowed { headers } | CorsOutcome::Preflight { headers } => headers.clone(),
        CorsOutcome::Disabled | CorsOutcome::Rejected { .. } => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Reads the DP cache, which only ever holds validated resolutions.
///
/// The key carries the *calling* tenant, not the owner of the resolved
/// upstream: two callers whose tenant chains differ must not share a snapshot,
/// because the chain decides which routes are visible. An upstream mutation
/// therefore cannot be expressed as a key prefix over the callers that resolved
/// it, so [`crate::infra::storage::RegistryStore::flush_upstream_caches`]
/// drops the whole data-plane snapshot cache instead — the same footprint a
/// route mutation already has.
fn lookup_dp_cache(
    plane: &DataPlane,
    tenant_id: Uuid,
    alias: &str,
    method: &Method,
    path_suffix: &str,
) -> Option<Arc<ResolvedProxyTarget>> {
    plane
        .service
        .store()
        .lookup_dp_cache(tenant_id, alias, method.as_str(), path_suffix)
        .filter(|target| target.upstream.enabled)
        .and_then(|target| reconcile_dp_snapshot(plane.service.store(), &target))
}

/// Reconciles a cached resolution against the authoritative registry.
///
/// **The cache is only ever served what the registry still agrees with.** The
/// snapshot cache is dropped wholesale on an upstream or route mutation, but the
/// request that fills it is not atomic with the mutation: a request can resolve
/// from the authoritative maps, be preempted before
/// [`RegistryStore::store_dp_cache`], and see its snapshot flushed by a
/// concurrent `replace_upstream` / `replace_route` / `delete_upstream` — which
/// then repopulates the cache with the pre-mutation state. Re-reading the cached
/// ids closes that window:
///
/// * the upstream is re-read by its id (owner tenant + id, which the snapshot
///   carries); a snapshot the registry no longer holds, or whose upstream is now
///   disabled, is not served;
/// * the route is re-read as well — replacing a route does not touch the
///   upstream `Arc`, so an unchanged upstream proves nothing about the route.
///   A route that has moved to another upstream, or was disabled, makes the
///   snapshot stale;
/// * both `Arc`s identical → the snapshot is the one the registry still holds
///   and is used as is (the fast path of every request not racing a mutation);
/// * an `Arc` differs → the snapshot is rebuilt from the fresh ones, which is
///   also how a route-less snapshot (a preflight resolution) reconciles: it
///   holds nothing but the upstream to re-validate;
/// * an id is gone → `None`, which makes the caller resolve from the
///   authoritative maps.
fn reconcile_dp_snapshot(
    store: &RegistryStore,
    cached: &Arc<ResolvedProxyTarget>,
) -> Option<Arc<ResolvedProxyTarget>> {
    let upstream = store
        .get_upstream(cached.upstream.tenant_id, cached.upstream.id)
        .filter(|upstream| upstream.enabled)?;
    let route = match cached.route.as_ref() {
        Some(cached_route) => {
            let fresh = store.get_route(cached_route.tenant_id, cached_route.id)?;
            if fresh.upstream_id != upstream.id || !fresh.enabled {
                // The cached route no longer belongs to an enabled route of
                // this upstream: the snapshot is stale, not route-less.
                return None;
            }
            Some(fresh)
        }
        // A preflight snapshot binds the alias to an upstream only.
        None => None,
    };
    let unchanged = Arc::ptr_eq(&upstream, &cached.upstream)
        && route.as_ref().map(Arc::as_ptr) == cached.route.as_ref().map(Arc::as_ptr);
    if unchanged {
        return Some(Arc::clone(cached));
    }
    Some(Arc::new(ResolvedProxyTarget { upstream, route }))
}

/// Resolves `(upstream, route)` for a preflight, or `None` when the request
/// cannot be bound to a configured upstream without a security context.
///
/// ADR-0004: a browser preflight carries no credentials, so an unauthenticated
/// caller is *expected* here and must not be answered with `401`; the same
/// fallback covers an unknown alias, an unmatchable route, a disabled upstream
/// and a `grpc` one.
async fn resolve_preflight_target(
    plane: &DataPlane,
    security: Option<&SecurityContext>,
    alias: &str,
    method: &Method,
    path_suffix: &str,
    request_path: &str,
) -> Option<Arc<ResolvedProxyTarget>> {
    let security = security?;
    let chain = plane.service.tenant_chain(security).await;
    lookup_dp_cache(
        plane,
        security.subject_tenant_id(),
        alias,
        method,
        path_suffix,
    )
    .or_else(|| {
        resolve_target(
            plane,
            &chain,
            alias,
            method,
            path_suffix,
            true,
            request_path,
        )
        .ok()
    })
}

/// The disabled plugin resources of the resolved tenant chain, for the chain
/// builder to skip.
///
/// A binding that names a plugin resource with `enabled: false` must degrade to
/// "not applied" instead of answering `503 PluginNotFound`, which is what an
/// enabled-but-unresolvable reference keeps doing. The scan is skipped when the
/// request carries no plugin binding at all, so the hot path of a gateway
/// without custom plugins never touches the plugin registry.
fn disabled_plugins(
    plane: &DataPlane,
    chain: &[Uuid],
    upstream: &Upstream,
    route: &Route,
) -> DisabledPlugins {
    if upstream.plugins.items.is_empty() && route.plugins.items.is_empty() {
        return DisabledPlugins::default();
    }
    DisabledPlugins::of(plane.service.store().list_plugins(chain))
}

/// Resolves `(upstream, route)` for one proxy request.
fn resolve_target(
    plane: &DataPlane,
    chain: &[Uuid],
    alias: &str,
    method: &Method,
    path_suffix: &str,
    preflight: bool,
    request_path: &str,
) -> Result<Arc<ResolvedProxyTarget>, Failed> {
    let store = plane.service.store();
    let Some(upstream) = store.resolve_upstream_alias(chain, alias) else {
        return Err(Failed::early(
            OagwError::route_not_found(format!(
                "no upstream with alias '{alias}' is visible to the calling tenant"
            ))
            .with_alias(alias)
            .with_path(request_path),
        ));
    };
    if upstream.protocol == Protocol::Grpc {
        return Err(Failed::early(not_implemented()));
    }
    let routes = store.list_routes(chain);
    let Some(route) = best_route(&routes, &upstream, method, path_suffix, preflight) else {
        return Err(Failed::early(
            OagwError::route_not_found(format!(
                "no route of upstream '{alias}' matches {} {request_path}",
                method.as_str()
            ))
            .with_alias(alias)
            .with_upstream_id(upstream.id)
            .with_path(request_path),
        ));
    };
    Ok(Arc::new(ResolvedProxyTarget {
        upstream,
        route: Some(route),
    }))
}

/// Picks the best matching route: method allowlist, then longest path prefix,
/// then the highest priority, then the order the store returned them in.
///
/// The tie-break on priority is not cosmetic: two routes may claim the same
/// path prefix as long as their priorities differ, and `max_by_key` alone would
/// hand the request to the *lowest* of them (Rust returns the last maximum of
/// the list, and the store sorts highest-priority-first).
///
/// A CORS preflight is answered by any enabled route that owns the path: the
/// browser never sends the application verb, so holding it to the allowlist
/// would turn every preflight into a `404`.
fn best_route(
    routes: &[Arc<Route>],
    upstream: &Upstream,
    method: &Method,
    path_suffix: &str,
    preflight: bool,
) -> Option<Arc<Route>> {
    let path = format!("/{}", path_suffix.trim_start_matches('/'));
    routes
        .iter()
        .filter(|route| route.upstream_id == upstream.id && route.enabled)
        .filter(|route| preflight || matches_method(route, method))
        .filter(|route| matches_path(route, &path))
        .enumerate()
        .max_by_key(|(declaration, route)| {
            (
                match_length(route, &path).unwrap_or_default(),
                route.priority,
                // Deterministic last resort for two routes that agree on both
                // length and priority: the earlier one in the resolved list.
                std::cmp::Reverse(*declaration),
            )
        })
        .map(|(_, route)| Arc::clone(route))
}

/// `true` when the route's method allowlist admits `method`.
fn matches_method(route: &Route, method: &Method) -> bool {
    let Some(http) = route.r#match.http.as_ref() else {
        return false;
    };
    http_method(method).is_some_and(|converted| http.methods.contains(&converted))
}

/// `true` when the route's path prefix matches `path` at a segment boundary.
fn matches_path(route: &Route, path: &str) -> bool {
    let Some(http) = route.r#match.http.as_ref() else {
        return false;
    };
    let prefix = http.path.as_str();
    if !path.starts_with(prefix) {
        return false;
    }
    let rest = &path[prefix.len()..];
    rest.is_empty() || rest.starts_with('/') || prefix.ends_with('/')
}

/// Length of the matching path prefix, used to pick the longest one.
fn match_length(route: &Route, path: &str) -> Option<usize> {
    let http = route.r#match.http.as_ref()?;
    matches_path(route, path).then_some(http.path.len())
}

/// Maps an HTTP method onto the route-match enum.
fn http_method(method: &Method) -> Option<HttpMethod> {
    match method.as_str() {
        "GET" => Some(HttpMethod::Get),
        "POST" => Some(HttpMethod::Post),
        "PUT" => Some(HttpMethod::Put),
        "DELETE" => Some(HttpMethod::Delete),
        "PATCH" => Some(HttpMethod::Patch),
        _ => None,
    }
}

/// Derives the outbound path from the route pattern and the matched suffix.
///
/// The path suffix of a proxy request is upstream-relative, so the matched
/// route prefix is re-attached and only the remainder past it is appended
/// (`/orders` + `orders/42/items` → `/orders/42/items`, never
/// `/orders/orders/42/items`).
fn outbound_path(route: &Route, suffix: &str) -> String {
    let Some(http) = route.r#match.http.as_ref() else {
        return format!("/{suffix}").trim_start_matches('/').to_owned();
    };
    if http.path_suffix_mode == PathSuffixMode::Disabled {
        return normalize_prefix(&http.path);
    }
    let prefix = normalize_prefix(&http.path);
    let requested = format!("/{suffix}").trim_start_matches('/').to_owned();
    let requested = format!("/{requested}");
    let remainder = requested
        .strip_prefix(&prefix)
        .unwrap_or(&requested)
        .to_owned();
    format!("{prefix}{remainder}")
}

/// Normalises a route path prefix to a leading slash and no trailing one.
fn normalize_prefix(path: &str) -> String {
    let trimmed = format!("/{path}").trim_start_matches('/').to_owned();
    format!("/{}", trimmed.trim_end_matches('/'))
}

/// Builds the `501` problem document for a `grpc` upstream.
fn not_implemented() -> OagwError {
    OagwError::Forbidden(Box::new(ProblemBody::bare(
        NOT_IMPLEMENTED_TYPE.to_owned(),
        "Not Implemented".to_owned(),
        501,
        "grpc upstreams are not proxied by this gateway yet".to_owned(),
    )))
}

/// Runs the error phase of the plugin chain over a failure.
///
/// A failing transform is swallowed: the original taxonomy error renders, which
/// is more useful to the client than an opaque `502`.
async fn run_error_phase(plugins: Option<PluginChain>, error: OagwError) -> OagwError {
    let Some(plugins) = plugins else {
        return error;
    };
    let mut context = ErrorContext::from_error(error);
    if plugins.transform_error(&mut context).await.is_err() {
        return context.error;
    }
    context.error
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

/// Reads and validates the request body.
///
/// Called after [`validate_declared_body_size`], so the declared size of an
/// oversized body has already been rejected and this buffers at most the cap.
///
/// # Errors
///
/// [`OagwError::PayloadTooLarge`] above the 100 MiB cap and
/// [`OagwError::Validation`] when `Content-Length` or `Transfer-Encoding`
/// disagree with the actual bytes.
async fn read_payload(
    headers: &HeaderMap,
    body: axum::body::Body,
) -> Result<BodyPayload, OagwError> {
    let bytes = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .map_err(body_read_error)?;
    validate_body(headers, &bytes)?;
    Ok(if bytes.is_empty() {
        BodyPayload::Empty
    } else {
        BodyPayload::Buffered(bytes)
    })
}

/// Maps a body-read failure onto the taxonomy.
///
/// An undeclared body (chunked, or without a `Content-Length`) that runs past
/// the 100 MiB cap fails inside `axum::body::to_bytes` with
/// `http_body_util::LengthLimitError`, which DESIGN's body validation rules
/// answer with `413` like any other body over the cap. The gear does not depend
/// on `http-body-util` outside its tests, so the type cannot be downcast here;
/// the limit case is instead recognised by the message that type renders
/// (`"length limit exceeded"`, stable across `http-body-util` 0.1, and the only
/// error `to_bytes` produces for a body that exhausts its limit). Every other
/// failure stays a `400` validation error.
fn body_read_error(error: axum::Error) -> OagwError {
    if error.to_string() == LENGTH_LIMIT_ERROR_MESSAGE {
        OagwError::payload_too_large(format!(
            "request body exceeds the {MAX_REQUEST_BODY_BYTES} byte cap"
        ))
    } else {
        OagwError::validation(format!("request body could not be read: {error}"))
    }
}

/// Validates the declared and actual body size, delegating to the engine so
/// the gateway and the transport agree on the same rules.
fn validate_body(headers: &HeaderMap, bytes: &Bytes) -> Result<(), OagwError> {
    crate::infra::proxy::validate_body(headers, Some(bytes.as_ref())).map(|_| ())
}

/// The transport body the engine forwards.
///
/// The handler always buffers the inbound body ([`read_payload`]), so the
/// streaming arm is only reachable from a plugin that replaced the payload.
fn request_body(payload: &BodyPayload) -> ProxyBody {
    match payload {
        BodyPayload::Empty | BodyPayload::Streaming => ProxyBody::Empty,
        BodyPayload::Buffered(bytes) => ProxyBody::Buffered(bytes.clone()),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Renders an upstream answer, with the ADR-0007 `upstream` stamp.
fn render_upstream(
    response: ProxyResponse,
    headers: HeaderMap,
    rate_limit_headers: Vec<(HeaderName, HeaderValue)>,
) -> Response {
    let mut builder = Response::builder().status(response.status);
    for (name, value) in headers {
        // Iterating a `HeaderMap` yields `Option<HeaderName>`; `None` marks a
        // continuation of the previous multi-valued header, already emitted.
        let Some(name) = name else {
            continue;
        };
        builder = builder.header(name, value);
    }
    for (name, value) in rate_limit_headers {
        builder = builder.header(name, value);
    }
    builder
        .header(ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM)
        .body(response.body)
        .unwrap_or_else(|error| {
            OagwError::protocol_error(format!("invalid upstream response: {error}"))
                .into_response_with_source(ERROR_SOURCE_GATEWAY)
        })
}

/// Renders a CORS preflight answer.
///
/// The browser never sends the application verb, so the preflight is answered
/// from the merged CORS configuration alone: a disabled or absent
/// configuration yields a bare `204`, an allowed origin an annotated one.
fn preflight_answer(
    cors: Option<&EffectiveCorsConfig>,
    origin: Option<&str>,
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Result<Answer, OagwError> {
    render_preflight(cors.map_or_else(
        || PreflightResponse {
            status: 204,
            headers: Vec::new(),
            allowed: false,
        },
        |cors| evaluate_preflight(cors, origin, request_method, request_headers),
    ))
}

/// Renders the permissive answer of a preflight no configured upstream owns.
///
/// ADR-0004 "Preflight Request Handling": the preflight is answered `204`
/// without a tenant context, and enforcement is deferred to the actual request.
/// No CORS configuration is available here, so nothing is granted — the answer
/// names no `Access-Control-Allow-Origin`, which is what keeps the browser from
/// reading a cross-origin response the gateway could not validate.
///
/// # Errors
///
/// [`OagwError::ProtocolError`] when the answer cannot be rendered, which the
/// caller reports like any other gateway failure.
fn fallback_preflight(
    request_method: Option<&str>,
    request_headers: Option<&str>,
) -> Result<Answer, Failed> {
    render_preflight(unresolved_preflight(request_method, request_headers))
        .map_err(|error| Failed::early(OagwError::protocol_error(error.to_string())))
}

/// Turns a preflight decision into the response parts.
fn render_preflight(outcome: PreflightResponse) -> Result<Answer, OagwError> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::NO_CONTENT));
    for (name, value) in outcome.headers {
        builder = builder.header(name, value);
    }
    let answer = builder
        .header(ERROR_SOURCE_HEADER, ERROR_SOURCE_GATEWAY)
        .body(axum::body::Body::empty())
        .map_err(|error| {
            OagwError::protocol_error(format!("invalid preflight response: {error}"))
        })?;
    Ok((outcome.status, answer))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `GET /oagw/v1/metrics` — renders the in-process metrics registry in the
/// Prometheus text exposition format.
///
/// Admin-only: the route is registered with `.authenticated()`, and the
/// licence gate is deliberately left open so a scrape can run without a
/// licence.
pub async fn get_metrics(Extension(plane): Extension<DataPlane>) -> Response {
    let rendered = plane.metrics.render();
    match Response::builder()
        .status(StatusCode::OK)
        .header(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )
        .header(ERROR_SOURCE_HEADER, ERROR_SOURCE_GATEWAY)
        .body(axum::body::Body::from(rendered))
    {
        Ok(response) => response,
        Err(error) => OagwError::protocol_error(format!("metrics rendering failed: {error}"))
            .into_response_with_source(ERROR_SOURCE_GATEWAY),
    }
}

/// Merged CORS configuration over the upstream → route chain.
fn merged_cors(upstream: &Upstream, route: Option<&Route>) -> Option<EffectiveCorsConfig> {
    let mut chain: Vec<&CorsConfig> = Vec::new();
    if let Some(cors) = upstream.cors.as_ref() {
        chain.push(cors);
    }
    if let Some(cors) = route.and_then(|route| route.cors.as_ref()) {
        chain.push(cors);
    }
    merge_cors(&chain)
}

/// Merged rate-limit configuration over the upstream → route chain.
fn effective_rate_limit(upstream: &Upstream, route: Option<&Route>) -> Option<EffectiveRateLimit> {
    let mut chain: Vec<&RateLimitConfig> = Vec::new();
    if let Some(limit) = upstream.rate_limit.as_ref() {
        chain.push(limit);
    }
    if let Some(limit) = route.and_then(|route| route.rate_limit.as_ref()) {
        chain.push(limit);
    }
    resolve_effective_rate_limit(&chain)
}

/// First value of a header as UTF-8.
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The peer socket address of the request, when the transport records one.
///
/// `axum::serve` runs this gear's router through
/// `into_make_service_with_connect_info::<SocketAddr>`, so a request that
/// arrived over the listener always carries it; the `None` case is a request
/// that reached the handler from a transport that does not (the `oneshot` test
/// harness, an in-process caller).
fn peer_of(extensions: &axum::http::Extensions) -> Option<SocketAddr> {
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| *address)
}

/// Client identity for the `ip` rate-limit scope (ADR-0003).
///
/// **The peer socket address wins, and `X-Forwarded-For` is only ever a
/// fallback.** The header is client-controlled: a limit keyed on it hands every
/// caller a fresh budget simply by rotating the header (the limit is bypassed
/// for free), and lets a caller spend a *specific* other caller's budget by
/// naming them. The socket address cannot be forged, so the budget of one
/// connection is exactly that connection's.
///
/// When no socket address is available — a request that did not come through
/// the axum listener — the *last* `X-Forwarded-For` entry is used rather than
/// the first. A proxy chain appends the address it saw, so the last entry is
/// the one the nearest trusted proxy observed, while the first is the one the
/// client chose to claim.
///
/// The trade-off is documented, not accidental: behind a single front proxy
/// that does not append per-client entries, the `ip` scope collapses every
/// caller into one budget. That is the conservative failure mode — callers
/// share a limit rather than escaping one.
fn client_identity(headers: &HeaderMap, peer: Option<SocketAddr>) -> Option<String> {
    if let Some(peer) = peer {
        return Some(peer.ip().to_string());
    }
    header_str(headers, "x-forwarded-for").and_then(|forwarded| {
        forwarded
            .rsplit(',')
            .next()
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_owned)
    })
}

/// Waits out a `queue` strategy reservation (ADR-0003 `strategy: queue`).
///
/// The limiter only ever grants a wait up to [`QUEUE_MAX_WAIT`]; the wait is
/// additionally capped by the proxy budget (`proxy_timeout_secs`), so a queued
/// request never spends longer waiting than the upstream call it is queued for
/// would have been allowed to take. A `None` wait (every other strategy) is a
/// no-op.
async fn wait_for_reserved_token(wait: Option<Duration>, budget: Duration) {
    let Some(wait) = wait else {
        return;
    };
    let granted = wait.min(QUEUE_MAX_WAIT).min(budget);
    if granted.is_zero() {
        return;
    }
    tokio::time::sleep(granted).await;
}

/// Body size the upstream declared for its answer, `0` when it declared none.
///
/// The response body streams, so this is the declared size and not the bytes
/// that actually crossed the socket; the audit record records it as such.
fn declared_body_size(headers: &HeaderMap) -> u64 {
    headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|declared| declared.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// Consumed fraction of the rate-limit budget, clamped to `0.0..=1.0`.
fn usage_ratio(limit: &EffectiveRateLimit, remaining: u64) -> f64 {
    if limit.capacity == 0 {
        return 0.0;
    }
    let consumed = limit.capacity.saturating_sub(remaining);
    consumed as f64 / limit.capacity as f64
}

/// Unix seconds, used by the sliding-window rate limiter.
fn epoch_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
