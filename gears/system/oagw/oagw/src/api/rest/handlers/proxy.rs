//! Proxy handlers — the Data Plane on the wire.
//!
//! The two routes of `cpt-cf-oagw-flow-proxy-request`: `{METHOD}
//! /oagw/v1/proxy/{alias}` and `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`.
//! Both realize the same flow, which the shared [`serve`] owns in the order the
//! FEATURE states it: authorize, resolve, match, select, validate, the
//! rate-limit seam, the chain, the header transform, the forward, and the
//! classification.
//!
//! The handler is the only place a transport type meets the Data Plane: it
//! assembles the [`ProxyContext`] from the extractor set, hands it to the
//! routines, and maps every failure through the foundation's problem mapping,
//! which sets `X-OAGW-Error-Source: gateway`; an upstream answer is passed
//! through with `upstream` instead.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, FromRequestParts, OriginalUri, Path, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use axum::Extension;
use parking_lot::Mutex;
use toolkit_security::SecurityContext;

use super::SharedState;
use crate::api::rest::problem;
use crate::control_plane::chain;
use crate::data_plane::classify::classify_upstream_head;
use crate::data_plane::endpoint::select_endpoint;
use crate::data_plane::execute::run_request_phase;
use crate::data_plane::headers::transform_request;
use crate::data_plane::match_route::{failure_of, match_route};
use crate::data_plane::observability::{
    DeferredObservation, EndpointObservation, Exchange, RateLimitObservation,
};
use crate::data_plane::validate::{read_body, validate_inbound};
use crate::data_plane::{consume, Resolution};
use crate::domain::effective::RouteSelector;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::observability::CorrelationContext;
use crate::domain::plugin_contract::SANDBOX_LIMITS;
use crate::domain::proxy::ProxyContext;
use crate::domain::stream::{StreamSession, TransferMode};

/// The upgrade handle the HTTP layer placed in the request extensions, taken
/// so the tunnel `cpt-cf-oagw-feature-streaming` carries can ride the
/// connection the caller arrived on. A request the platform delivered with no
/// such handle leaves `None`, and no tunnel is carried over it.
#[derive(Debug)]
pub struct UpgradeHandle(Option<hyper::upgrade::OnUpgrade>);

impl<S: Send + Sync> FromRequestParts<S> for UpgradeHandle {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.remove::<hyper::upgrade::OnUpgrade>()))
    }
}

/// Answers `GET /oagw/v1/proxy/{alias}` and every other method on it.
#[allow(clippy::too_many_arguments)]
pub async fn root(
    State(state): State<SharedState>,
    original: OriginalUri,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    context: Option<Extension<SecurityContext>>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    Path(alias): Path<String>,
    UpgradeHandle(upgrade): UpgradeHandle,
    body: Body,
) -> Response {
    serve(
        &state,
        method,
        alias,
        None,
        uri.query(),
        headers,
        body,
        context.map(|value| value.0),
        peer.map(|value| (value.0).0),
        upgrade,
        instance_of(&original.0, &uri),
    )
    .await
}

/// Answers `{METHOD} /oagw/v1/proxy/{alias}/{path_suffix}`.
#[allow(clippy::too_many_arguments)]
pub async fn suffix(
    State(state): State<SharedState>,
    original: OriginalUri,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    context: Option<Extension<SecurityContext>>,
    peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
    Path((alias, path_suffix)): Path<(String, String)>,
    UpgradeHandle(upgrade): UpgradeHandle,
    body: Body,
) -> Response {
    serve(
        &state,
        method,
        alias,
        Some(path_suffix),
        uri.query(),
        headers,
        body,
        context.map(|value| value.0),
        peer.map(|value| (value.0).0),
        upgrade,
        instance_of(&original.0, &uri),
    )
    .await
}

/// The observation the proxy path's exit performs, held by the request's own
/// task so every return the path takes answers through it.
///
/// The guard is armed at the path's entry, where the correlate step assigns
/// the correlation identifier and raises the in-flight gauge, and the exchange
/// it holds is the skeleton the path's steps fill as they produce values. Its
/// drop is the exit step of the observed flow, so a request that left the path
/// at any branch is still observed once and only once; the streamed exchanges
/// take the exchange out and defer the observation to the transfer's end.
struct PathExit {
    observability: Arc<crate::data_plane::observability::Observability>,
    exchange: Option<Exchange>,
    correlation: Option<CorrelationContext>,
    timings: crate::data_plane::observability::PhaseTimings,
}

impl PathExit {
    /// Arms the guard at the path's entry: the correlate step of the observed
    /// flow assigns the identifier, the gauge is raised for the alias the
    /// request addressed, and the exchange is the skeleton to fill.
    fn arm(
        observability: Arc<crate::data_plane::observability::Observability>,
        method: &Method,
        alias: &str,
        inbound: Option<&str>,
    ) -> Self {
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-correlate
        // `cpt-cf-oagw-algo-correlate` runs before any authorization step: the
        // identifier is assigned from the header the platform injected or
        // generated, the in-flight gauge is raised for the alias the request
        // addressed, and the context is recorded on the exchange the exit
        // reads and on the `ProxyContext` the path's steps read.
        let correlation = CorrelationContext::assign(inbound, None, None);
        observability.raise_in_flight(alias);
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-correlate
        Self {
            exchange: Some(Exchange {
                host: Some(String::from(alias)),
                method: method.as_str().to_ascii_uppercase(),
                ..Exchange::default()
            }),
            correlation: Some(correlation),
            observability,
            timings: crate::data_plane::observability::PhaseTimings::started(),
        }
    }

    /// The correlation context the request carries, for the `ProxyContext` the
    /// path's steps read.
    fn correlation(&self) -> Option<CorrelationContext> {
        self.correlation.clone()
    }

    /// The exchange the path's steps fill in.
    fn exchange(&mut self) -> &mut Exchange {
        self.exchange
            .as_mut()
            .expect("the exit is armed for the whole of the path")
    }

    /// Records the identity the platform resolved onto the correlation
    /// context.
    fn identify(&mut self, tenant_id: uuid::Uuid, principal_id: &str) {
        if let Some(correlation) = self.correlation.as_mut() {
            correlation.tenant_id = Some(tenant_id);
            correlation.principal_id = Some(String::from(principal_id));
        }
    }

    /// Stamps the moment the resolution completed.
    fn resolved(&mut self) {
        self.timings.resolved();
    }

