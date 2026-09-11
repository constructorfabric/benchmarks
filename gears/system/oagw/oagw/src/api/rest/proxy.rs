//! Proxy transport of the OAGW gear (entry 2.4).
//!
//! The module is the thin transport the data plane serves through: it detects
//! the CORS preflight shape *before* authentication and tenant resolution, it
//! authenticates the actual proxy request against
//! `gts.cf.core.oagw.proxy.v1~:invoke`, it runs the declared half of the body
//! checks before it buffers the body once, and it hands the request to
//! [`ProxyEngine::serve`]. Every outcome is mapped through the entry-2.1 layer,
//! so the caller sees one response shape:
//!
//! * a passthrough response carries the upstream status, headers and body with
//!   `X-OAGW-Error-Source: upstream`;
//! * a gateway failure is a canonical `application/problem+json` document with
//!   `X-OAGW-Error-Source: gateway`.
//!
//! The transport owns no pipeline stage: resolution, matching, merging,
//! selection, validation, transformation, the breaker and the call all live in
//! the [`crate::infra::proxy`] engine, and the plugin hook points in it belong
//! to entry 2.5.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Body;
use axum::extract::{OriginalUri, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use toolkit_security::SecurityContext;

use crate::api::rest::dto::ErrorContext;
use crate::api::rest::error::OagwProblem;
use crate::api::rest::routes::{MOUNT_ROOT, classify_gateway, classify_upstream};
use crate::config::OagwConfig;
use crate::domain::error::DomainError;
use crate::domain::sharing::{PERM_PROXY_INVOKE, TenantHierarchy};
use crate::infra::proxy::context::RequestContext;
use crate::infra::proxy::engine::{
    OutcomeKind, ProxyEngine, ProxyFailure, ProxyOutcome, ProxyRequest,
};
use crate::infra::proxy::endpoint::TARGET_HOST_HEADER;
use crate::infra::obs::{BreakerSource, Observability, RequestObservation};
use crate::infra::proxy::UpstreamCaller;
use crate::infra::proxy::stream::StreamReply;
use crate::infra::proxy::validate::{BODY_HARD_LIMIT, check_declared, framing};
use crate::infra::storage::OagwStore;

/// The `Access-Control-Max-Age` the preflight answer carries, in seconds.
pub const PREFLIGHT_MAX_AGE: &str = "86400";

/// The `Vary` header a preflight answer carries.
pub const PREFLIGHT_VARY: &str =
    "Origin, Access-Control-Request-Method, Access-Control-Request-Headers";

/// The header the W3C trace-context correlation identifier arrives in.
const TRACEPARENT: &str = "traceparent";

/// The header a caller may name its own correlation identifier in.
const REQUEST_ID: &str = "x-request-id";

/// The mount-relative path prefix of the proxy endpoints.
const PROXY_PREFIX: &str = "/proxy";

/// State of the proxy endpoints: the store the snapshots are read from and the
/// pipeline that serves a request through them.
#[derive(Clone)]
pub struct ProxyState {
    store: Arc<OagwStore>,
    engine: Arc<ProxyEngine>,
    /// The observability layer the closed request outcome is emitted through
    /// (entry 2.7).
    obs: Arc<Observability>,
}

impl ProxyState {
    /// Build the state over `store`, `hierarchy` and the loaded configuration.
    ///
    /// `chains` is the plugin chain the pipeline executes at its hook points;
    /// `None` mounts the hook points that continue the pipeline without running
    /// a plugin.
    ///
    /// # Errors
    ///
    /// Returns the client-construction failure of the toolkit, which the mount
    /// step reports as a failed host startup.
    pub fn new(
        store: Arc<OagwStore>,
        hierarchy: Arc<dyn TenantHierarchy>,
        config: &OagwConfig,
        chains: Option<Arc<dyn crate::infra::proxy::hooks::PluginChains>>,
    ) -> Result<Self, toolkit_http::HttpError> {
        let obs = Observability::shared();
        let client = crate::infra::proxy::call::build_client(config)?;
        let engine = Arc::new(
            ProxyEngine::new(
                hierarchy,
                crate::infra::proxy::call::UpstreamCaller::new(Arc::new(client), config),
                chains.unwrap_or_else(|| Arc::new(crate::infra::proxy::hooks::NoPlugins)),
            )
            .with_obs(Arc::clone(&obs)),
        );
        // The gauges of the runtime state read the pipeline's own breaker, so
        // the registry holds no copy of it.
        obs.attach_state_source(Arc::new(BreakerSource::new(&engine)));
        Ok(Self {
            store,
            engine,
            obs,
        })
    }

    /// The pipeline, for the observability layer.
    #[must_use]
    pub const fn engine(&self) -> &Arc<ProxyEngine> {
        &self.engine
    }

    /// The observability layer, for the emission hooks of entry 2.7.
    #[must_use]
    pub const fn observability(&self) -> &Arc<Observability> {
        &self.obs
    }
}

// @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-09
/// The `204` a detected preflight is answered with, locally.
///
/// The answer echoes the request's `Origin`, `Access-Control-Request-Method`
/// and `Access-Control-Request-Headers` into the matching
/// `Access-Control-Allow-*` response headers, sets the `Access-Control-Max-Age`
/// and the `Vary` header, and resolves no upstream, reads no tenant context,
/// runs no plugin hook and requires no Bearer token.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-06
    let mut response = StatusCode::NO_CONTENT.into_response();
    let echoed = [
        (
            "access-control-allow-origin",
            http::header::ORIGIN.as_str(),
        ),
        (
            "access-control-allow-methods",
            "access-control-request-method",
        ),
        (
            "access-control-allow-headers",
            "access-control-request-headers",
        ),
    ];
    for (name, source) in echoed {
        if let Some(value) = headers
            .get(source)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| http::HeaderValue::from_str(value).ok())
        {
            response.headers_mut().insert(name, value);
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-06

    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-07
    response.headers_mut().insert(
        "access-control-max-age",
        http::HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    response.headers_mut().insert(
        http::header::VARY,
        http::HeaderValue::from_static(PREFLIGHT_VARY),
    );
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-07

    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-08
    // Nothing is resolved for a preflight: no upstream, no plugin hook, no
    // tenant context and no Bearer token, so it is answered even when the
    // upstream it names is unreachable.
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-08

    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-10
    // The response is generated by gear code, so the entry-2.1 layer stamps the
    // gateway classification on it on its way out.
    classify_gateway(response)
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-10
}
// @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-09

/// Whether the request is a CORS preflight, decided on the request alone.
///
/// The shape is a `method: OPTIONS` request carrying both `Origin` and
/// `Access-Control-Request-Method`. Anything else is an actual proxy request
/// and continues in the proxy-request flow from authentication.
#[must_use]
pub fn is_preflight(method: &str, headers: &HeaderMap) -> bool {
    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-02
    method.eq_ignore_ascii_case("OPTIONS")
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-02
}

/// Serve one proxy request.
///
/// The single handler of both proxy paths and every forwarded method: the
/// preflight detection runs first, so a preflight never reaches the security
/// context, and an actual request authenticates before the engine resolves
/// anything.
///
/// # Errors
///
/// Never returns an error: every failure is mapped onto the canonical problem
/// document and returned as a response.
pub async fn serve(
    State(state): State<ProxyState>,
    uri: OriginalUri,
    request: Request,
) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01
    // The request reached the proxy endpoint the mount registered for every
    // forwarded method: `/oagw/v1/proxy/{alias}` and
    // `/oagw/v1/proxy/{alias}/{path_suffix}`.
    let path = uri.0.path().to_owned();
    let method = request.method().as_str().to_owned();
    let headers = request.headers().clone();
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01
    let origin = headers
        .get(http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    // The peer address of the inbound connection, when the host serves the
    // router with connect information: the `ip` scope of the rate limit keys on
    // it, and a host that does not provide it leaves the scope to fall back to
    // the tenant key (`inst-arl-06`).
    let peer_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string());

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01b
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01c
    // The preflight request reached the same handler the forwarded methods use:
    // the flow's actor step is this entry, carrying the method, the headers and
    // the `Origin` the preflight answer echoes.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-01
    // The preflight shape is decided on the request alone, before the security
    // context is read, before any tenant is resolved and before the store
    // snapshot is read: a preflight is answered locally and never reaches the
    // alias walk.
    if is_preflight(&method, &headers) {
        // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-05
        return preflight_response(&headers);
        // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-05
    }
    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-03
    // @cpt-begin:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-04
    // The shape did not match, so the request is not a preflight: it continues
    // in the proxy-request flow from authentication below and reaches the alias
    // walk only after the token check passed.
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-04
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-03
    // @cpt-end:cpt-cf-oagw-flow-proxy-preflight:p1:inst-pe-pre-01
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01c
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-01b

    // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-01
    // The exchange is opened on the arrival of the request: its own
    // start-to-close interval is measured from here, which is the interval the
    // audit line's `duration_ms` reports, and the emission happens at the close
    // of the exchange, on every exit of the handler below — the exits the
    // transport takes before the pipeline is reached included, and the close a
    // relayed exchange takes after this handler has returned included — and no
    // failure of it reaches the response.
    let exchange = Exchange::new(&state.obs);
    // The mounted path is split before the gate so the refusals below record
    // the same suffix path a served request records, and the context they close
    // on carries the correlation identifier the request arrived with and
    // nothing else: the pipeline never opened one of its own for them.
    let match_path = request.uri().path().to_owned();
    let (_, suffix) = split_path(&match_path);
    let transport = RequestContext::new(correlation_id(&headers), suffix.clone(), method.clone());
    // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-01

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-02
    // The Bearer token the host resolved: a request without one, with an
    // anonymous context or with a context whose token does not carry the proxy
    // permission is refused before the alias walk runs. Each refusal closes the
    // exchange it opened, so a request the gateway refused is still one
    // recorded proxied request.
    let Some(context) = request.extensions().get::<SecurityContext>().cloned() else {
        let error = DomainError::AuthenticationFailed {
            detail: "the proxy request carries no authenticated caller".to_owned(),
        };
        let response = unauthenticated(&path);
        exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
        return response;
    };
    if context.subject_tenant_id().is_nil() {
        let error = DomainError::AuthenticationFailed {
            detail: "the proxy request carries no authenticated caller".to_owned(),
        };
        let response = unauthenticated(&path);
        exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
        return response;
    }
    if !is_granted(context.token_scopes(), PERM_PROXY_INVOKE) {
        let error = DomainError::AuthenticationFailed {
            detail: format!("the token does not carry `{PERM_PROXY_INVOKE}`"),
        };
        let response = OagwProblem::new(&error, &transport_context(&path)).into_response();
        exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
        return response;
    }
    // The caller is known from here on, so the identifiers the line records are
    // the ones the transport authenticated the request with.
    exchange.identify(&context);
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-02

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-04
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-05
    // A request that was dispatched to the preflight flow above already left
    // this handler with that flow's `204`, so no step below runs for it: a
    // preflight reaches neither the alias walk, nor a plugin hook point, nor
    // the upstream call.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-05
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-04

    // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-03
    // The declared half of the body checks is a pure function of the request
    // head, so a `413` never depends on a body the gateway already read.
    let declared = match framing(&headers) {
        Ok(declared) => declared,
        Err(error) => {
            let response = problem(error.clone(), transport_context(&path));
            exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
            return response;
        }
    };
    if let Err(error) = check_declared(&declared) {
        let response = problem(error.clone(), transport_context(&path));
        exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
        return response;
    }
    // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-03

    // The mounted router strips the `/oagw/v1` prefix, so the matching path the
    // handler sees is `/proxy/{alias}` or `/proxy/{alias}/{suffix}`; the
    // problem document reports the full request path.
    let target_host = headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let (parts, body) = request.into_parts();
    // Buffered once, capped at the hard limit plus the sentinel byte the
    // framing comparison needs: an oversized body is refused by the engine's
    // observed-size check.
    let buffered = match axum::body::to_bytes(body, buffer_limit()).await {
        Ok(bytes) => bytes,
        Err(_) => {
            let error = DomainError::PayloadTooLarge {
                detail: format!("the body exceeds the {BODY_HARD_LIMIT} byte limit"),
            };
            let response = problem(error.clone(), transport_context(&path));
            exchange.close(&transport, response.status().as_u16(), None, false, Some(&error));
            return response;
        }
    };
    let (alias, _) = split_path(&match_path);
    // The upgrade handle the request extensions carried: the client side of the
    // pair an upgrade relay relays. A request that carries none is not an
    // upgrade, so the relay never sees a handle that cannot be awaited.
    let client_upgrade = parts
        .extensions
        .get::<hyper::upgrade::OnUpgrade>()
        .cloned();
    let inbound = headers.clone();
    let proxy_request = ProxyRequest {
        snapshot: state.store.snapshot(),
        security: context.clone(),
        tenant_id: context.subject_tenant_id(),
        trace_id: correlation_id(&headers),
        alias,
        method,
        match_path: suffix,
        query: parts.uri.query().map(str::to_owned),
        headers,
        body: buffered,
        origin,
        peer_ip,
    };

    // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-01
    // The request ends either in a passthrough response or in a failure of a
    // pipeline stage; both are classified by the paths below.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-02
    match state.engine.serve(proxy_request).await {
        Ok(outcome) => {
            passthrough(
                outcome,
                &path,
                &inbound,
                client_upgrade,
                state.engine.caller(),
                &exchange,
            )
            .await
        }
        Err(failure) => {
            // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-04
            // A gateway failure closes the request context the pipeline opened,
            // so the outcome is recorded from that context, with the error the
            // problem document is mapped from. The response the client receives
            // is unchanged by the emission.
            let upstream = crate::infra::proxy::breaker::counts_as_failure(&failure.error);
            let error = failure.error.clone();
            let context = failure.context.clone();
            let response = failed(failure, &path, target_host);
            exchange.close(
                &context,
                response.status().as_u16(),
                None,
                upstream,
                Some(&error),
            );
            response
            // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-04
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-02
    // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-01
}

/// The byte limit the request body is buffered under.
///
/// The hard limit plus the sentinel byte the framing comparison needs: an
/// oversized body is then refused by the engine's observed-size check, so the
/// gateway never reads a body it would have to reject as a whole.
const fn buffer_limit() -> usize {
    // The hard limit is 100 MiB, far below `usize::MAX` on every supported
    // target, so the conversion is lossless.
    (BODY_HARD_LIMIT + 1) as usize
}

// @cpt-begin:cpt-cf-oagw-dod-error-mapping:p1:inst-full
/// The caller the transport authenticated.
#[derive(Default)]
struct Caller {
    tenant_id: Option<String>,
    principal_id: Option<String>,
}

/// The emission one proxied exchange closes with (entry 2.7).
///
/// The emission is held by a count of its own, because a relayed exchange
/// outlives the handler that handed it off: the relay that ends it closes the
/// exchange it holds, so the handler's own copy is gone by then. Exactly one
/// line and one metric update set are produced per exchange, on the exit that
/// committed the response, and no failure of the emission is raised to the
/// caller.
struct Exchange {
    inner: Arc<ExchangeInner>,
}

impl Exchange {
    /// An exchange measured from the arrival of the request, before the
    /// transport knows who the caller is: a request the gateway refuses before
    /// authentication is still one exchange, recorded with the identifiers it
    /// never learned as JSON `null`.
    fn new(obs: &Arc<Observability>) -> Self {
        Self {
            inner: Arc::new(ExchangeInner {
                started: std::time::Instant::now(),
                obs: Arc::clone(obs),
                caller: Mutex::new(Caller::default()),
                emitted: AtomicBool::new(false),
            }),
        }
    }

    /// Record the caller the transport authenticated, once it is known.
    fn identify(&self, security: &SecurityContext) {
        let tenant = security.subject_tenant_id();
        let subject = security.subject_id();
        let mut caller = self.inner.caller.lock();
        caller.tenant_id = (!tenant.is_nil()).then(|| tenant.to_string());
        caller.principal_id = (!subject.is_nil()).then(|| subject.to_string());
    }

    /// Record the closed request outcome
    /// (`cpt-cf-oagw-flow-request-audit`).
    fn close(
        &self,
        context: &RequestContext,
        status: u16,
        response_bytes: Option<u64>,
        upstream_called: bool,
        error: Option<&DomainError>,
    ) {
        self.inner.close(context, status, response_bytes, upstream_called, error);
    }

    /// The close a handed-off exchange still owes, for the relay that ends it.
    ///
    /// A relay whose head was committed runs after this handler has returned, so
    /// the close the exchange owes moves onto it and runs at the terminal state
    /// the relay records — the clean end, an abort, the idle window or the client
    /// that went away while the body was streaming. The relay owns it from here
    /// and the exchange keeps its own gate, so the two of them cannot emit twice.
    // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-03
    // A streamed exchange is recorded once, at the close the relay reports: the
    // emission the exchange holds is handed to the relay, which carries the
    // request context and the in-flight accounting the exchange opened.
    fn handed_off_close(&self, status: u16) -> crate::infra::proxy::stream::ExchangeClose {
        let inner = Arc::clone(&self.inner);
        crate::infra::proxy::stream::ExchangeClose::new(Box::new(move |context| {
            inner.close(context, status, None, true, None);
        }))
    }
    // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-03
}

/// The facts one exchange's emission reads, held by the count the relayed
/// exchange closes through.
struct ExchangeInner {
    /// The instant the request arrived, so the interval the line reports covers
    /// the whole of a relayed exchange and not the handoff alone.
    started: std::time::Instant,
    obs: Arc<Observability>,
    /// The identifiers the transport authenticated, read once they are known.
    caller: Mutex<Caller>,
    /// Whether the exchange's line was already emitted.
    emitted: AtomicBool,
}

impl ExchangeInner {
    /// Record the closed request outcome exactly once
    /// (`cpt-cf-oagw-flow-request-audit`).
    // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-02
    // The pipeline closed the request context it opened at `inst-pe-req-03`,
    // and this is the call entry 2.7 receives at that close: the completed
    // context is its only input.
    fn close(
        &self,
        context: &RequestContext,
        status: u16,
        response_bytes: Option<u64>,
        upstream_called: bool,
        error: Option<&DomainError>,
    ) {
        // One exchange, one line: a close the relay already ran on the terminal
        // state it recorded leaves nothing for a second one to emit, whichever
        // of the two reaches this point first.
        if self.emitted.swap(true, Ordering::AcqRel) {
            return;
        }
        let caller = self.caller.lock();
        self.obs.record_request(RequestObservation {
            context,
            status,
            upstream_called,
            duration: self.started.elapsed(),
            response_bytes,
            tenant_id: caller.tenant_id.as_deref(),
            principal_id: caller.principal_id.as_deref(),
            error,
        });
    }
    // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-02
}

/// The response of a pipeline outcome.
///
/// A complete response is passed through here; a streamed or an upgraded
/// exchange is handed to the entry-2.6 relay, which returns either a response
/// the entry-2.1 header layer classifies or a gateway failure this layer maps
/// onto the canonical problem document.
async fn passthrough(
    outcome: ProxyOutcome,
    path: &str,
    inbound: &HeaderMap,
    client_upgrade: Option<hyper::upgrade::OnUpgrade>,
    caller: &UpstreamCaller,
    exchange: &Exchange,
) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-01
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-02
    // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-06
    // An upstream response is available, whatever status it carries: the
    // status, the headers and the body are passed through, no gateway-generated
    // body is added and the error source is `upstream`.
    match outcome.kind {
        OutcomeKind::Passthrough => {
            let mut builder = Response::builder().status(outcome.status);
            for (name, value) in outcome.headers.iter() {
                builder = builder.header(name, value);
            }
            for (name, value) in crate::infra::proxy::validate::cors_response_headers(
                &outcome.cors,
                outcome.context.cross_origin,
            ) {
                builder = builder.header(name, value);
            }
            let body = outcome
                .body
                .map_or_else(Body::empty, Body::new);
            // The byte count the relayed head declared: the body is passed
            // through unread, so the count the client receives is the one the
            // response head carries, and an exchange that declared none is
            // recorded as an unknown size.
            let response_bytes = outcome
                .headers
                .get(http::header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a
            // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07
            // The response is committed here, so the request context the
            // pipeline carried to it closes with the status the client
            // receives, the byte count of the body the gateway relayed and no
            // error. One line, one metric update set.
            exchange.close(
                &outcome.context,
                outcome.status.as_u16(),
                response_bytes,
                true,
                None,
            );
            // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07
            // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-07a
            // The emission returned control: whatever the writer did with the
            // line, the response the client receives is the one the pipeline
            // built, and a successful passthrough carries no gateway-generated
            // identifier beyond the one the bound transform set.
            // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-11
            // @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-07
            match builder.body(body) {
            // @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-07
            // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-11
                // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-07
                // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-08
                // The entry-2.1 header layer applies the classification without
                // overwriting a value the producing path already set.
                // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-09
                Ok(response) => classify_upstream(response),
                // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-09
                // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-08
                // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-07
                Err(_) => problem(
                    DomainError::ProtocolError {
                        detail: "the upstream response could not be relayed".to_owned(),
                    },
                    failure_context(&outcome.context, path, None),
                ),
            }
        }
        // @cpt-begin:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-03
        // @cpt-begin:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-15
        // The open exchange the pipeline classified as streamed is handed to the
        // relay at the handoff point, with the header set the request arrived
        // with: the relay reads the streamed facts from it.
        OutcomeKind::Streamed => {
            // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-03
            // A streamed exchange is recorded once, at the close the relay
            // reports, and never per chunk: the close the exchange owes, with
            // the in-flight accounting it opened, is handed to the relay, which
            // runs it when the record it advances reaches its terminal state —
            // at the handoff itself for an upstream that ended before a byte
            // crossed, after it for a relay whose head was committed.
            let close = exchange.handed_off_close(outcome.status.as_u16());
            let reply =
                crate::infra::proxy::stream::streamed(outcome, inbound, caller.timeout(), close)
                    .await;
            handed_off(reply, path, exchange)
            // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-03
        }
        // @cpt-end:cpt-cf-oagw-algo-sse-forward:p1:inst-ss-fwd-15
        // @cpt-end:cpt-cf-oagw-flow-sse-proxy:p1:inst-ss-sse-03
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b-1
        // The request-side handoff carries the endpoint, the transformed header
        // set and the dial target; the dialing itself belongs to entry 2.6.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b-1
        // @cpt-begin:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-02
        // @cpt-begin:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-03
        // The upgrade request the pipeline handed over before its own upstream
        // call is dialed and relayed by the entry-2.6 upgrade path, which
        // validates and re-injects the upgrade headers.
        OutcomeKind::Upgrade => {
            // Same accounting as the streamed handoff: the close the exchange
            // owes, with the in-flight accounting it opened, is handed to the
            // relay session, which runs it when the session ends.
            let close = exchange.handed_off_close(outcome.status.as_u16());
            let reply = crate::infra::proxy::stream::upgraded(
                outcome,
                inbound,
                caller,
                client_upgrade,
                caller.timeout(),
                close,
            )
            .await;
            handed_off(reply, path, exchange)
        }
        // @cpt-end:cpt-cf-oagw-algo-ws-upgrade:p1:inst-ss-upg-03
        // @cpt-end:cpt-cf-oagw-flow-ws-proxy:p1:inst-ss-wsx-02
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-06
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-02
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-01
}

/// The reply of a handed-off exchange, as a response.
///
/// A response the relay built is classified with the source the handoff already
/// recorded, so the entry-2.1 header layer never rewrites a head the upstream
/// owns; a gateway failure is mapped onto the canonical problem document, which
/// the mapping layer stamps with `X-OAGW-Error-Source: gateway`.
fn handed_off(reply: StreamReply, path: &str, exchange: &Exchange) -> Response {
    match reply {
        StreamReply::Response(response, source, closed) => {
            let response = crate::api::rest::routes::stamp_error_source(response, source);
            // @cpt-begin:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-04
            // One line for the handed-off exchange, at its close. A relay that
            // ended before the handoff — an upstream that produced no body, an
            // upgrade the upstream refused — has already recorded that terminal
            // state, so the close is this one. A relay whose head was committed
            // still holds the exchange, and the close the handoff gave it runs
            // at the terminal state it records, covering the whole relay.
            if closed
                .stream
                .as_ref()
                .is_some_and(|record| record.outcome().is_some())
            {
                exchange.close(&closed, response.status().as_u16(), None, true, None);
            }
            // @cpt-end:cpt-cf-oagw-algo-audit-line:p1:inst-ob-aline-04
            response
        }
        StreamReply::Failed(error, context) => {
            let response = problem(error.clone(), failure_context(&context, path, None));
            exchange.close(
                &context,
                response.status().as_u16(),
                None,
                crate::infra::proxy::breaker::counts_as_failure(&error),
                Some(&error),
            );
            response
        }
    }
}

/// The canonical problem document of a mapped failure.
fn problem(error: DomainError, context: ErrorContext) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-03
    // @cpt-begin:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-04
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-03
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-04
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-05
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-07
    // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-09
    // The entry-2.1 mapping layer resolves the HTTP status, the GTS `type`
    // identifier and the title from the error table and attaches the extension
    // fields the request context provides, omitting the ones that do not
    // apply. The document never carries credential material, a resolved secret
    // value, a request body or a header value: the mapping layer holds no
    // reference to a credential store and serializes a fixed field set only.
    OagwProblem::new(&error, &context).into_response()
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-09
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-07
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-05
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-04
    // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-03
    // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-04
    // @cpt-end:cpt-cf-oagw-flow-proxy-error-source:p1:inst-pe-err-03
}

/// The canonical problem document of a pipeline failure
/// (`cpt-cf-oagw-flow-rate-limit-enforcement:p1:inst-rl-15`).
///
/// The document carries the error code the failing stage recorded —
/// REQUIRED_HEADER_MISSING of the RequiredHeaders guard — and the response
/// carries the headers the failure produces: the rate-limit headers of a
/// throttled request, `Retry-After` included.
fn failed(failure: ProxyFailure, path: &str, target_host: Option<String>) -> Response {
    let mapped = failure_context(&failure.context, path, target_host);
    let document = match failure.error_code {
        Some(code) => OagwProblem::new(&failure.error, &mapped).with_error_code(code),
        None => OagwProblem::new(&failure.error, &mapped),
    };
    let mut response = document.into_response();
    for (name, value) in failure.headers {
        response.headers_mut().insert(name, value);
    }
    response
}

/// The request context of a failure that never reached the pipeline.
///
/// The path names the alias, which is the one routing fact the transport holds
/// before the engine resolves anything.
fn transport_context(path: &str) -> ErrorContext {
    let mapped = ErrorContext::for_request(path);
    match alias_of(path) {
        Some(alias) => mapped.with_alias(alias),
        None => mapped,
    }
}

/// The request context the problem document of a pipeline failure is built from.
///
/// The pipeline closed its own context at the failure, so the routing facts it
/// recorded — the correlation identifier, the alias, the upstream and the
/// endpoint host — are attached here, and the rejected routing-header value is
/// attached from the request. Nothing of the request body and no header value
/// is copied into the record.
// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-05
// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-06
// The problem body of a gateway failure carries the context's correlation
// identifier as its `trace_id` extension field, which is the same value the
// audit line of the request records as `request_id`, so a client-reported
// `trace_id` resolves to one audit line.
fn failure_context(
    context: &RequestContext,
    path: &str,
    target_host: Option<String>,
) -> ErrorContext {
    let mut mapped = ErrorContext::for_request(path).with_trace_id(context.trace_id.clone());
    if let Some(alias) = &context.alias {
        mapped = mapped.with_alias(alias.clone());
    }
    if let Some(upstream_id) = &context.upstream_id {
        mapped = mapped.with_upstream_id(upstream_id.clone());
    }
    if let Some(host) = &context.endpoint_host {
        mapped = mapped.with_host(host.clone());
    }
    if let Some(hosts) = &context.valid_hosts {
        mapped = mapped.with_valid_hosts(hosts.clone());
    }
    if let Some(value) = target_host {
        mapped = mapped.with_invalid_value(value);
    }
    mapped
}
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-06
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-05

/// The `401` of a request that reaches the proxy endpoint unauthenticated.
fn unauthenticated(path: &str) -> Response {
    OagwProblem::new(
        &DomainError::AuthenticationFailed {
            detail: "the proxy request carries no authenticated caller".to_owned(),
        },
        &transport_context(path),
    )
    .into_response()
}
// @cpt-end:cpt-cf-oagw-dod-error-mapping:p1:inst-full

/// Whether the token scopes grant `permission`.
///
/// The wildcard scope is what the toolkit issues to a platform operator; an
/// empty scope set asserts nothing and grants nothing.
fn is_granted(scopes: &[String], permission: &str) -> bool {
    scopes
        .iter()
        .any(|scope| scope == "*" || scope == permission)
}

/// The correlation identifier the request carries, or a fresh one.
///
/// The W3C `traceparent` header is preferred, then a caller-supplied
/// `X-Request-Id`, then an identifier the gateway generates, so the context
/// always carries one to the response and to entry 2.7.
#[must_use]
pub fn correlation_id(headers: &HeaderMap) -> String {
    let from_traceparent = headers.get(TRACEPARENT).and_then(|value| {
        value
            .to_str()
            .ok()
            .and_then(|value| value.split('-').nth(1))
            .filter(|trace| !trace.is_empty())
            .map(str::to_owned)
    });
    let from_request_id = headers
        .get(REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    from_traceparent
        .or(from_request_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string())
}

/// Split the request path into the alias and the path suffix to match.
///
/// The handler sees the mount-relative path `/proxy/{alias}` or
/// `/proxy/{alias}/{suffix}`, while a problem document names the full request
/// path the caller sent, so the optional [`MOUNT_ROOT`] prefix is stripped too
/// and both shapes resolve to the same alias.
///
/// The suffix is returned as an absolute path (`/v1/things`), which is the shape
/// the route match keys on: a route prefix is compared on a path-segment
/// boundary, so a suffix that had its leading slash stripped by the mount would
/// never match the route that declares `/v1`.
fn split_path(path: &str) -> (String, String) {
    let mounted = path.strip_prefix(MOUNT_ROOT).unwrap_or(path);
    // A path that is not the proxy subtree carries no alias at all.
    let Some(rest) = mounted.strip_prefix(PROXY_PREFIX) else {
        return (String::new(), String::new());
    };
    let rest = rest.trim_start_matches('/');
    // A trailing slash is the alias path with no suffix, not a suffix of `""`
    // that would forward an empty segment.
    let (alias, suffix) = match rest.split_once('/') {
        Some((alias, "")) => (alias, ""),
        Some((alias, suffix)) => (alias, suffix),
        None => (rest, ""),
    };
    // The suffix is the absolute path the route match compares its prefix
    // against, so it carries the leading slash the mount stripped.
    let suffix = if suffix.is_empty() {
        String::new()
    } else {
        format!("/{suffix}")
    };
    (alias.to_owned(), suffix)
}

/// The alias a problem document names, when the path carries one.
fn alias_of(path: &str) -> Option<String> {
    let (alias, _) = split_path(path);
    if alias.is_empty() {
        None
    } else {
        Some(alias)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn test_headers(entries: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in entries {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("a header name"),
                axum::http::HeaderValue::from_str(value).expect("a header value"),
            );
        }
        headers
    }

    #[test]
    fn a_preflight_shape_is_options_with_origin_and_request_method() {
        assert!(is_preflight(
            "OPTIONS",
            &test_headers(&[
                ("origin", "https://app.dev"),
                ("access-control-request-method", "POST")
            ])
        ));
        assert!(is_preflight(
            "options",
            &test_headers(&[
                ("origin", "https://app.dev"),
                ("access-control-request-method", "POST")
            ])
        ));
    }

    #[test]
    fn a_request_that_is_not_a_preflight_continues_in_the_proxy_request_flow() {
        // Any shape that does not match is an actual proxy request and reaches
        // the alias walk only after authentication.
        assert!(!is_preflight("OPTIONS", &test_headers(&[("origin", "https://app.dev")])));
        assert!(!is_preflight(
            "OPTIONS",
            &test_headers(&[("access-control-request-method", "POST")])
        ));
        assert!(!is_preflight("GET", &test_headers(&[("origin", "https://app.dev")])));
    }

    #[test]
    fn a_preflight_is_answered_locally_with_the_echoed_headers() {
        let response = preflight_response(&test_headers(&[
            ("origin", "https://app.dev"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-trace"),
        ]));
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://app.dev")
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-methods")
                .and_then(|value| value.to_str().ok()),
            Some("POST")
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-headers")
                .and_then(|value| value.to_str().ok()),
            Some("x-trace")
        );
        assert_eq!(
            response
                .headers()
                .get("access-control-max-age")
                .and_then(|value| value.to_str().ok()),
            Some(PREFLIGHT_MAX_AGE)
        );
        assert_eq!(
            response
                .headers()
                .get(http::header::VARY)
                .and_then(|value| value.to_str().ok()),
            Some(PREFLIGHT_VARY)
        );
    }

    #[test]
    fn a_preflight_answer_is_classified_as_gateway_generated() {
        let response = preflight_response(&test_headers(&[("origin", "https://app.dev")]));
        assert_eq!(
            response
                .headers()
                .get(crate::api::rest::error::ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(crate::api::rest::error::ERROR_SOURCE_GATEWAY)
        );
    }

    #[test]
    fn a_proxy_request_without_a_scope_is_refused_before_the_walk() {
        // The permission constant is the one `cpt-cf-oagw-interface-api` names
        // for the proxy path.
        assert_eq!(
            PERM_PROXY_INVOKE,
            "gts.cf.core.oagw.proxy.v1~:invoke"
        );
        assert!(is_granted(&["*".to_owned()], PERM_PROXY_INVOKE));
        assert!(is_granted(
            &[PERM_PROXY_INVOKE.to_owned()],
            PERM_PROXY_INVOKE
        ));
        assert!(!is_granted(&[], PERM_PROXY_INVOKE));
        assert!(!is_granted(&["gts.cf.core.oagw.upstream.v1~:read".to_owned()], PERM_PROXY_INVOKE));
    }

    #[test]
    fn the_correlation_identifier_prefers_the_trace_context() {
        let trace = test_headers(&[(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        )]);
        assert_eq!(
            correlation_id(&trace),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        let named = test_headers(&[("x-request-id", "caller-id")]);
        assert_eq!(correlation_id(&named), "caller-id");
        assert_eq!(correlation_id(&HeaderMap::new()).len(), 32);
    }

    #[test]
    fn the_header_a_caller_supplied_is_used_when_the_trace_context_is_absent() {
        let named = test_headers(&[("x-request-id", "caller-id")]);
        assert_eq!(correlation_id(&named), "caller-id");
    }

    #[test]
    fn a_traceparent_without_a_trace_identifier_falls_back() {
        let short = test_headers(&[("traceparent", "00-")]);
        assert_eq!(correlation_id(&short).len(), 32);
    }

    #[test]
    fn the_path_splits_into_the_alias_and_the_suffix() {
        assert_eq!(
            split_path("/proxy/api.vendor.com"),
            ("api.vendor.com".to_owned(), String::new())
        );
        // The suffix is the absolute path the route match compares the route
        // prefix against.
        assert_eq!(
            split_path("/proxy/api.vendor.com/v1/things"),
            ("api.vendor.com".to_owned(), "/v1/things".to_owned())
        );
        assert_eq!(
            split_path("/proxy/api.vendor.com/v1"),
            ("api.vendor.com".to_owned(), "/v1".to_owned())
        );
        assert_eq!(
            split_path("/proxy/api.vendor.com/"),
            ("api.vendor.com".to_owned(), String::new())
        );
    }

    #[test]
    fn the_problem_context_names_the_alias_the_path_carries() {
        let context = transport_context("/oagw/v1/proxy/api.vendor.com/v1");
        assert_eq!(context.alias.as_deref(), Some("api.vendor.com"));
        assert_eq!(context.instance.as_deref(), Some("/oagw/v1/proxy/api.vendor.com/v1"));
        assert!(transport_context("/oagw/v1/proxy").alias.is_none());
    }

    #[test]
    fn the_failure_context_carries_the_routing_facts_the_pipeline_recorded() {
        let mut context = crate::infra::proxy::context::RequestContext::new(
            "trace-1".to_owned(),
            "/v1".to_owned(),
            "GET".to_owned(),
        );
        context.alias = Some("api.vendor.com".to_owned());
        context.upstream_id = Some("u-1".to_owned());
        context.endpoint_host = Some("a.vendor.com".to_owned());
        context.valid_hosts = Some(vec!["a.vendor.com".to_owned(), "b.vendor.com".to_owned()]);
        let mapped = failure_context(
            &context,
            "/oagw/v1/proxy/api.vendor.com",
            Some("a.vendor.com:443".to_owned()),
        );
        assert_eq!(mapped.trace_id.as_deref(), Some("trace-1"));
        assert_eq!(mapped.alias.as_deref(), Some("api.vendor.com"));
        assert_eq!(mapped.upstream_id.as_deref(), Some("u-1"));
        assert_eq!(mapped.host.as_deref(), Some("a.vendor.com"));
        assert_eq!(mapped.invalid_value.as_deref(), Some("a.vendor.com:443"));
        assert_eq!(
            mapped.valid_hosts.as_deref(),
            Some(
                &["a.vendor.com".to_owned(), "b.vendor.com".to_owned()][..]
            )
        );
        // The record is a routing record: no body and no header value is in it.
        let rendered = format!("{mapped:?}").to_lowercase();
        assert!(!rendered.contains("bearer"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn a_header_value_that_cannot_be_represented_is_not_echoed() {
        // An unrepresentable `Origin` leaves the echo out instead of failing.
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::ORIGIN,
            HeaderValue::from_bytes(b"\xff").expect("a byte value"),
        );
        let response = preflight_response(&headers);
        assert!(response.headers().get("access-control-allow-origin").is_none());
    }
}
