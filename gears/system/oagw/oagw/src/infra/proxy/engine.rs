//! The proxy pipeline.
//!
//! [`ProxyEngine::serve`] is the data-plane entry point: it walks the tenant
//! hierarchy for the addressed alias, matches a route, merges the effective
//! configuration, runs the request-phase hook point, selects an endpoint,
//! validates the request, transforms the headers, hands an upgrade off before
//! the upstream call, evaluates the circuit breaker, opens the upstream exchange
//! and classifies the response. One request, one call, no retry and no cache.
//!
//! The pipeline carries a [`RequestContext`] from ingress to response and drives
//! it through `cpt-cf-oagw-state-request-lifecycle`, so a stage can never be
//! skipped. The transport maps the outcome onto the response the entry-2.1
//! layers produce: the problem document, the error-source header and the
//! measurements entry 2.7 records.

use std::sync::Arc;

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode};
use toolkit_security::SecurityContext;
use url::form_urlencoded;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Protocol, Scheme, Upstream};
use crate::domain::sharing::TenantHierarchy;
use crate::infra::proxy::breaker::{COOL_DOWN, counts_as_failure, CallOutcome, CircuitBreaker};
use crate::infra::proxy::call::{CallReply, HttpVersion, OutboundRequest, UpstreamCaller};
use crate::infra::proxy::context::{RequestContext, RequestPhase, ResponseContext};
use crate::infra::proxy::effective::merge;
use crate::infra::proxy::endpoint::{select, RoundRobin, Selection, TARGET_HOST_HEADER};
use crate::infra::proxy::hooks::{PluginChains, PluginRequest, RequestEffects, consume_rejection};
use crate::infra::proxy::passthrough::{classify, Classification};
use crate::infra::proxy::rate_limit;
use crate::infra::proxy::route_match::{candidates, match_route, route_pattern, RouteMatch};
use crate::infra::proxy::validate::{check_buffered, framing, validate_cors, validate_header_section, CorsDecision};
use crate::infra::proxy::walk::walk;
use crate::infra::storage::ConfigSnapshot;

/// The URL of the selected endpoint for a forward path and query.
#[must_use]
pub fn url_for(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    let authority = if endpoint.port == endpoint.scheme.default_port() {
        endpoint.host.clone()
    } else {
        format!("{}:{}", endpoint.host, endpoint.port)
    };
    match query {
        Some(query) => format!("{}://{}{}?{}", endpoint.scheme.as_str(), authority, path, query),
        None => format!("{}://{}{}", endpoint.scheme.as_str(), authority, path),
    }
}

/// The request the pipeline serves, already resolved by the transport.
pub struct ProxyRequest {
    /// The published snapshot the transport read once.
    pub snapshot: Arc<ConfigSnapshot>,
    /// The security context of the authenticated caller.
    pub security: SecurityContext,
    /// The calling tenant, from the security context.
    pub tenant_id: Uuid,
    /// The correlation identifier the pipeline carries.
    pub trace_id: String,
    /// The `{alias}` path segment the request addressed.
    pub alias: String,
    /// The HTTP method of the request.
    pub method: String,
    /// The path to match routes against, without the proxy prefix and the alias.
    pub match_path: String,
    /// The query string, without the leading `?`.
    pub query: Option<String>,
    /// The inbound header set.
    pub headers: HeaderMap,
    /// The buffered request body.
    pub body: Bytes,
    /// The `Origin` header of the request, when it carried one.
    pub origin: Option<String>,
    /// The peer address of the inbound connection, when the host provided one.
    pub peer_ip: Option<String>,
}

/// What the pipeline did with the exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// A complete response was passed through.
    Passthrough,
    /// An open exchange was handed to entry 2.6 on the response side.
    Streamed,
    /// An upgrade exchange was handed to entry 2.6 on the request side, before
    /// the upstream call.
    Upgrade,
}