    /// Stamps the moment the composed chain completed.
    fn chained(&mut self) {
        self.timings.chained();
    }

    /// Stamps the moment the outbound forward completed.
    fn forwarded(&mut self) {
        self.timings.forwarded();
    }

    /// Records the answer a gateway failure produced and maps it to the
    /// response the caller is answered with.
    ///
    /// The correlation identifier is copied into the `ErrorContext` of the
    /// error as its `trace_id` member, which
    /// `cpt-cf-oagw-algo-error-mapping` attaches to the problem body as the
    /// extension field it already reads: no problem body is built here and no
    /// second serialization path is added.
    fn gateway(&mut self, failure: &DomainError, instance: &str) -> Response {
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-gateway-if
        // The path answered the request from a gateway error it produced, so
        // the identifier is echoed and the answer is a problem body.
        let exchange = self.exchange();
        exchange.error = Some(failure.kind);
        exchange.gateway_answer = true;
        exchange.status = Some(failure.http_status());
        tracing::info!(
            instance,
            kind = failure.kind.title(),
            detail = %failure.detail,
            "proxy request refused by the gateway"
        );
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-echo
        let mut echoed = failure.clone();
        echoed.context.trace_id = self
            .correlation
            .as_ref()
            .map(|correlation| correlation.request_id.clone());
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-echo
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-gateway-if
        problem::problem_response(&echoed, instance)
    }

    /// Records the answer a bare problem response carries, which is the CORS
    /// refusal: the answer is outside the catalogue and names no error type.
    fn bare(&mut self, status: u16) {
        let exchange = self.exchange();
        exchange.gateway_answer = true;
        exchange.status = Some(status);
    }

    /// Records the answer the authorize step gave a caller whose identity or
    /// whose permission it could not establish.
    ///
    /// The answer is the platform's permission-denied problem body and not a
    /// catalogue row of this feature, so it is recorded as bare; §1.5 row 177
    /// names it as an authentication failure this feature records, so the
    /// exchange is marked for the `auth.failed` event the closed set holds.
    fn authentication_failure(&mut self, status: u16) {
        let exchange = self.exchange();
        exchange.gateway_answer = true;
        exchange.authentication_failure = true;
        exchange.status = Some(status);
    }

    /// Records that the path resolved a configured upstream before it went on
    /// to answer, which is what decides the `host` label of the answer
    /// families: a request that never resolved names no upstream and is filed
    /// under the one bounded literal instead of the alias the caller
    /// addressed.
    fn upstream_resolved(&mut self) {
        self.exchange().upstream_resolved = true;
    }

    /// Records the endpoint selection the path performed.
    fn endpoint(
        &mut self,
        upstream_id: uuid::Uuid,
        endpoint_host: String,
        choice: crate::domain::proxy::EndpointChoice,
    ) {
        self.exchange().endpoint = Some(EndpointObservation {
            upstream_id,
            endpoint_host,
            method: crate::domain::observability::selection_method_label(choice),
            used_header: choice == crate::domain::proxy::EndpointChoice::Header,
        });
    }

    /// Records the rate-limit outcome the check produced.
    fn rate_limit(&mut self, observation: RateLimitObservation) {
        self.exchange().rate_limit = Some(observation);
    }

    /// Records the breaker state and the transitions the machine reported.
    fn breaker(
        &mut self,
        phase: Option<crate::domain::ratelimit::BreakerPhase>,
        transitions: Vec<crate::domain::ratelimit::BreakerTransition>,
    ) {
        self.exchange().breaker = Some(crate::data_plane::observability::BreakerObservation {
            phase,
            transitions,
        });
    }

    /// Takes the exchange and the correlation out and returns the observation
    /// the transfer's end runs.
    ///
    /// The streamed exchanges are the ones that take it: the transfer outlives
    /// the response head, so the observation runs when the body's session ends
    /// and the in-flight gauge stays raised for the whole of it.
    #[must_use]
    fn defer_to_transfer(
        &mut self,
        session: Arc<Mutex<StreamSession>>,
    ) -> DeferredObservation {
        let (mut exchange, correlation, observability) = self.take();
        exchange.session = Some(session);
        observability.defer(exchange, correlation)
    }

    /// Takes the exchange and the correlation out, disarming the guard.
    fn take(
        &mut self,
    ) -> (
        Exchange,
        Option<CorrelationContext>,
        Arc<crate::data_plane::observability::Observability>,
    ) {
        // The timings travel with the exchange, so a deferred observation of a
        // streamed transfer carries the same four phases a finished one does.
        let mut exchange = self
            .exchange
            .take()
            .expect("the exit is armed for the whole of the path");
        exchange.timings = Some(self.timings);
        (
            exchange,
            self.correlation.take(),
            Arc::clone(&self.observability),
        )
    }
}

impl Drop for PathExit {
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-observe
    // The exit of the path, taken on every return the path can make after the
    // `ProxyResponse` exists or the streamed transfer has ended:
    // `cpt-cf-oagw-algo-metrics-observe` applies the cardinality rules and
    // updates the twelve families from the execution context and the sibling
    // states.
    fn drop(&mut self) {
        let Some(mut exchange) = self.exchange.take() else {
            return;
        };
        exchange.timings = Some(self.timings);
        self.observability.observe(&exchange, self.correlation.as_ref());
    }
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-observe
}

/// The `instance` a proxy problem document names: the request path the
/// operation was issued against, query excluded.
#[must_use]
fn instance_of(_original: &Uri, uri: &Uri) -> String {
    String::from(uri.path())
}