/// The pipeline's outcome.
#[derive(Debug)]
pub struct ProxyOutcome {
    /// What the pipeline did with the exchange.
    pub kind: OutcomeKind,
    /// The status the upstream returned, or the status the handoff declares.
    pub status: StatusCode,
    /// The transformed headers of the response.
    pub headers: HeaderMap,
    /// The response body, unread while the exchange stayed open. An upgrade
    /// handoff carries no body: the dialing is entry 2.6's.
    pub body: Option<toolkit_http::ResponseBody>,
    /// The HTTP version the exchange spoke.
    pub http_version: HttpVersion,
    /// The error source the entry-2.1 header layer stamps.
    pub error_source: &'static str,
    /// The CORS headers the response carries.
    pub cors: CorsDecision,
    /// The selected endpoint, present when the outcome handed an upgrade off.
    pub endpoint: Option<Endpoint>,
    /// The absolute URL of the selected endpoint the upgrade dial connects to,
    /// present when the outcome handed an upgrade off: the dialing itself is
    /// entry 2.6's, and the target is what the pipeline resolved for it.
    pub target: Option<String>,
    /// The closed request context, for entry 2.7.
    pub context: RequestContext,
    /// The in-flight accounting the pipeline opened with the context, handed
    /// back so the transport releases it when the exchange is over and not
    /// when `serve` returns: a handed-off exchange is still open during the
    /// relay, and `oagw_requests_in_flight` and the `active` connection value
    /// report it for that whole interval.
    pub in_flight: Option<crate::infra::obs::metrics::InFlightGuard>,
}

/// A pipeline failure, with the request context it closed.
///
/// The context is returned with the error so the transport can build the
/// problem document out of the routing facts the pipeline recorded — the alias,
/// the upstream, the endpoint host and the correlation identifier — without
/// re-resolving anything, and so entry 2.7 sees one closed context per request.
#[derive(Debug)]
pub struct ProxyFailure {
    /// The mapped error of the stage that failed.
    pub error: DomainError,
    /// The request context, closed at the state the failure reached.
    pub context: RequestContext,
    /// The headers the response to this failure carries: the rate-limit headers
    /// of a throttled request (`inst-rl-15`, `inst-arp-08`).
    pub headers: Vec<(http::HeaderName, HeaderValue)>,
    /// The error code the problem body names, when the failing stage carries
    /// one (`inst-arh-08`).
    pub error_code: Option<&'static str>,
}

/// Close `context` at `failed` and carry it back with `error`.
fn failure(error: DomainError, mut context: RequestContext) -> ProxyFailure {
    context.fail(&error);
    let headers = rate_limit_headers(&context);
    let error_code = context.error_code;
    ProxyFailure {
        error,
        headers,
        error_code,
        context,
    }
}

/// The rate-limit headers of a throttled request (`inst-rl-15`).
///
/// A request the rate-limit evaluation did not throttle carries none of them,
/// so the headers are produced for a `rejected` disposition only.
fn rate_limit_headers(context: &RequestContext) -> Vec<(http::HeaderName, HeaderValue)> {
    let Some(observation) = context.rate_limit.as_ref().filter(|observation| {
        observation.response_headers && observation.decision == rate_limit::REJECTED_DECISION
    }) else {
        return Vec::new();
    };
    let mut headers = Vec::new();
    let mut push = |name: &'static str, value: String| {
        if let (Ok(name), Ok(value)) = (
            http::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            headers.push((name, value));
        }
    };
    push("x-ratelimit-limit", observation.limit.to_string());
    push("x-ratelimit-remaining", observation.remaining.to_string());
    if let Some(reset) = observation.reset {
        push("x-ratelimit-reset", reset.to_string());
    }
    if let Some(seconds) = observation.retry_after {
        push("retry-after", seconds.to_string());
    }
    headers
}

/// The proxy engine: the pipeline stages, in execution order.
pub struct ProxyEngine {
    hierarchy: Arc<dyn TenantHierarchy>,
    caller: UpstreamCaller,
    cursors: RoundRobin,
    chains: Arc<dyn PluginChains>,
    breaker: CircuitBreaker,
    /// The observability layer the in-flight series and the upstream-call
    /// duration are recorded through, when the transport attached one.
    obs: Option<Arc<crate::infra::obs::Observability>>,
}

impl ProxyEngine {
    /// An engine over `hierarchy` and `caller`, with the hook points of
    /// `chains`.
    #[must_use]
    pub fn new(
        hierarchy: Arc<dyn TenantHierarchy>,
        caller: UpstreamCaller,
        chains: Arc<dyn PluginChains>,
    ) -> Self {
        // The breaker's probe window is the window a call has, because an
        // admission that outlived its call can only be a request the runtime
        // dropped between the admission and the outcome.
        let breaker = CircuitBreaker::with_windows(COOL_DOWN, caller.timeout());
        Self {
            hierarchy,
            caller,
            cursors: RoundRobin::default(),
            chains,
            breaker,
            obs: None,
        }
    }

    /// Attach the observability layer of entry 2.7.
    ///
    /// The engine records no metric and writes no audit line itself: it hands
    /// the request-context lifecycle and the upstream-call stage to the layer,
    /// which owns the families. An engine built without one behaves exactly as
    /// it did before, so every existing call site is unchanged.
    #[must_use]
    pub fn with_obs(mut self, obs: Arc<crate::infra::obs::Observability>) -> Self {
        self.obs = Some(obs);
        self
    }