/// Runs one proxy exchange through the Data Plane.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn serve(
    state: &SharedState,
    method: Method,
    alias: String,
    path_suffix: Option<String>,
    query: Option<&str>,
    headers: HeaderMap,
    body: Body,
    security: Option<SecurityContext>,
    peer: Option<std::net::SocketAddr>,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
    instance: String,
) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-issue
    // The actor's request is the one this path is serving, and its answer is
    // either the upstream's or a problem document: nothing about it is
    // decided here, and the flow only observes what the path produces.
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-api
    // The API is the proxy path `cpt-cf-oagw-feature-data-plane-proxy`
    // registers, which is the path this flow is reached from and not a second
    // registration of it.
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-api
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-issue

    // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-receive
    // The request the proxy handler holds arrives here first, before the
    // permission check and the resolution step of
    // `cpt-cf-oagw-flow-proxy-request` run: the three-part detection reads the
    // method and two headers and nothing else, so the answer below is produced
    // whatever the alias names and whether it resolves at all (§1.5).
    let preflight = preflight_of(&method, &headers);
    // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-receive

    // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-preflight-if
    // The three-part detection ADR 0004 states: method `OPTIONS`, an `Origin`
    // header, and an `Access-Control-Request-Method` header. Nothing behind
    // the answer is resolved, read, or charged.
    if let Some(answer) = preflight {
        // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-headers
        // `cpt-cf-oagw-algo-cors-preflight-headers` builds the header set from
        // the request's own three header values and from the constant max age,
        // reading no configuration and resolving no upstream.
        let answer = crate::domain::cors::preflight_answer(
            answer.origin.as_deref(),
            answer.request_method.as_deref(),
            answer.request_headers.as_deref(),
        );
        // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-headers

        // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-return
        // RETURN 204 with that header set and no body: no upstream resolution,
        // no tenant context, no route match, no endpoint selection, no plugin
        // execution, no rate-limit charge, and no permission check, so the
        // answer discloses nothing but the permissiveness ADR 0004 fixes.
        return preflight_response(&answer, &instance);
        // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-return
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-preflight-if

    // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-preflight-else
    // The ELSE of the detection: the request failed the three-part test, so it
    // is an ordinary proxy request and not a CORS preflight.
    // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-else-return
    // RETURN nothing: the proxy flow below resolves, matches, authenticates,
    // validates, and charges it like any other request.
    // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-else-return
    // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cpf-preflight-else

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-issue
    // The actor's request carries the method, the alias, an optional path
    // suffix, an optional query, and any headers, and the answer is the
    // upstream's or a problem document.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-api
    // The route classified the request to the Data Plane by path; the platform
    // middleware has already authenticated the bearer token and resolved the
    // calling tenant and subject into the context this handler reads.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-api
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-issue

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-issue
    // The actor's request that carries a body arriving over time is the same
    // request any proxy exchange carries, and nothing about it is known here
    // until the upstream's answer names the transfer mode.
    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-api
    // The API is the proxy path `cpt-cf-oagw-feature-data-plane-proxy`
    // registers, taken through the permission check, the resolution, the match,
    // the validations, the rate-limit charge, and the composed chain exactly as
    // for a non-streaming request: no streaming-specific relaxation applies
    // anywhere before the send, and the transfer begins only at the response
    // head below.
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-api
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-issue

    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-issue
    // The actor's upgrade request carries the `GET` method, the `Upgrade`
    // header naming `websocket`, the `Connection` header naming the `upgrade`
    // token, the handshake's own request headers, and the same bearer token
    // every proxy request carries.
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-api
    // The API is the same proxy path, taken under those conditions with the
    // permission check, the resolution, the match, the validations, the
    // rate-limit charge, and the composed chain all executed before this flow
    // is reached, so the upgrade request bypasses nothing of it.
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-api
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-issue

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-normalize
    // The same normalization routine the write path uses, so a proxy resolution
    // can never disagree with a stored alias about shape, case, or a trailing
    // dot.
    let normalized = match crate::domain::Alias::parse(&alias) {
        Ok(normalized) => normalized,
        Err(_) => {
            return gateway(
                &DomainError::gateway(
                    ErrorKind::RouteNotFound,
                    "the request addressed no alias the gateway can normalize",
                ),
                &instance,
            );
        }
    };
    let alias = normalized.to_string();
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-normalize

    let mut exit = PathExit::arm(
        Arc::clone(state.observability()),
        &method,
        &alias,
        crate::data_plane::observability::correlation_header(&header_pairs(&headers)),
    );

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-authorize
    // `cpt-cf-oagw-flow-proxy-authorize` runs before any resolution or cache
    // read: the platform resolved the caller and this handler enforces the
    // `:invoke` permission of the proxy API on it.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-context
    // The calling tenant and the subject come from the resolved
    // SecurityContext; a request with neither fails closed rather than
    // proceeding, and answers before any resolution or cache read.
    let Some(context) = security.as_ref() else {
        return exit.gateway(
            &DomainError::gateway(
                ErrorKind::AuthenticationFailed,
                "the request carries no authenticated subject",
            ),
            &instance,
        );
    };
    let tenant = match crate::control_plane::scoping::calling_tenant(context) {
        Ok(tenant) => tenant,
        Err(error) => return exit.gateway(&error, &instance),
    };
    let subject = context.subject_id();
    exit.identify(tenant, &subject.to_string());
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-context

    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-ownership
    // The ownership condition is satisfied by the chain resolution itself: the
    // candidate set the walk produces contains only rows of the calling tenant
    // and its ancestors, so a request that reaches a route at all reached a
    // route of its own chain, and no separate ownership check runs here.
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-ownership

    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-empty-if
    // An alias outside the caller's chain is never a candidate, so the
    // resolution's empty answer is the 404 an unauthorized caller is answered
    // with, and never 403: the answer names no alias that exists outside its
    // chain.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-empty-return
    // RETURN the empty candidate set, which the resolution step maps to the 404
    // the flow answers with.
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-empty-return
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-empty-if

    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-return
    // RETURN the authorized context carrying the tenant, the subject, and the
    // permission verdict, for the resolution step to consume.
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-return

    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-if
    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-permission
    // The platform middleware authenticates; the permission of the proxy API is
    // this handler's to enforce, before any resolution or cache read. An
    // enforcer the state does not hold fails closed, exactly as the management
    // surface does.
    let Some(enforcer) = state.enforcer() else {
        tracing::warn!(instance, "no AuthZ client resolved; the proxy surface fails closed");
        exit.authentication_failure(403);
        return problem::forbidden_response(crate::gts::PROXY_TYPE, &instance);
    };
    if let Err(error) = enforcer
        .access_scope(
            context,
            &proxy_resource_type(),
            super::INVOKE,
            None,
        )
        .await
    {
        tracing::warn!(instance, error = %error, "proxy permission refused");
        exit.authentication_failure(403);
        return problem::forbidden_response(crate::gts::PROXY_TYPE, &instance);
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-permission
    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-else
    // The ELSE of the delegation check: this deployment's middleware enforces
    // no permission of its own, so the handler's enforcement is the only one
    // and its decision is authoritative.
    // @cpt-begin:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-continue
    // CONTINUE with the handler's own verdict, which is the enforcement this
    // deployment has.
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-continue
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-else
    // @cpt-end:cpt-cf-oagw-flow-proxy-authorize:p1:inst-authz-delegate-if
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-authorize

    // The request context the resolution, the validation, and the plugins
    // read: the identity the platform resolved, the alias as normalized, and
    // the request as it arrived.
    let context = ProxyContext {
        method: method.as_str().to_ascii_uppercase(),
        alias: alias.clone(),
        path_suffix,
        query: query.map(String::from),
        headers: header_pairs(&headers),
        target_host: headers
            .get("x-oagw-target-host")
            .and_then(|value| value.to_str().ok())
            .map(String::from),
        tenant_id: tenant,
        subject_id: Some(subject),
        correlation: exit.correlation(),
    };

    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-path
    // The path's own steps run here, restated by reference and decided by
    // their own feature: the alias normalization, the authorization, the
    // resolution, the match, the endpoint selection, the inbound and body
    // validation, the rate-limit check, the composed chain, the header
    // transformation, the forward, and the response classification that
    // produces the `ProxyResponse` the exit of this flow reads.
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-path

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve
    // The candidate set comes from the L1 cache on a hit and from the
    // hierarchical feature's resolution on a miss; the ancestor chain is the
    // platform tenant-resolver's, and its absence is an empty chain, which is
    // the calling tenant alone.
    let ancestors = chain::chain_of(state.resolver(), context_security(security.as_ref()), tenant)
        .await
        .map(|chain| chain.tenants().to_vec())
        .unwrap_or_default();
    let selector = RouteSelector::Http {
        method: context.method.clone(),
        path: context.request_path(),
    };
    let resolution = consume(
        state.store(),
        state.dp_cache(),
        tenant,
        &ancestors,
        &alias,
        &selector,
    );
    exit.resolved();
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-if
    // An empty candidate set, a disabled upstream, and a gRPC upstream are the
    // three outcomes the resolution answers before any route is matched, and
    // none of them builds an outbound request. The failed-closed outcome is the
    // platform 500 problem shape, not a catalogue row.
    let Resolution::Resolved(resolved) = resolution else {
        if let Resolution::Failed = resolution {
            tracing::error!(
                instance,
                "the effective configuration could not be resolved; the request failed closed"
            );
            exit.bare(500);
            return problem::storage_problem_response(&instance);
        }
        let failure = resolution
            .failure_of()
            .unwrap_or_else(|| DomainError::gateway(ErrorKind::RouteNotFound, "no route matched"));
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-return
        // 404 with the `RouteNotFound` variant for an empty candidate set or an
        // unmatched route, and 503 with the `LinkUnavailable` variant for a
        // disabled upstream; no outbound request is built.
        return exit.gateway(&failure, &instance);
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-return
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-if
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-else
    // The resolved configuration carries the candidate set the match selects
    // from.
    exit.upstream_resolved();
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-resolve-else

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match
    // The match selects by method allowlist, longest path prefix, priority, and
    // `path_suffix_mode`, over the candidate set the resolution produced, in
    // the path space the route paths address.
    let outcome = match_route(
        &resolved,
        None,
        &context.method,
        &context.request_path(),
        context.path_suffix.as_deref(),
    );
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-if
    // The answer names the upstream that resolved, never a candidate that did
    // not: a rejected suffix and an unmatched request are the two failures the
    // match produces, answered 404 and 400.
    let Some(matched) = (match &outcome {
        crate::data_plane::MatchOutcome::Matched(matched) => Some(matched),
        _ => None,
    }) else {
        let failure = failure_of(&outcome).unwrap_or_else(|| {
            DomainError::gateway(ErrorKind::RouteNotFound, "no route matched the request")
        });
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-return
        // The answer names the upstream that resolved, never a candidate that
        // did not.
        return exit.gateway(&failure, &instance);
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-return
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-if
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-else
    // The match selected a route, and the selection is what the steps below
    // consume; no branch of its own is taken here.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-match-else

    exit.exchange().route = Some(matched.match_pattern.clone());

    // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-receive
    // The check request arrives from the proxy path after its resolution and
    // route match have produced the effective configuration and the matched
    // route, and before its rate-limit check and its composed chain run: the
    // two layer results the resolution attached are the flow's input, and the
    // origin is the value the platform delivered byte-exact.
    let origin = text_header(&headers, "origin");
    let mut decoration: Option<crate::domain::cors::CorsDecoration> = None;
    // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-receive

    // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin-if
    // A request that carries no `Origin` header is admitted by nothing here:
    // DESIGN §3.2 qualifies both CORS guard rows "actual cross-origin requests
    // only", so the flow is not invoked for it and the forwarded answer carries
    // no CORS header of any kind.
    // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin
    // RETURN the not-cross-origin outcome with no decoration and no CORS
    // header of any kind, which is what `decoration` holds when the branch
    // below never runs.
    // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin
    if let Some(origin) = origin {
    // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin-if
    // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin-else
    // The ELSE of the origin check: the request is cross-origin, so the
    // effective configuration is folded and the request is decided.
    // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-no-origin-else

        // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-fold
        // `cpt-cf-oagw-algo-cors-fold` applies its per-member overlay across
        // the two layer results in the upstream, then route order, and reports
        // the absent family when no layer carries one or the prevailing
        // `enabled` is false.
        let policy = crate::domain::cors::fold(resolved.cors.as_ref(), matched.cors.as_ref());
        // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-fold

        // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-decide
        // `cpt-cf-oagw-algo-cors-decide` evaluates that configuration against
        // the request's `Origin` and its method; an absent family decides
        // nothing and decorates nothing.
        let decision = policy
            .map(|policy| crate::domain::cors::decide(&policy, Some(origin.as_str()), context.method.as_str()));
        // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-decide

        // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow-if
        match decision {
            Some(crate::domain::cors::CorsDecision::Allowed(admitted)) => {
                // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow
                // RETURN the admission with the decoration the decision
                // computed, carried on the response the proxy path assembles
                // below, and let the proxy path forward the request.
                decoration = Some(admitted);
                // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow
            }
            // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow-if
            // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow-else
            // The ELSE of the decision: the request is refused, and the answer
            // is produced before anything is forwarded.
            // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-allow-else
            Some(crate::domain::cors::CorsDecision::Refused(reason)) => {
                // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-refuse
                // RETURN 403 with the problem body the decision names — the
                // origin type for a disallowed origin and the method type for a
                // disallowed method — carrying `Vary: Origin` and
                // `X-OAGW-Error-Source: gateway`, answered before anything is
                // forwarded, so no counter is charged and no plugin runs.
                exit.bare(403_u16);
                return cors_refusal_response(&reason, origin.as_str(), context.method.as_str(), &instance);
                // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-refuse
            }
            None => {}
        }
    }
    // @cpt-begin:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-return
    // RETURN the admission, the decoration, or the 403: the outcome the
    // enforcement produced is recorded in the request's execution context for
    // `cpt-cf-oagw-feature-observability` to report, and this feature
    // registers no sink and emits no metric of its own.
    // @cpt-end:cpt-cf-oagw-flow-cors-enforce:p1:inst-cfe-return

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint
    // The selection reads the routing header the context already carried and
    // never forwards it, and advances the per-upstream round-robin counter when
    // no header named the endpoint.
    let selected = match select_endpoint(&resolved, context.target_host.as_deref(), state.round_robin()) {
        Ok(selected) => {
            exit.endpoint(
                resolved.upstream_id,
                selected.endpoint.host.as_str().to_owned(),
                selected.choice,
            );
            selected
        }
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-if
        Err(failure) => {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-return
            // 400 with the `MissingTargetHost`, `InvalidTargetHost`, or
            // `UnknownTargetHost` variant the selection named; no upstream call
            // is attempted.
            return exit.gateway(&failure, &instance);
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-return
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-if
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-else
    // The endpoint was selected, and the steps below send to it; no branch of
    // its own is taken here.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-endpoint-else

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-inbound
    // The method is the match's own filter, so the checks that remain are the
    // query parameters against the matched route's allowlist and the header
    // values against the injection check.
    let validated = validate_inbound(&context, matched);
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-inbound

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-body
    // The body is read and validated together: the framing headers and the
    // declared size are answered before any byte is buffered, and the read
    // stops at the first byte past the hard limit.
    let body_read = read_body(&context, body).await;
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-body

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-if
    // Either validation answers the request before anything is forwarded, and
    // no buffer of the body outlives the answer.
    let body = match (validated, body_read) {
        (Ok(()), Ok(body)) => {
            exit.exchange().request_size = u64::try_from(body.len()).unwrap_or(u64::MAX);
            body
        }
        (_, Err(failure)) | (Err(failure), _) => {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-return
            return exit.gateway(&failure, &instance);
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-return
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-if
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-else
    // Both validations passed, and the read body is what the outbound request
    // carries.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-validate-else

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit
    // The rate-limit check of `cpt-cf-oagw-feature-rate-limiting` runs here,
    // after the body validation and ahead of the composed chain, keyed on the
    // identity the security context and the inbound connection carry.
    let identity = crate::data_plane::ratelimit::LimitIdentity::new(
        tenant,
        Some(String::from(subject)),
        peer,
    );
    let verdict = crate::data_plane::ratelimit::check(
        state.rate_limits(),
        &resolved,
        matched,
        &identity,
        std::time::Instant::now(),
    )
    .await;
    exit.rate_limit(RateLimitObservation {
        exceeded: matches!(
            verdict,
            crate::data_plane::ratelimit::LimitVerdict::Rejected(_)
        ),
        usage_ratio: None,
    });
    exit.breaker(
        Some(state.rate_limits().lock().breaker(&crate::data_plane::ratelimit::upstream_prefix(
            resolved.upstream_id,
        )).phase),
        Vec::new(),
    );
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit-if
    // The verdict is mapped to the answer the check's own flow produced: 429
    // with the header set for a refusal, 503 for a breaker that is not
    // admitting, and the forward for an admission, a release, and a degraded
    // admission.
    match verdict {
        crate::data_plane::ratelimit::LimitVerdict::Admitted
        | crate::data_plane::ratelimit::LimitVerdict::Degraded => {}
        crate::data_plane::ratelimit::LimitVerdict::Rejected(headers) => {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit-return
            let failure = DomainError::with_retry_after(
                ErrorKind::RateLimitExceeded,
                "the request exceeded the effective rate limit",
                headers.retry_after_seconds,
            );
            let response = exit.gateway(&failure, &instance);
            return with_rate_limit_headers(response, &headers);
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit-return
        }
        crate::data_plane::ratelimit::LimitVerdict::Open {
            retry_after_seconds,
        } => {
            let failure = DomainError::with_retry_after(
                ErrorKind::CircuitBreakerOpen,
                "the resolved upstream is not admitting attempts",
                Some(retry_after_seconds),
            );
            return exit.gateway(&failure, &instance);
        }
    }
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-ratelimit-if

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain
    // The composed chain of the upstream and route layers runs its request leg:
    // auth, then the guards, then the transforms, whose mutations the header
    // transform carries into the outbound map.
    let chain_composed = match crate::plugins::chain::compose(
        state.store(),
        tenant,
        &crate::domain::plugin_contract::NamedPluginRegistry::with_builtins(),
        state.registries(),
        None,
        &state.store().upstream_plugin_rows(tenant, resolved.upstream_id),
        &state.store().route_plugin_rows(tenant, matched.route_id),
    ) {
        Ok(composed) => composed,
        Err(failure) => return exit.gateway(&failure, &instance),
    };
    let mutations = match run_request_phase(&chain_composed, &context, &SANDBOX_LIMITS).await {
        Ok(mutations) => mutations,
        // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-if
        Err(failure) => {
            // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-return
            // The gateway error `cpt-cf-oagw-algo-chain-execute` named, mapped
            // through `cpt-cf-oagw-algo-error-mapping`.
            return exit.gateway(&failure, &instance);
            // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-return
        }
        // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-if
    };
    exit.chained();
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain
    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-else
    // The chain executed and its mutations are what the transform and the
    // forward below carry; no branch of its own is taken here.
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-chain-else

    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect
    // The three-part detection `cpt-cf-oagw-algo-stream-mode-select` applies to
    // the request as the proxy path holds it, before the header transform builds
    // the outbound map and before any strip runs: the method, the `Upgrade`
    // header, and every value the `Connection` header carried, so a token list
    // naming `upgrade` is read as the list it is.
    let connections = context.header_values("connection").join(", ");
    let detection = crate::domain::stream::upgrade_detection(
        &context.method,
        context.header("upgrade"),
        if connections.is_empty() {
            None
        } else {
            Some(connections.as_str())
        },
    );
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-if
    // The negative branch of the detection: no suspension is recorded, all
    // eight hop-by-hop headers are stripped, and no handshake is built, so the
    // exchange stays a plain request/response transfer.
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-return
    // RETURN no suspension and no `UpgradeHandshake`: `detection` is `None`
    // here, so the transform strips all eight hop-by-hop headers and the
    // exchange stays under `cpt-cf-oagw-flow-stream-transfer`. A request that
    // fails any one of the three parts is an ordinary proxy request and not a
    // handshake.
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-return
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-if
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-else
    // The ELSE of the detection: the request failed none of the three parts, so
    // the handshake is built and the send that follows it is the handshake's.
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-detect-else

    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-build
    // The handshake is built from the detection and the request, which records
    // the suspended headers the transform forwards and leaves the answer
    // unjudged until the upstream's own answer arrives.
    let mut handshake =
        detection.map(|detected| crate::domain::stream::UpgradeHandshake::build(detected, &context));
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-build

    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-return
    // RETURN the `UpgradeHandshake`: it carries the suspended two and the
    // forwarded handshake headers out of the build phase to the transform that
    // applies them and to the judgement below, where the answer the send
    // receives decides whether the session it opened reaches `Open`. The
    // routine has no answer of its own to substitute at this point, so the
    // value returned is the handshake as built and nothing else.
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-return

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-forward
    // The outbound map is built from the validated context, the resolved
    // upstream's request rules, and the plugin mutations, and is sent once over
    // the shared client.
    let outbound_headers =
        match transform_request(&context, &resolved.headers, &selected, &mutations, detection) {
            Ok(headers) => headers,
            Err(failure) => return exit.gateway(&failure, &instance),
        };
    let request = crate::domain::proxy::OutboundRequest {
        method: context.method.clone(),
        scheme: selected.endpoint.scheme,
        host: selected.endpoint.host.to_string(),
        port: selected.endpoint.port,
        path: outbound_path(context.query.as_deref(), &matched.outbound_path),
        headers: outbound_headers,
        body,
    };
    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-session
    // The session of a detected upgrade is opened in `Opening` as the send
    // begins, so it exists for as long as the handshake is in flight and
    // carries the `tunnel` mode the detection fixed.
    let opened = detection.is_some().then(|| {
        Arc::new(Mutex::new(StreamSession::open_for_handshake(
            tenant,
            resolved.upstream_id,
        )))
    });
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-session
    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-try
    // The send through `cpt-cf-oagw-algo-outbound-forward`, which applies the
    // dial-time scheme check and bounds the wait for the answer with the
    // `RequestTimeout` deadline. The response header is the boundary that
    // deadline bounds, and the last moment at which the exchange can still be
    // answered as a whole.
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-send
    // The handshake request is sent once and never re-issued, which is
    // `cpt-cf-oagw-principle-no-retry` applied to the one request type that
    // cannot be repeated idempotently: the caller's handshake key is spent and
    // a second send would be a different handshake.
    let attempt = match state.outbound().begin(&request, state.config()).await {
        Ok(mut live) => live.head().await.map(|head| (live, head)),
        Err(failure) => Err(failure),
    };
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-send
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-try
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-forward

    // @cpt-begin:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-return
    // The classification of one outbound attempt is delivered to the breaker
    // machine the check consulted, which is the flow's input; a failed
    // exchange is the classification of the failure the forward produced, and
    // the machine counts only the three rows §1.5 enumerates. The count reads
    // nothing back, so it stays off the request's latency budget.
    let (succeeded, counted) = match &attempt {
        Ok(_) => (true, false),
        Err(failure) => (
            false,
            matches!(
                failure.kind,
                ErrorKind::ConnectionTimeout | ErrorKind::RequestTimeout | ErrorKind::LinkUnavailable
            ),
        ),
    };
    state
        .rate_limits()
        .lock()
        .breaker(&crate::data_plane::ratelimit::upstream_prefix(resolved.upstream_id))
        .count(succeeded, counted, std::time::Instant::now());
    {
        let mut limits = state.rate_limits().lock();
        let breaker = limits.breaker(&crate::data_plane::ratelimit::upstream_prefix(
            resolved.upstream_id,
        ));
        exit.breaker(Some(breaker.phase), breaker.drain_reported());
    }
    // @cpt-end:cpt-cf-oagw-algo-breaker-count:p1:inst-brc-return

    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-catch
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-fail-if
    // A handshake that failed before any data moved is recorded on the
    // handshake, closes the session it opened in `Opening`, and is answered
    // with the failure the forward produced.
    let (live, head) = match attempt {
        Ok(pair) => {
            exit.forwarded();
            pair
        }
        Err(failure) => {
            if let Some(record) = handshake.as_mut() {
                record.failed_before_data();
            }
            if let Some(opened) = &opened {
                // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-catch-handle
                opened.lock().refuse();
                // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-catch-handle
            }
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-fail-return
            run_error_phase_of(&chain_composed, &failure);
            return exit.gateway(&failure, &instance);
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-fail-return
        }
    };
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-fail-if
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-catch

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-headers
    // The response header is what this flow is invoked after, and it is the
    // boundary `proxy_timeout_secs` bounds.
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-headers

    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-if
    // The upstream's answer is judged on the handshake it was sent for: a 101
    // is the handshake taken up, anything else is an answer that passes
    // through unchanged, and the session opened at the send moves with the
    // judgement.
    // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-101-if
    if let Some(record) = handshake.as_mut() {
        record.judge(head.status);
    }
    let tunnelling = opened.as_ref().and_then(|session| {
        let mut guard = session.lock();
        if head.status == 101 {
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-101-open
            if let Ok(open) = guard.lifecycle.opened() {
                guard.lifecycle = open;
            }
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-101-open
            Some(Arc::clone(session))
        } else {
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-not-101-else
            // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-not-101
            // The handshake was not taken up: the session closes without any
            // half being read and no variant of the catalogue is substituted
            // for the answer the upstream itself produced, so the caller
            // receives the upstream's own response rather than a gateway
            // variant of it.
            guard.refuse();
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-not-101
            // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-not-101-else
            None
        }
    });
    // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-101-if
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-if
    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-else
    // The ELSE of the 101 branch: no tunnel is carried and the connection
    // stays a plain request/response exchange.
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-101-else

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-classify
    // A failed exchange is a gateway failure mapped by the foundation's
    // problem mapping; the response leg of the chain runs on the upstream
    // answer, whose headers the `headers.response` rules then transform, and
    // the answer is tagged with its error source before any body byte moves.
    let response_mutations = match crate::data_plane::execute::run_response_phase(
        &chain_composed,
        head.status,
        &head.headers,
        &SANDBOX_LIMITS,
    ) {
        Ok(mutations) => mutations,
        Err(failure) => return exit.gateway(&failure, &instance),
    };
    let mut classified =
        classify_upstream_head(head.status, head.headers.clone(), &resolved.headers, &response_mutations);
    // The status the caller was answered with is the upstream's own numeric
    // code, which is what the `http.response.status_code` label and the
    // record's `status` field carry on a streamed exchange.
    exit.exchange().status = Some(classified.status);
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-classify

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-mode
    // The transfer mode is selected from the request the first half of
    // `cpt-cf-oagw-algo-stream-mode-select` judged and from the response
    // headers the send received, and it is the body transfer that owns
    // whatever the classification hands over.
    let (mode, _carry) = crate::domain::stream::select_mode(
        detection,
        head.status,
        head.header("content-type"),
    );
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-mode

    // @cpt-begin:cpt-cf-oagw-dod-cors-headers:p1:inst-cors-attach
    // The decoration an allowed decision computed rides the response the proxy
    // path assembled: this feature attaches it and assembles, tags, and
    // classifies nothing of the response itself.
    if let Some(admitted) = &decoration {
        classified.headers.extend(admitted.headers());
    }
    // @cpt-end:cpt-cf-oagw-dod-cors-headers:p1:inst-cors-attach

    // @cpt-begin:cpt-cf-oagw-flow-proxy-request:p1:inst-px-return
    // The record of the plugin use is issued after the response is produced and
    // reads nothing back, so it is off the request's latency budget.
    crate::data_plane::execute::record_last_used(
        state.store(),
        &chain_composed,
        crate::store::unix_now(),
    );
    // @cpt-end:cpt-cf-oagw-flow-proxy-request:p1:inst-px-return

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-if
    if mode == TransferMode::Tunnel {
        // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-return
        // Nothing is returned for this flow to transfer: the exchange is
        // `cpt-cf-oagw-flow-upgrade-proxy`'s and its two halves are already
        // held there.
        // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-return
        // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-tunnel
        // The 101 is answered with the headers the transform left, and the two
        // halves become a byte tunnel in a task of its own, because the
        // response this handler returns ends the handler's part in it and the
        // tunnel outlives it.
        let Some(handle) = upgrade else {
            // The connection the caller arrived on carries no upgrade to
            // fulfil, so no tunnel is carried over it and the session the send
            // opened closes with it. The answer is the exchange's end, and the
            // armed guard observes it as the path returns.
            if let Some(session) = &tunnelling {
                session.lock().refuse();
            }
            return passthrough(&classified, Body::empty(), &instance);
        };
        let session = tunnelling.expect("the tunnel is carried by the session the send opened");
        // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-observe
        // The tunnel outlives the response head, so the observation is taken
        // out of the guard and travels into the task the tunnel runs in: the
        // in-flight gauge stays raised for the whole of the transfer and the
        // record carries the byte counts as transferred.
        let deferred = exit.defer_to_transfer(Arc::clone(&session));
        tokio::spawn(async move {
            match handle.await {
                Ok(caller) => crate::data_plane::stream::tunnel(live, session, caller).await,
                Err(_upgrade_failed) => {}
            }
            drop(deferred);
        });
        // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-observe
        return passthrough(&classified, Body::empty(), &instance);
        // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-tunnel
        // @cpt-begin:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-return
        // RETURN the outcome the tunnel records in the session the send opened,
        // which is the record the request's execution context carries, for
        // `cpt-cf-oagw-feature-observability` to report; this flow emits no log
        // line, no metric, and no span of its own. The head below is the answer
        // the handshake was judged with and the body it carries is empty,
        // because the two halves are the tunnel's and no body is transferred
        // past the 101.
        // @cpt-end:cpt-cf-oagw-flow-upgrade-proxy:p1:inst-up-return
    }
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-if
    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-else
    // The ELSE of the tunnel branch: the body is transferred as it arrives.
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-tunnel-else

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-session
    // The session is opened in the `Open` state, carrying the caller's half,
    // the upstream half the forward opened, the `incremental` mode the
    // selection returned, the response `Content-Type` it attached, the idle
    // deadline in force, and no outcome.
    let session = Arc::new(Mutex::new(StreamSession::open_for_incremental(
        tenant,
        resolved.upstream_id,
        crate::data_plane::classify::content_type_of(&classified).map(String::from),
    )));
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-session

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-pump
    // The pump transfers the body one chunk at a time, and the first chunk is
    // awaited here so a stall and an abort can still be answered as a whole.
    let body = match crate::data_plane::stream::incremental(live, Arc::clone(&session)).await {
        Ok(body) => body,
        Err(failure) => return exit.gateway(&failure, &instance),
    };
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-pump

    // @cpt-begin:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-return
    // The body is answered with its head, and the outcome of the transfer is
    // recorded on the session the pump holds, which is the record the request's
    // execution context carries; this flow emits no log line, no metric, and no
    // span of its own. The observation is taken out of the guard and rides the
    // body, so it runs when the transfer ends and not when the head is
    // answered.
    let deferred = exit.defer_to_transfer(Arc::clone(&session));
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-return
    // RETURN the answer unchanged: this flow mutates no header, no status, and
    // no body of any response except the `trace_id` extension field a gateway
    // error body carries, and it adds no latency to the measured path beyond
    // the reading of values the path already computed.
    passthrough(
        &classified,
        Body::from_stream(ObservedBody {
            inner: body,
            deferred: Some(deferred),
        }),
        &instance,
    )
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-return
    // @cpt-end:cpt-cf-oagw-flow-stream-transfer:p1:inst-st-return
}

/// The body of an incremental transfer, which carries the observation the
/// exchange's exit performs and lets it run when the transfer ends.
///
/// The transport drops the body when the transfer ends or when the caller stops
/// accepting it, and the drop is what runs the deferred observation the guard
/// handed over.
struct ObservedBody {
    inner: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, String>>,
    /// The exchange's deferred exit, read by the destructor the transport
    /// runs when the transfer ends.
    #[allow(dead_code)]
    deferred: Option<DeferredObservation>,
}

impl Drop for ObservedBody {
    // The body's own drop is the moment the transfer ended, and the field it
    // holds is the observation that ends the exchange: the destructor runs it
    // before the pump's half is closed under it.
    fn drop(&mut self) {}
}

impl futures_util::Stream for ObservedBody {
    type Item = Result<bytes::Bytes, String>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

/// The preflight the three-part detection produced: the method and two header
/// values the answer echoes, each absent when the platform delivered a value
/// that cannot be formed into a response header.
struct Preflight {
    origin: Option<String>,
    request_method: Option<String>,
    request_headers: Option<String>,
}

/// Detects a CORS preflight from the method and two headers alone.
///
/// The detection is presence-based, which is the three-part test ADR 0004
/// states: a value the platform delivered that cannot be formed into a
/// response header leaves the request a preflight and only the header that
/// would echo it is omitted (§1.5 of the FEATURE).
#[must_use]
fn preflight_of(method: &Method, headers: &HeaderMap) -> Option<Preflight> {
    if method != Method::OPTIONS {
        return None;
    }
    if !headers.contains_key("origin") || !headers.contains_key("access-control-request-method") {
        return None;
    }
    Some(Preflight {
        origin: text_header(headers, "origin"),
        request_method: text_header(headers, "access-control-request-method"),
        request_headers: text_header(headers, "access-control-request-headers"),
    })
}

/// One header value as the platform delivered it, byte-exact.
///
/// A value the platform cannot form into a response header is reported absent
/// rather than guessed at, which is the refuse-rather-than-guess rule §1.4
/// states for a value that cannot be compared byte-exactly.
#[must_use]
fn text_header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(String::from)
}

/// The answer a preflight is mapped to: the 204 status, the header set, and no
/// body.
///
/// A member the platform cannot form into a response header is omitted and the
/// omission is recorded in the request's execution context, which is the
/// record the correlation identifier and the trace surfaces read.
fn preflight_response(answer: &crate::domain::cors::PreflightAnswer, instance: &str) -> Response {
    let status = axum::http::StatusCode::from_u16(answer.status)
        .unwrap_or(axum::http::StatusCode::NO_CONTENT);
    let mut response = Response::builder()
        .status(status)
        .header(
            crate::api::rest::problem::ERROR_SOURCE_HEADER,
            axum::http::HeaderValue::from_static("gateway"),
        )
        .body(Body::empty())
        .unwrap_or_else(|_| Response::new(Body::empty()));
    for (name, value) in &answer.headers {
        match (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                response.headers_mut().insert(name, value);
            }
            _ => tracing::info!(
                instance,
                header = %name,
                "a preflight echo the platform cannot form is omitted"
            ),
        }
    }
    response
}

/// The answer a CORS refusal is mapped to: the bare 403 problem document ADR
/// 0004 spells, with `Vary: Origin` and the gateway error-source tag, answered
/// before anything is forwarded.
fn cors_refusal_response(
    refusal: &crate::domain::cors::CorsRefusal,
    origin: &str,
    method: &str,
    instance: &str,
) -> Response {
    let detail = crate::domain::cors::refusal_detail(*refusal, origin, method);
    let mut response = problem::bare_forbidden_response(
        refusal.gts_type(),
        refusal.title(),
        &detail,
        instance,
    );
    if let (Ok(name), Ok(value)) = (
        axum::http::HeaderName::try_from("vary"),
        axum::http::HeaderValue::from_str(crate::domain::cors::VARY_ORIGIN),
    ) {
        response.headers_mut().insert(name, value);
    }
    tracing::info!(
        instance,
        reason = refusal.title(),
        "cross-origin request refused before forwarding"
    );
    response
}

/// Runs the error leg of the chain on the failure the exchange produced.
fn run_error_phase_of(
    chain: &crate::plugins::chain::ComposedChain,
    failure: &DomainError,
) {
    let mut failed = failure.clone();
    crate::data_plane::execute::run_error_phase(chain, &mut failed);
}

/// The outbound path the request is dialed with: the matched route's path with
/// the suffix appended, and the query the inbound validation admitted, which
/// passed the same allowlist the route declares.
fn outbound_path(query: Option<&str>, matched_path: &str) -> String {
    match query {
        Some(query) if !query.is_empty() => format!("{matched_path}?{query}"),
        _ => String::from(matched_path),
    }
}

/// The header pairs the HTTP layer held, in arrival order.
fn header_pairs(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// The enforcer's descriptor of the proxy resource.
#[must_use]
fn proxy_resource_type() -> authz_resolver_sdk::pep::ResourceType {
    authz_resolver_sdk::pep::ResourceType::from_static(
        crate::gts::PROXY_TYPE,
        super::SUPPORTED_PROPERTIES,
    )
}

/// The security context the chain walk reads, borrowed for the call.
fn context_security(context: Option<&SecurityContext>) -> &SecurityContext {
    // The authorize step answered before this point, so the context is present.
    context.expect("the authorized request carries its security context")
}

/// The answer a gateway failure is mapped to.
fn gateway(failure: &DomainError, instance: &str) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-if
    // Every failure this helper receives was produced by the gateway — a
    // validation, authorization, resolution, selection, chain, deadline, or
    // scheme failure above — so the classification sends it to the
    // foundation's problem mapping.
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-if
    tracing::info!(
        instance,
        kind = failure.kind.title(),
        detail = %failure.detail,
        "proxy request refused by the gateway"
    );
    problem::problem_response(failure, instance)
}

/// Appends the rate-limit header set of a 429 answer to a built response.
fn with_rate_limit_headers(
    mut response: Response,
    headers: &crate::data_plane::ratelimit::RateLimitHeaders,
) -> Response {
    for (name, value) in headers.pairs() {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// The answer an upstream-sourced classification is mapped to: the status and
/// the headers the transform left, with the body the pump carries under them.
fn passthrough(
    classified: &crate::domain::proxy::ProxyResponse,
    body: Body,
    _instance: &str,
) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-gateway-else
    // The path did not answer the request from a gateway error: the upstream's
    // own answer passes through under the error-source classification.
    // @cpt-begin:cpt-cf-oagw-flow-request-observed:p1:inst-ro-no-echo
    // The body passes through unmodified and carries no `trace_id`, and no
    // echo is synthesized for it.
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-no-echo
    // @cpt-end:cpt-cf-oagw-flow-request-observed:p1:inst-ro-gateway-else
    let content_type = crate::data_plane::classify::content_type_of(classified);
    let mut response = problem::passthrough_response(classified.status, body, content_type);
    for (name, value) in &classified.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            axum::http::HeaderValue::from_str(value),
        ) {
            response.headers_mut().append(name, value);
        }
    }
    response
}