    /// The observability layer attached to this engine, when there is one.
    #[must_use]
    pub fn observability(&self) -> Option<&Arc<crate::infra::obs::Observability>> {
        self.obs.as_ref()
    }

    /// The breakers, for the observability layer.
    #[must_use]
    pub const fn breaker(&self) -> &CircuitBreaker {
        &self.breaker
    }

    /// The round-robin cursors, for the observability layer.
    #[must_use]
    pub const fn cursors(&self) -> &RoundRobin {
        &self.cursors
    }

    /// Serve one proxy request.
    ///
    /// # Errors
    ///
    /// Returns the mapped error of the stage that failed with the request
    /// context it closed, which the transport turns into the problem document
    /// the error table describes.
    pub async fn serve(&self, request: ProxyRequest) -> Result<ProxyOutcome, ProxyFailure> {
        let mut context = RequestContext::new(
            request.trace_id.clone(),
            request.match_path.clone(),
            request.method.clone(),
        );
        context.cross_origin = request.origin.is_some();

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-03
        // The context is opened with the correlation identifier the pipeline
        // carries to the response and to entry 2.7. The calling tenant the
        // transport resolved is not carried into the proxy context: the walk
        // below takes the tenant the request's security context names.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-03

        // @cpt-begin:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-02
        // @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-02
        // Entry 2.7 is called at the close of this context and reads nothing
        // else: the context carries the correlation identifier from the step
        // above (`inst-pe-req-03`) to the outcome it is closed with at
        // `inst-pe-req-30`.
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // The in-flight series is emitted from the request-context lifecycle:
        // the pipeline opens the context here and closes it at `inst-pe-req-30`,
        // and each of those two steps hands an in-flight change to
        // `cpt-cf-oagw-algo-metric-emit`. The guard is dropped on every exit of
        // `serve`, so a rejected request closes the same context it opened and
        // the gauge cannot leak an increment.
        let in_flight = self
            .obs
            .as_ref()
            .map(|observability| observability.request_opened());
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-02
        // @cpt-end:cpt-cf-oagw-flow-request-audit:p1:inst-ob-audit-02

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-06
        let walked = match walk(
            self.hierarchy.as_ref(),
            &request.security,
            request.tenant_id,
            &request.snapshot,
            &request.alias,
        )
        .await
        {
            Ok(walked) => walked,
            Err(error) => {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-09
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-10
                // A disabled closest match arrives as the walk's link-unavailable
                // outcome and is returned unmapped: the transport resolves the
                // `503` problem body from it, and the walk never fell through to
                // an ancestor target to produce it.
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-10
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-09
                return Err(failure(error, context));
            }
        };
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-06

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-07
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-08
        // A gRPC upstream and a `wt`-scheme upstream have no reachable proxy
        // path in this pipeline: neither carries an HTTP match key, so the
        // request is not routed and issues no upstream call.
        if walked.selected.protocol == Protocol::Grpc || is_ws_scheme(&walked.selected) {
            return Err(failure(
                DomainError::RouteNotFound {
                    detail: format!(
                        "the upstream `{}` has no reachable proxy path",
                        walked.selected.alias
                    ),
                },
                context,
            ));
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-08
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-07

        context.alias = Some(walked.selected.alias.clone());
        context.upstream_id = Some(walked.upstream_id().to_string());
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // The context's host is resolved here, so the in-flight change the open
        // step handed over is published under the resolved upstream alias from
        // this point on, and under no invented host value before it.
        if let Some(guard) = in_flight.as_ref() {
            guard.resolve_host(&walked.selected.alias);
        }
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a

        // The pool hosts the caller may name: the ADR 0007 `valid_hosts` a
        // target-host error reports. It is a routing fact of the selected
        // upstream, never a credential and never a header value.
        context.valid_hosts = Some(crate::infra::proxy::endpoint::valid_hosts(
            &walked.selected.server,
        ));
        if let Err(error) = context.advance(RequestPhase::Resolved) {
            return Err(failure(error, context));
        }

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-11
        let route_candidates =
            candidates(&request.snapshot, &walked.tenants, walked.upstream_id());
        let matched = match match_route(
            &walked.selected,
            &route_candidates,
            &request.method,
            &request.match_path,
            request.query.as_deref(),
        ) {
            Ok(matched) => matched,
            Err(error) => return Err(failure(error, context)),
        };
        context.matched_route = route_pattern(&matched.route);
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-11

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-12
        let effective = merge(
            &request.snapshot,
            &walked.selected,
            &matched.route,
            &walked.tenants,
            &walked.shadowed,
            &walked.selected.tags,
        );
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-12

        if let Err(error) = context.advance(RequestPhase::Matched) {
            // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-02
            // `resolved` -> `matched` happens only when a route matched the
            // method and path and its guard rules passed, which the match above
            // already decided.
            // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-02
            return Err(failure(error, context));
        }

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-13
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-14
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-15
        // The hook point sits after the merge and before endpoint selection: a
        // rejection stops the pipeline here and no upstream call is made.
        let plugin_request = PluginRequest {
            security: &request.security,
            tenant_id: request.tenant_id,
            method: &request.method,
            query: request.query.as_deref(),
            headers: &request.headers,
            origin: request.origin.as_deref(),
            peer_ip: request.peer_ip.as_deref(),
        };
        let effects = match self
            .chains
            .request_phase(&effective, &effective.plugins, &plugin_request, &mut context)
            .await
        {
            Ok(effects) => effects,
            Err(error) => {
                let mapped = consume_rejection(&error, &mut context);
                return Err(failure(mapped, context));
            }
        };
        let RequestEffects {
            headers: injected,
            query: injected_query,
            cors: chain_cors,
        } = effects;
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-15
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-14
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-13

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-16
        let selection =
            match select(&walked.selected, target_host(&request.headers), &self.cursors) {
                Ok(selection) => selection,
                Err(error) => return Err(failure(error, context)),
            };
        context.endpoint_host = Some(selection.endpoint.host.clone());
        context.selection = Some(selection.method);
        // @cpt-begin:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // The endpoint the request selected is an open exchange of that host
        // until the context closes, which is the `active` state of
        // `oagw_upstream_connections`.
        if let Some(guard) = in_flight.as_ref() {
            guard.resolve_endpoint(&selection.endpoint.host);
        }
        // @cpt-end:cpt-cf-oagw-flow-runtime-state-observation:p1:inst-ob-obs-03a
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-16

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-17
        // The framing is checked against the buffered body here; the declared
        // half of the check ran in the transport before the body was read, so a
        // 413 never depends on a body the gateway already consumed.
        let request_framing = match framing(&request.headers) {
            Ok(framing) => framing,
            Err(error) => return Err(failure(error, context)),
        };
        if let Err(error) = validate_header_section(&request.headers) {
            return Err(failure(error, context));
        }
        let actual = u64::try_from(request.body.len()).unwrap_or(u64::MAX);
        if let Err(error) = check_buffered(&request_framing, actual) {
            return Err(failure(error, context));
        }
        context.request_bytes = Some(actual);
        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-01
        // The chain evaluated the actual cross-origin request with the merged
        // configuration, so its decision is the one the response carries; the
        // pipeline evaluates its own only when the hook produced none, which is
        // the empty chain a mount without plugins is wired with.
        let cors = match chain_cors {
            Some(cors) => cors,
            None => match validate_cors(&effective, request.origin.as_deref(), &request.method) {
                Ok(cors) => cors,
                Err(error) => return Err(failure(error, context)),
            },
        };
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-01

        // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-12
        // The decision is carried on the outcome, and the response phase of the
        // transport adds `Access-Control-Allow-Origin`, the configured
        // `Access-Control-Expose-Headers`, `Access-Control-Allow-Credentials`
        // when applicable and `Vary: Origin`.
        // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-co-12
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-17

        // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-08
        // The size is recorded on the context and nothing of the body or of the
        // headers is.
        // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-08

        // @cpt-begin:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-09
        // Every check passed: the validated request and its buffered body are
        // what the rest of the pipeline forwards, and no body byte and no
        // header value of it is recorded anywhere.
        // @cpt-end:cpt-cf-oagw-algo-request-validation:p1:inst-pe-bv-09

        // The validation above and the hook point before it are the two gates
        // the lifecycle names for this transition, so the request context
        // reaches `validated` here and only here.
        if let Err(error) = context.advance(RequestPhase::Validated) {
            // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-03
            // `matched` -> `validated` happens when the request, body and CORS
            // validation passed and the request-phase hook points did not
            // reject; a hook rejection or a validation failure leaves the
            // context on the failure path below instead.
            // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-03
            return Err(failure(error, context));
        }

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18
        let mut outbound = match crate::infra::proxy::headers::transform_request(
            &request.headers,
            &effective.headers,
            &selection.endpoint,
            request.body.len(),
        ) {
            Ok(outbound) => outbound,
            Err(error) => return Err(failure(error, context)),
        };
        // The chain returned its mutations instead of applying them, so the
        // pipeline stays the one writer of the outbound surface: the credentials
        // the auth stage resolved and the identifiers the transforms set are
        // written here, after the declared transformation
        // (`cpt-cf-oagw-flow-plugin-chain`).
        for (name, value) in injected {
            outbound.insert(name, value);
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b-1
        // An upgrade request leaves the pipeline before the upstream call: the
        // selected endpoint, the transformed header set and the request context
        // are the handoff, and neither the breaker nor the response
        // classification runs for an upgrade exchange.
        if is_upgrade(&request.headers) {
            let endpoint = selection.endpoint.clone();
            // The dial target is what the route match resolved, with the scheme
            // the upgrade is carried over: the relay dials the stored endpoint
            // and never a client-supplied host.
            let target = upgrade_target(
                &endpoint,
                &matched.forward_path,
                forwarded_query(matched.forward_query.as_deref(), &injected_query).as_deref(),
            );
            return Ok(ProxyOutcome {
                kind: OutcomeKind::Upgrade,
                status: StatusCode::SWITCHING_PROTOCOLS,
                headers: outbound,
                body: None,
                http_version: HttpVersion::Http11,
                error_source: crate::infra::proxy::passthrough::ERROR_SOURCE_GATEWAY,
                cors,
                endpoint: Some(endpoint),
                target: Some(target),
                context,
                in_flight,
            });
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b-1
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-18b

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-19
        // An open breaker refuses the call before any connection attempt.
        if let Err(error) = self.breaker.admit(&selection.endpoint.host) {
            return Err(failure(error, context));
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-19

        let reply = self
            .dial(
                &selection,
                &outbound,
                &request,
                &matched,
                &injected_query,
                context.alias.as_deref().unwrap_or_default(),
            )
            .await;

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-20
        if let Err(error) = context.advance(RequestPhase::Forwarded) {
            // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-04
            // `validated` -> `forwarded` is reached once the endpoint is
            // selected, the headers transformed, the breaker admitted the call
            // and the call sent; the dial above is that call.
            // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-04
            return Err(failure(error, context));
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-20

        let reply = match reply {
            Ok(reply) => {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-23
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-24
                // A response the upstream produced is a breaker success, even an
                // error status: the breaker counts the call stage, not the
                // answer.
                self.breaker
                    .record(&selection.endpoint.host, CallOutcome::Success);
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-24
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-23
                reply
            }
            Err(error) => {
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-21
                // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-22
                // @cpt-begin:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-08
                // A failed call is recorded on the breaker when it is one of the
                // upstream-call failures, its GTS type is recorded on the
                // request context for entry 2.7 and the mapped error is
                // returned: no second request is issued. A failure the breaker
                // does not count is recorded as a neutral outcome, because the
                // admission it closed out still owes the breaker an outcome: an
                // unrecorded one would hold the half-open probe slot for good.
                if counts_as_failure(&error) {
                    self.breaker
                        .record(&selection.endpoint.host, CallOutcome::Failure);
                } else {
                    self.breaker
                        .record(&selection.endpoint.host, CallOutcome::Neutral);
                }
                return Err(failure(error, context));
                // @cpt-end:cpt-cf-oagw-algo-proxy-error-mapping:p1:inst-pe-em-08
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-22
                // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-21
            }
        };

        context.http_version = Some(reply.version);
        let classified = classify(reply);

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-25
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-26
        // A streamed response leaves the pipeline unread: the streamed session,
        // its lifecycle and its error mapping are entry 2.6's.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-26
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-25
        let streamed = classified.context.streamed;
        let outcome = if streamed {
            let headers = classified_headers(&classified);
            if let Err(error) = context.advance(RequestPhase::HandedOff) {
                // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-06
                // `forwarded` -> `handed_off`: the open exchange leaves this
                // pipeline for the entry-2.6 session, which this build does not
                // implement, so the transport ends the exchange with a gateway
                // error instead of an incomplete relay.
                // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-06
                return Err(failure(error, context));
            }
            ProxyOutcome::from_classification(
                OutcomeKind::Streamed,
                classified,
                headers,
                &context,
                cors,
                in_flight,
            )
        } else {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-27
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-28
            // The response-side transformation runs and the body passes through
            // unchanged.
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-28
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-27
            let headers = crate::infra::proxy::headers::transform_response(
                &classified.headers,
                Some(&effective.headers.response),
            )
            .unwrap_or_else(|_| classified_headers(&classified));
            if let Err(error) = context.advance(RequestPhase::Responded) {
                // @cpt-begin:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-05
                // `forwarded` -> `responded` is reached when a complete upstream
                // response was received and is passed through.
                // @cpt-end:cpt-cf-oagw-state-request-lifecycle:p1:inst-pe-srl-05
                return Err(failure(error, context));
            }
            ProxyOutcome::from_classification(
                OutcomeKind::Passthrough,
                classified,
                headers,
                &context,
                cors,
                in_flight,
            )
        };

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-29
        // The response-phase hook point runs on the classified response; an
        // error-phase hook would run on the mapped error instead. The chain
        // mutates the response header set in place, so the pipeline still owns
        // the surface the caller reads.
        let mut outcome = outcome;
        let response = outcome.response_context();

        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-09
        // @cpt-begin:cpt-cf-oagw-flow-response-phase:p1:inst-rp-10
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-07
        // @cpt-begin:cpt-cf-oagw-algo-response-phase:p1:inst-arp-08
        // A call that failed left the pipeline with a mapped error instead of a
        // response: the failure path carries the rate-limit headers of a
        // throttled request (`Retry-After` on every throttled response, the
        // `X-RateLimit-*` triple when the configuration asked for them) beside
        // the error context, and the error-phase transforms of the entry-2.1
        // header layer run against that context.
        if let Err(error) = self
            .chains
            .response_phase(
                &effective,
                &effective.plugins,
                &response,
                &mut outcome.headers,
                &mut context,
            )
            .await
        {
            let mapped = consume_rejection(&error, &mut context);
            return Err(failure(mapped, context));
        }
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-08
        // @cpt-end:cpt-cf-oagw-algo-response-phase:p1:inst-arp-07
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-10
        // @cpt-end:cpt-cf-oagw-flow-response-phase:p1:inst-rp-09
        // The context the hook recorded the response-phase outcomes on is the
        // one entry 2.7 reports.
        outcome.context = context.clone();
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-29

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-30
        // The entry-2.1 header layer stamps `X-OAGW-Error-Source` from the
        // outcome's error source, on success and on failure alike; the context
        // is closed with the measurements entry 2.7 records.
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-30

        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-31
        Ok(outcome)
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-pe-req-31
    }

    /// Dial the selected endpoint once, with the transformed headers.
    async fn dial(
        &self,
        selection: &Selection,
        outbound: &HeaderMap,
        request: &ProxyRequest,
        matched: &RouteMatch,
        injected_query: &[(String, String)],
        alias: &str,
    ) -> Result<CallReply, DomainError> {
        // The alias the caller passes is the resolved upstream alias the walk
        // normalized (`inst-pe-req-06`), not the `{alias}` path segment the
        // request spelled: the `host` label of the `upstream` phase is the same
        // bounded value every other family labels with, and the spelling the
        // request used never becomes a label value.
        let headers: Vec<(http::HeaderName, HeaderValue)> = outbound
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        let request = OutboundRequest {
            method: request
                .method
                .parse::<http::Method>()
                .map_err(|_| DomainError::ValidationError {
                    detail: "the request method is not a valid HTTP method".to_owned(),
                })?,
            url: url_for(
                &selection.endpoint,
                &matched.forward_path,
                forwarded_query(matched.forward_query.as_deref(), injected_query).as_deref(),
            ),
            headers,
            body: request.body.clone(),
            endpoint: selection.endpoint.clone(),
        };
        // @cpt-begin:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-04
        // The upstream-call stage is observed as the `upstream` phase of the
        // request-duration histogram, over the same twelve buckets; the
        // gateway-added phase is observed by the transport at the close of the
        // request.
        let upstream_started = std::time::Instant::now();
        let reply = self.caller.call(request).await;
        if let Some(observability) = self.obs.as_ref() {
            // The `host` label is the resolved upstream alias on every family,
            // the `upstream` phase included, so the two phases of one request
            // share the label set the route and the alias name.
            observability.observe_upstream_duration(
                alias.as_ref(),
                route_pattern(&matched.route).as_deref().unwrap_or_default(),
                upstream_started.elapsed(),
            );
        }
        reply
        // @cpt-end:cpt-cf-oagw-algo-metric-emit:p1:inst-ob-aemit-04
    }
}

/// The forwarded query string: the route's query with the members the chain
/// appended (`inst-ai-08`).
///
/// The route's own query passes through untouched when no member was appended,
/// so a request without a plugin keeps the query byte for byte.
fn forwarded_query(base: Option<&str>, injected: &[(String, String)]) -> Option<String> {
    if injected.is_empty() {
        return base.map(str::to_owned);
    }
    let mut serializer = form_urlencoded::Serializer::new(String::new());
    if let Some(base) = base {
        for pair in base.split('&').filter(|pair| !pair.is_empty()) {
            match pair.split_once('=') {
                Some((key, value)) => serializer.append_pair(key, value),
                None => serializer.append_pair(pair, ""),
            };
        }
    }
    for (key, value) in injected {
        serializer.append_pair(key, value);
    }
    Some(serializer.finish())
}

/// The upstream headers of a classification, for the fallback path.
fn classified_headers(classified: &Classification) -> HeaderMap {
    classified.headers.clone()
}

impl ProxyEngine {
    /// The upstream caller, for the entry-2.6 handoff that dials the endpoint
    /// over the same client stack the entry-2.4 call uses.
    #[must_use]
    pub const fn caller(&self) -> &UpstreamCaller {
        &self.caller
    }
}

/// The URL the upgrade dial connects to (`inst-ss-upg-07`).
///
/// The upgrade is carried over the HTTP scheme the endpoint's scheme negotiates:
/// `wss` dials `https`, so the existing client stack — which validates an
/// `http`/`https` URL only — dials the stored endpoint unchanged. The path and
/// the query are the ones the route match resolved.
///
/// No `wt` arm exists here by construction: a `wt`-scheme upstream is excluded
/// at request time by `cpt-cf-oagw-feature-proxy-engine`'s alias-resolution
/// step, so no exchange this entry-2.6 dial builds can carry one. The arm is
/// folded into the unreachable scheme set rather than dialled as `http`, which
/// would be a WebTransport code path this feature owns none of
/// (`cpt-cf-oagw-dod-stream-scheme-posture`).
#[must_use]
pub fn upgrade_target(endpoint: &Endpoint, path: &str, query: Option<&str>) -> String {
    debug_assert!(
        !matches!(endpoint.scheme, Scheme::Wt),
        "a wt-scheme upstream is excluded before any exchange reaches the upgrade dial"
    );
    let url = url_for(endpoint, path, query);
    let dialled = match endpoint.scheme {
        Scheme::Wss => "https",
        Scheme::Http | Scheme::Https | Scheme::Grpc | Scheme::Wt => endpoint.scheme.as_str(),
    };
    url.replacen(
        &format!("{}://", endpoint.scheme.as_str()),
        &format!("{dialled}://"),
        1,
    )
}

impl ProxyOutcome {
    /// Build an outcome from a classification.
    fn from_classification(
        kind: OutcomeKind,
        classified: Classification,
        headers: HeaderMap,
        context: &RequestContext,
        cors: CorsDecision,
        in_flight: Option<crate::infra::obs::metrics::InFlightGuard>,
    ) -> Self {
        let status =
            StatusCode::from_u16(classified.context.status).unwrap_or(StatusCode::BAD_GATEWAY);
        Self {
            kind,
            status,
            headers,
            body: Some(classified.body),
            http_version: classified.context.http_version,
            error_source: classified.context.error_source,
            cors,
            endpoint: None,
            target: None,
            context: context.clone(),
            in_flight,
        }
    }

    /// The response context the response-phase hook point reads.
    #[must_use]
    pub fn response_context(&self) -> ResponseContext {
        ResponseContext {
            status: self.status.as_u16(),
            streamed: self.kind == OutcomeKind::Streamed,
            handed_off: self.kind != OutcomeKind::Passthrough,
            http_version: self.http_version,
            error_source: self.error_source,
        }
    }
}

/// The `X-OAGW-Target-Host` value of the inbound header set, read once.
fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// Whether the request carries a WebSocket upgrade.
///
/// The shape is `Upgrade: websocket` with `Connection: Upgrade`, the shape the
/// flow's upgrade step names.
#[must_use]
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let upgrade = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(str::to_ascii_lowercase);
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    upgrade.as_deref() == Some("websocket")
        && connection.split(',').any(|token| token.trim() == "upgrade")
}

/// Whether an upstream's scheme has no reachable proxy path.
fn is_ws_scheme(upstream: &Upstream) -> bool {
    upstream
        .server
        .endpoints
        .iter()
        .any(|endpoint| endpoint.scheme == Scheme::Wt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::ServerConfig;
    use http::HeaderName;

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: Scheme::Http,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(scheme: Scheme) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "api.vendor.com".to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme,
                    host: "upstream.internal".to_owned(),
                    port: 80,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            auth_plugin_ref: None,
            auth_plugin_uuid: None,
            headers: None,
            rate_limit: None,
            cors: None,
            plugins: None,
            created_at: crate::domain::model::Timestamp::now(),
        }
    }

    #[test]
    fn the_upgrade_target_carries_no_wt_scheme_and_no_wt_branch() {
        // The `wt` scheme is excluded at request time by the alias-resolution
        // step of `cpt-cf-oagw-feature-proxy-engine`, so the upgrade dial never
        // sees one: the target it builds for every scheme it can be handed is
        // carried over an `http`/`https` scheme, and a `wss` endpoint dials
        // `https` rather than being dialled as its own scheme.
        for scheme in [Scheme::Http, Scheme::Https, Scheme::Grpc] {
            let selected = upstream(scheme)
                .server
                .endpoints
                .into_iter()
                .next()
                .expect("one endpoint");
            let target = upgrade_target(&selected, "/v1/socket", None);
            assert!(
                !target.starts_with("wt://"),
                "no wt scheme is dialled: {target}"
            );
            assert!(
                target.starts_with(&format!("{}://", scheme.as_str())),
                "the stored scheme is dialled: {target}"
            );
        }
        let wss = upstream(Scheme::Wss)
            .server
            .endpoints
            .into_iter()
            .next()
            .expect("one endpoint");
        assert_eq!(
            upgrade_target(&wss, "/v1/socket", None),
            "https://upstream.internal:80/v1/socket",
            "a wss upgrade is carried over TLS"
        );
    }

    #[test]
    fn the_url_is_built_from_the_selected_endpoint() {
        let selected = endpoint("upstream.internal", 8080);
        assert_eq!(
            url_for(&selected, "/v1/things", None),
            "http://upstream.internal:8080/v1/things"
        );
        assert_eq!(
            url_for(&selected, "/v1", Some("a=1")),
            "http://upstream.internal:8080/v1?a=1"
        );
        let mut default_port = selected;
        default_port.port = 80;
        assert_eq!(
            url_for(&default_port, "/v1", None),
            "http://upstream.internal/v1"
        );
    }

    #[test]
    fn an_upgrade_request_is_detected_from_its_header_shape() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert(http::header::CONNECTION, HeaderValue::from_static("Upgrade"));
        assert!(is_upgrade(&headers));
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        assert!(is_upgrade(&headers));
        headers.insert(
            http::header::CONNECTION,
            HeaderValue::from_static("keep-alive"),
        );
        assert!(!is_upgrade(&headers));
        headers.remove(http::header::UPGRADE);
        assert!(!is_upgrade(&headers));
    }

    #[test]
    fn a_target_host_is_read_once_from_the_inbound_set() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-oagw-target-host"),
            HeaderValue::from_static("a.vendor.com"),
        );
        assert_eq!(target_host(&headers), Some("a.vendor.com"));
        assert_eq!(target_host(&HeaderMap::new()), None);
    }

    #[test]
    fn a_wt_scheme_upstream_has_no_reachable_proxy_path() {
        assert!(is_ws_scheme(&upstream(Scheme::Wt)));
        assert!(!is_ws_scheme(&upstream(Scheme::Http)));
    }

    #[test]
    fn an_upgrade_outcome_carries_the_selected_endpoint_and_no_body() {
        let outcome = ProxyOutcome {
            kind: OutcomeKind::Upgrade,
            status: StatusCode::SWITCHING_PROTOCOLS,
            headers: HeaderMap::new(),
            body: None,
            http_version: HttpVersion::Http11,
            error_source: crate::infra::proxy::passthrough::ERROR_SOURCE_GATEWAY,
            cors: CorsDecision::same_origin(),
            endpoint: Some(endpoint("upstream.internal", 80)),
            target: Some(upgrade_target(&endpoint("upstream.internal", 80), "/v1", None)),
            context: RequestContext::new("t".to_owned(), "/p".to_owned(), "GET".to_owned()),
            in_flight: None,
        };
        assert_eq!(outcome.kind, OutcomeKind::Upgrade);
        assert_eq!(outcome.response_context().status, 101);
        assert!(outcome.response_context().handed_off);
    }

    #[test]
    fn a_streamed_outcome_is_handed_off_and_a_passthrough_is_not() {
        let passthrough = ProxyOutcome {
            kind: OutcomeKind::Passthrough,
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: None,
            http_version: HttpVersion::Http11,
            error_source: crate::infra::proxy::passthrough::ERROR_SOURCE_UPSTREAM,
            cors: CorsDecision::same_origin(),
            endpoint: None,
            target: None,
            context: RequestContext::new("t".to_owned(), "/p".to_owned(), "GET".to_owned()),
            in_flight: None,
        };
        assert!(!passthrough.response_context().streamed);
        assert!(!passthrough.response_context().handed_off);
        let streamed = ProxyOutcome {
            kind: OutcomeKind::Streamed,
            ..passthrough
        };
        assert!(streamed.response_context().streamed);
        assert!(streamed.response_context().handed_off);
    }
}
