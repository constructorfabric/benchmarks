//! The OAGW data plane: turn a proxy request into an upstream request.
//!
//! [`DataPlaneService::proxy`] is the whole request path. It is deliberately
//! boring and strictly ordered, because every step either rejects the request
//! with a problem document or changes exactly one thing about the bytes the
//! upstream finally receives:
//!
//! 1. **method guard** — only the five proxied methods reach the data plane
//!    (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`); anything else is a 400. A CORS
//!    preflight (`OPTIONS` plus the two request headers) is answered *before*
//!    this step and before anything is resolved, because ADR-0004 makes the
//!    preflight answer independent of the policy of the target (see
//!    [`crate::domain::cors`]).
//! 2. **target resolution** — [`resolve_proxy_target`] (alias, route, endpoint
//!    selection, SSRF and scheme policy). This runs *before* the body is
//!    buffered: every policy the next steps enforce is configured on the
//!    resolved resource, so a 404 must not cost a body the client may still be
//!    streaming, and a plugin chain needs the upstream and route documents to
//!    resolve from.
//! 3. **S4 pre-checks** — the ordered seam, each step free to reject the request
//!    with a gateway problem document (ADR-0002, ADR-0003, ADR-0004):
//!    CORS origin and method enforcement on the actual request, then the rate
//!    limits of the upstream and the route, then the plugin chain in its
//!    documented order (auth, then guards, then request transforms). The body is
//!    buffered after the seam, so no plugin ever sees a buffered body and no
//!    bytes are read for a request that is refused here.
//! 4. **body validation** — the inbound body is buffered whole (DESIGN "Body
//!    Validation Rules"): a non-numeric `Content-Length` is a 400, a
//!    `Content-Length` that disagrees with the buffered byte count is a 400, a
//!    `Transfer-Encoding` that is not exactly `chunked` is a 400 and a body
//!    that outgrows `gears.oagw.config.body_limit_bytes` is a 413 raised as
//!    soon as the limit is crossed — never after the whole body was read.
//! 5. **request transformation** — hop-by-hop headers, `X-OAGW-Target-Host` and
//!    `Host` never reach the upstream; `Host` becomes the endpoint authority;
//!    `upstream.headers.request` is applied (`set`, then `add`, then `remove`)
//!    after the passthrough mode decided which inbound headers survive — and
//!    after the plugins, whose header changes are the inbound set this step
//!    starts from.
//! 6. **the call** — one request to the resolved endpoint through the shared
//!    hyper client, under the `proxy_timeout_secs` deadline. Every error the
//!    chain produced or that the call raised passes through the chain's
//!    `transform_error` phase before it leaves the gateway (DESIGN "Plugin
//!    System"), so a plugin may annotate the problem document. Transport
//!    failures are mapped: refused, unresolved or unreachable is a 503
//!    `link.unavailable.v1`, a deadline overrun is a 504
//!    `timeout.request.v1` (a timeout inside the connector is a 504
//!    `timeout.connection.v1`), a response that cannot be parsed is a 502
//!    `protocol.error.v1` and a connection dropped mid-response is a 502
//!    `downstream.error.v1`.
//! 7. **response transformation** — `upstream.headers.response` is applied and
//!    the hop-by-hop response headers are stripped; the body is passed through
//!    **streaming**, so the gateway never buffers an upstream response. Every
//!    upstream response — 4xx and 5xx included — is stamped
//!    `X-OAGW-Error-Source: upstream` (ADR-0007) and forwarded unchanged. On the
//!    way out the response phases of the plugin chain run (guards, then
//!    response/error transforms) and an admitted CORS request is answered with
//!    its `Access-Control-*` advertisement.
//! 8. **streaming and upgrades** (S3) — the streaming body of step 7 is what
//!    carries server-sent events (PRD §5.4,
//!    `cpt-cf-oagw-fr-streaming`, and §8 `cpt-cf-oagw-usecase-sse-streaming`):
//!    each event is forwarded as the upstream writes it, and the
//!    `proxy_timeout_secs` deadline brackets only the upstream *handshake*, so a
//!    stream may outlive it. A `GET` that asks for a WebSocket upgrade is not
//!    proxied but **tunneled**: the handshake headers (`connection: Upgrade`, the
//!    `upgrade` token and every `sec-websocket-*` header) are re-applied on top
//!    of the normal transformation — the smallest deviation from the header rules
//!    an upgrade needs, since RFC 7230 §6.7 makes `upgrade` and `connection`
//!    hop-by-hop and the strip would otherwise eat the handshake — the upstream's
//!    `101` is returned to the client with its headers verbatim, and a spawned
//!    task splices both upgraded connections with `copy_bidirectional`. A reply
//!    to an upgrade request that is *not* a `101` is an ordinary response and
//!    follows step 7 unchanged: the upstream refused the handshake, so the client
//!    sees its status, headers and body. WebTransport (PRD §5.4) runs over HTTP/3
//!    and is not supported: no dependency of this workspace speaks HTTP/3. When
//!    the client disconnects, hyper cancels the response task and drops the
//!    upstream body, which closes the upstream connection (PRD §8: "Client
//!    disconnects: System closes upstream connection").
//!
//!    One consequence of that lifetime is worth stating plainly: a tunnel is
//!    spawned onto the runtime and is not tracked by the gateway, so the
//!    request-drain of the host's graceful shutdown does not see it — an open
//!    tunnel is cut when the process stops, with no drain window of its own. The
//!    gear exposes no shutdown hook that could wait for tunnels (the rest
//!    capability registers no `RunnableCapability`), and inventing one would add
//!    wiring the design does not ask for; the limitation is recorded here rather
//!    than hidden in a task handle nobody retains.
//!
//! There is deliberately **no DNS resolution** on this path: the endpoint host
//! is handed to the connector as-is, so a hostname is resolved (and pinned) by
//! the connector itself and the SSRF policy only ever sees literal IP
//! addresses.
//!
//! The rate limiter and the plugin registries are owned by the data plane
//! (ADR-0003: "the data plane holds the counters"; ADR-0002: plugins register
//! with it), built once in [`DataPlaneService::new`]. The registries are the
//! built-in ones with **no credential source**: an auth plugin that names a
//! `cred://` reference reports the reference as unresolvable (401) rather than
//! guessing at a value — see [`crate::domain::plugins::CredentialSource`] for
//! what a deployment wires to make credential resolution work.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::Response;
use futures_util::StreamExt;
use hyper::upgrade::OnUpgrade;
use hyper_util::client::legacy::{Client, Error as ClientError, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo};
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::control_plane::ControlPlane;
use crate::domain::cors;
use crate::domain::cors::{is_preflight, preflight_response};
use crate::domain::model::{
    Endpoint, EndpointScheme, HeaderPassthrough, PathSuffixMode, ROUTE_METHODS, RequestHeaders,
    ResponseHeaders, normalize_host,
};
use crate::domain::plugins::{
    AuthPluginConfig, ErrorContext, PluginPipeline, PluginRegistries, RequestContext,
    ResponseContext,
};
use crate::domain::rate_limit::{RateLimitRule, RateLimitSubject, RateLimiter};
use crate::domain::resolution::{DialPolicy, ResolvedTarget, resolve_proxy_target};
use crate::error::{ERROR_SOURCE_HEADER, OagwError, UPSTREAM_ERROR_SOURCE, with_error_source};
use toolkit_security::SecurityContext;

/// The request header the `request_id.v1` transform works on (DESIGN
/// "Built-in Plugins").
const REQUEST_ID_HEADER: &str = "x-request-id";

/// The inbound header that selects an endpoint (ADR-0001 Appendix A). It is
/// consumed by the resolution and never forwarded.
const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The hop-by-hop headers of DESIGN "Headers Transformation": they describe a
/// single connection and are stripped from both legs.
const HOP_BY_HOP: [HeaderName; 8] = [
    header::CONNECTION,
    KEEP_ALIVE,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// `keep-alive` has no constant in the `http` crate, so it is minted once here.
const KEEP_ALIVE: HeaderName = HeaderName::from_static("keep-alive");

/// The prefix of every header of the WebSocket handshake (RFC 6455 §4.1).
const SEC_WEBSOCKET_PREFIX: &str = "sec-websocket-";

/// The buffered inbound body plus what its headers declared about it.
struct InboundBody {
    bytes: Bytes,
    /// `true` when the request declared a body (`Content-Length` or a chunked
    /// `Transfer-Encoding`), even for a zero-length body.
    declared: bool,
}

/// The data plane: one shared upstream client plus the control plane and the
/// configuration the resolution and the transformations read from.
pub struct DataPlaneService {
    plane: Arc<ControlPlane>,
    config: OagwConfig,
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>,
    /// Rate limiter of the proxy path (ADR-0003): in-memory counters, owned by
    /// the data plane and shared by every request it serves.
    rate_limiter: RateLimiter,
    /// Registries the plugin chains are resolved from (ADR-0002): the built-in
    /// plugins, without a credential source (see
    /// [`crate::domain::plugins::CredentialSource`]).
    plugins: PluginRegistries,
}

impl DataPlaneService {
    /// Build the data plane with one shared upstream client (webpki roots,
    /// HTTP/1.1 and plaintext `http` — the latter still gated by
    /// [`OagwConfig::allows_http_upstream`]).
    #[must_use]
    pub fn new(plane: Arc<ControlPlane>, config: OagwConfig) -> Self {
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(https);
        // The gear-level configuration reaches the plugins through the
        // registries (ADR-0008 "Gear-Level Configuration"): the token cache
        // sizing, and the same outbound dial policy the resolution applies to
        // an upstream endpoint.
        let plugins = PluginRegistries::with_builtins_for(&AuthPluginConfig {
            credentials: None,
            token_cache: config.token_cache,
            dial_policy: DialPolicy {
                allow_http: config.allows_http_upstream(),
                ssrf_enabled: config.ssrf_policy.enabled,
            },
        });
        Self {
            plane,
            config,
            client,
            rate_limiter: RateLimiter::new(),
            plugins,
        }
    }

    /// Forward a proxy request to the upstream its `alias` resolves to.
    ///
    /// `path_suffix` is the part of the proxy URL that follows
    /// `/oagw/v1/proxy/{alias}` **as received**: `/`-prefixed, percent-encoding
    /// preserved, empty when the proxy URL carried no suffix at all.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`OagwError`] for every rejection on the path: 400 for
    /// a rejected method, an invalid `X-OAGW-Target-Host`, a body rule
    /// violation or an SSRF-blocked endpoint, 401 for a credential a plugin
    /// could not produce, 403 for a CORS rule the request violates, 404 for an
    /// unknown alias or a missing route, 413 for an oversized body, 429 for an
    /// exhausted rate limit, 503 for an unresolvable plugin reference and
    /// 502/503/504 for a transport failure.
    pub async fn proxy(
        &self,
        mut request: Request<Body>,
        tenant_id: Uuid,
        alias: &str,
        path_suffix: &str,
    ) -> Result<Response, OagwError> {
        // CORS preflight (ADR-0004): answered before anything is resolved, with
        // an answer that echoes the request and defers every decision.
        if is_preflight(request.method(), request.headers()) {
            return Ok(preflight_response(request.headers()));
        }

        let method = request.method().clone();
        ensure_supported_method(&method)?;

        // The downstream half of a possible upgrade has to be taken *before* the
        // request is consumed: `hyper::upgrade::on` removes the handle from the
        // request's extensions, and a handler that never takes it leaves hyper
        // holding an upgrade nobody waits for, so the client connection never
        // completes. For a request that asks for no upgrade the handle is inert.
        let downstream = hyper::upgrade::on(&mut request);
        // RFC 6455 §4.1 only defines the handshake for `GET`; any other method
        // carrying upgrade tokens is an ordinary request for the method it names,
        // never a tunnel (the module documentation states this as the tunnel's
        // precondition).
        let upgrade = method == Method::GET && is_websocket_upgrade(request.headers());

        // S4 seam, part 1 — resolution. Every policy the seam enforces is
        // configured on the resource the request resolves to, so the target is
        // resolved before the body is buffered: a 404 or an SSRF rejection must
        // not cost a request the client may still be streaming, and a plugin
        // chain needs the upstream and route documents to resolve from.
        let target = resolve_proxy_target(
            &self.plane,
            &self.config,
            tenant_id,
            alias,
            method.as_str(),
            path_suffix,
            target_host_of(request.headers()).as_deref(),
        )?;

        // S4 seam, part 2 — the ordered pre-checks, each free to reject the
        // request with a problem document before the body is buffered.
        let origin = Self::enforce_cors(&target, &method, request.headers())?;
        self.enforce_rate_limit(&target, request.extensions())?;
        let pipeline = PluginPipeline::resolve(
            &self.plugins,
            target.upstream.spec.plugins.as_ref(),
            target.route.spec.plugins.as_ref(),
            target.upstream.spec.auth.as_ref(),
        )?;
        // The header names the request carried *before* the chain: whatever it
        // carries after that is either one of them or a header a plugin wrote,
        // and a plugin-written header must reach the upstream even under a
        // `passthrough` mode that would drop it — an `Authorization` header an
        // auth plugin injected is the whole point of the plugin.
        let plugin_free_names: Vec<HeaderName> = request.headers().keys().cloned().collect();
        let request_id = self
            .run_request_plugins(&mut request, &target, &pipeline, tenant_id)
            .await?;
        let plugin_added: Vec<HeaderName> = request
            .headers()
            .keys()
            .filter(|name| !plugin_free_names.contains(name))
            .cloned()
            .collect();

        let (parts, body) = request.into_parts();
        let inbound = read_inbound_body(&parts.headers, body, self.config.body_limit_bytes).await?;

        let mut outbound = build_outbound_request(
            &parts.headers,
            parts.uri.query(),
            &target,
            &method,
            path_suffix,
            inbound,
            &plugin_added,
        )?;
        if upgrade {
            // The handshake headers are applied last: they describe the tunnel the
            // two endpoints are agreeing on, so neither the passthrough mode nor a
            // route rule may leave the upstream without them.
            apply_upgrade_headers(outbound.headers_mut(), &parts.headers);
            // The tunnel is not a response the gateway transforms: its headers are
            // the handshake two endpoints negotiated (RFC 7230 §6.7), so the
            // response phases of the plugin chain and the CORS advertisement do
            // not run for it.
            return self.tunnel(outbound, &target, downstream).await;
        }
        let response = match self.call_upstream(outbound, &target).await {
            Ok(response) => response,
            Err(error) => return Err(self.transform_error(&pipeline, error).await),
        };
        self.finish(response, &target, &pipeline, origin, request_id)
            .await
    }

    /// Hand a gateway error to the chain's `transform_error` phase (DESIGN
    /// "Plugin System": `Transform(on_response/on_error)`), so a plugin may
    /// annotate the problem document the client is about to see.
    ///
    /// A failing error transform never masks the rejection it annotates: the
    /// client always sees the original problem document.
    async fn transform_error(&self, pipeline: &PluginPipeline, error: OagwError) -> OagwError {
        let mut ctx = ErrorContext {
            error,
            config: None,
        };
        drop(pipeline.transform_error(&mut ctx).await);
        ctx.error
    }

    /// Enforce the CORS rules of the resolved resource on an actual request.
    ///
    /// # Errors
    ///
    /// Returns the 403 problem document of the first violated rule.
    fn enforce_cors(
        target: &ResolvedTarget,
        method: &Method,
        headers: &HeaderMap,
    ) -> Result<Option<HeaderValue>, OagwError> {
        // CORS is an upstream-level document: `route.v1` carries no `cors`
        // member, so the policy of the upstream governs every route it exposes.
        match target.upstream.spec.cors.as_ref() {
            Some(config) => cors::enforce(config, method, headers),
            None => Ok(None),
        }
    }

    /// Charge the request against the rate limits of its upstream and its route
    /// (ADR-0003): both must allow, the tighter of the two deciding.
    ///
    /// # Errors
    ///
    /// Returns the 429 problem document of the first limit the request exhausts.
    fn enforce_rate_limit(
        &self,
        target: &ResolvedTarget,
        extensions: &http::Extensions,
    ) -> Result<(), OagwError> {
        let mut rules = Vec::new();
        if let Some(config) = target.upstream.spec.rate_limit.as_ref() {
            rules.push(RateLimitRule::upstream(target.upstream.id, config));
        }
        if let Some(config) = target.route.spec.rate_limit.as_ref() {
            rules.push(RateLimitRule::route(target.route.id, config));
        }
        if rules.is_empty() {
            return Ok(());
        }
        let subject = RateLimitSubject {
            tenant_id: target.upstream.tenant_id,
            subject_id: extensions
                .get::<SecurityContext>()
                .map(SecurityContext::subject_id),
            // The client address is a property of the listener the host owns
            // (`ConnectInfo`), which the gear handler never sees. ADR-0003
            // lists `ip` among the scopes but says nothing about a peer the
            // gateway cannot see, so the degradation to the tenant counter is
            // this slice's own decision: a keyless `ip` counter would be shared
            // by every anonymous caller of the tenant and would let one of them
            // exhaust the budget of all the others.
            peer: None,
            route_id: target.route.id,
        };
        self.rate_limiter.enforce(&rules, &subject)
    }

    /// Run the request phases of the plugin chain (ADR-0002: auth, then guards,
    /// then transforms) and return the request id the response has to carry.
    ///
    /// A rejection of one of the three phases passes through the chain's
    /// `transform_error` phase before it leaves the data plane, so a plugin may
    /// annotate the problem document its own chain produced. The seam rejections
    /// of CORS, of the rate limiter and of an unresolvable plugin reference are
    /// raised before a chain is bound to the request and are not seen by it.
    ///
    /// # Errors
    ///
    /// Returns the rejection of the first plugin that refuses the request.
    async fn run_request_plugins(
        &self,
        request: &mut Request<Body>,
        target: &ResolvedTarget,
        pipeline: &PluginPipeline,
        tenant_id: Uuid,
    ) -> Result<Option<String>, OagwError> {
        if pipeline.is_empty() {
            return Ok(None);
        }
        let mut ctx = RequestContext {
            headers: request.headers().clone(),
            method: request.method().clone(),
            tenant_id,
            subject_id: request
                .extensions()
                .get::<SecurityContext>()
                .map(SecurityContext::subject_id),
            upstream_id: target.upstream.id,
            route_id: target.route.id,
            security: request.extensions().get::<SecurityContext>().cloned(),
            config: None,
        };
        // The `?` of a chain phase is the `transform_error` phase's cue: the
        // error leaves the data plane only after the transforms have seen it.
        let chain = async {
            pipeline.authenticate(&mut ctx).await?;
            pipeline.guard_request(&mut ctx).await?;
            pipeline.transform_request(&mut ctx).await?;
            Ok(())
        }
        .await;
        if let Err(error) = chain {
            return Err(self.transform_error(pipeline, error).await);
        }
        // The headers a plugin wrote are the headers the request carries from
        // here on, including the body rules that read `Content-Length`.
        *request.headers_mut() = ctx.headers;
        Ok(request
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    /// Run the response phases of the plugin chain and advertise CORS on the
    /// response the client gets.
    ///
    /// # Errors
    ///
    /// Returns the rejection of the first guard that refuses the response.
    async fn finish(
        &self,
        response: Response,
        target: &ResolvedTarget,
        pipeline: &PluginPipeline,
        origin: Option<HeaderValue>,
        request_id: Option<String>,
    ) -> Result<Response, OagwError> {
        let (mut parts, body) = response.into_parts();
        if !pipeline.is_empty() {
            let mut ctx = ResponseContext {
                status: parts.status,
                headers: parts.headers.clone(),
                request_id,
                config: None,
            };
            let response_phases = async {
                pipeline.guard_response(&mut ctx).await?;
                pipeline.transform_response(&mut ctx).await?;
                Ok(())
            }
            .await;
            if let Err(error) = response_phases {
                return Err(self.transform_error(pipeline, error).await);
            }
            parts.headers = ctx.headers;
        }
        if let (Some(origin), Some(config)) = (origin, target.upstream.spec.cors.as_ref()) {
            cors::apply_response_headers(config, &origin, &mut parts.headers);
        }
        Ok(Response::from_parts(parts, body))
    }

    /// Send the transformed request and transform the response.
    ///
    /// # Errors
    ///
    /// Returns the transport failure mapped by [`transport_error`].
    async fn call_upstream(
        &self,
        outbound: Request<Body>,
        target: &ResolvedTarget,
    ) -> Result<Response, OagwError> {
        let response = self.send_upstream(outbound).await?;
        Ok(stamp_upstream(map_upstream_response(response, target)))
    }

    /// One request to the resolved endpoint under the `proxy_timeout_secs`
    /// deadline (step 6 of the [module documentation](self)).
    ///
    /// # Errors
    ///
    /// Returns a 504 `timeout.request.v1` for a deadline overrun and the failure
    /// mapped by [`transport_error`] for every other transport failure.
    async fn send_upstream(
        &self,
        outbound: Request<Body>,
    ) -> Result<axum::http::Response<hyper::body::Incoming>, OagwError> {
        let deadline = self.config.proxy_timeout();
        tokio::time::timeout(deadline, self.client.request(outbound))
            .await
            .map_err(|_| upstream_timeout(deadline))?
            .map_err(|error| transport_error(&error))
    }

    /// Tunnel an upgrade request through to the upstream (step 8 of the [module
    /// documentation](self)).
    ///
    /// The handshake is sent under the same deadline as any other request. Once
    /// the upstream answers, the deadline no longer applies: a `101` hands the
    /// connection to the spawned splice task, which lives as long as both
    /// endpoints keep it.
    ///
    /// # Errors
    ///
    /// Returns the transport failure mapped by [`transport_error`] while the
    /// handshake is in flight. A reply that is not a `101` is **not** an error: it
    /// follows the ordinary response transformation of step 7, so a refused
    /// upgrade reaches the client with the upstream's status, headers and body,
    /// and no tunnel is opened.
    async fn tunnel(
        &self,
        outbound: Request<Body>,
        target: &ResolvedTarget,
        downstream: OnUpgrade,
    ) -> Result<Response, OagwError> {
        let mut response = self.send_upstream(outbound).await?;
        if response.status() != StatusCode::SWITCHING_PROTOCOLS {
            return Ok(stamp_upstream(map_upstream_response(response, target)));
        }
        // hyper's h1 server writes a `101` with the headers the handler supplies,
        // verbatim, and adds none of its own: the upstream's `upgrade`,
        // `connection` and `sec-websocket-*` headers have to reach the client as
        // they are, so the hop-by-hop strip of `map_upstream_response` must not
        // run here (RFC 7230 §6.7).
        let upstream = hyper::upgrade::on(&mut response);
        let (parts, _) = response.into_parts();
        let mut tunnel = Response::new(Body::empty());
        *tunnel.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
        *tunnel.headers_mut() = parts.headers;
        let tunnel = stamp_upstream(tunnel);
        tokio::spawn(splice(downstream, upstream));
        Ok(tunnel)
    }
}

/// 504 problem for an upstream request that did not complete in time.
///
/// The `proxy_timeout_secs` deadline brackets the whole upstream call, so its
/// overrun is a *request* timeout, not a connection timeout: a timeout inside
/// the connector is reported by [`transport_error`] as
/// [`OagwError::connection_timeout`].
fn upstream_timeout(deadline: Duration) -> OagwError {
    OagwError::request_timeout(format!(
        "the upstream did not respond within the configured deadline of {} s",
        deadline.as_secs()
    ))
}

/// 400 problem for a method the data plane does not proxy.
fn ensure_supported_method(method: &Method) -> Result<(), OagwError> {
    if ROUTE_METHODS
        .iter()
        .any(|name| method.as_str().eq_ignore_ascii_case(name))
    {
        return Ok(());
    }
    Err(OagwError::validation(format!(
        "the OAGW data plane proxies `{}` requests only, not `{method}`",
        ROUTE_METHODS.join("`, `")
    )))
}

/// Whether the request asks for a WebSocket upgrade.
///
/// RFC 7230 §6.7: a client signals an upgrade with the `Upgrade` header *and* by
/// listing `Upgrade` in `Connection`. Both are read as comma-separated tokens and
/// compared case-insensitively, because a client may send several tokens in
/// either header (`connection: keep-alive, Upgrade`).
#[must_use]
fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    has_token(headers, &header::CONNECTION, "upgrade")
        && has_token(headers, &header::UPGRADE, "websocket")
}

/// Whether any value of `name` carries `token` as one of its comma-separated
/// members.
#[must_use]
fn has_token(headers: &HeaderMap, name: &HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|text| {
            text.split(',')
                .any(|member| member.trim().eq_ignore_ascii_case(token))
        })
    })
}

/// Re-apply the upgrade handshake headers on top of the transformed set.
///
/// `connection` and `upgrade` are hop-by-hop (RFC 7230 §6.7), so the normal
/// transformation strips them — but two endpoints cannot negotiate a tunnel
/// without them. The gateway therefore forwards the `Upgrade` token it selected
/// for its own leg and every `sec-websocket-*` header of the handshake, after the
/// passthrough mode and the route rules ran. This is the smallest deviation from
/// the header rules an upgrade tunnel requires.
///
/// A header that survived the transformation is **not** appended again: RFC 6455
/// §4.1 requires exactly one `Sec-WebSocket-Key`, so a `passthrough: all` upstream
/// that already received the handshake headers must not receive them twice.
fn apply_upgrade_headers(outbound: &mut HeaderMap, inbound: &HeaderMap) {
    for (name, value) in inbound {
        if name != header::UPGRADE && !name.as_str().starts_with(SEC_WEBSOCKET_PREFIX) {
            continue;
        }
        if outbound.contains_key(name) {
            continue;
        }
        outbound.append(name, value.clone());
    }
    outbound.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
}

/// Stamp a passed-through response `X-OAGW-Error-Source: upstream` (ADR-0007:
/// every upstream response — 4xx and 5xx included — is stamped, so the client can
/// tell who produced it).
fn stamp_upstream(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    parts.headers.insert(
        ERROR_SOURCE_HEADER,
        HeaderValue::from_static(UPSTREAM_ERROR_SOURCE),
    );
    with_error_source(Response::from_parts(parts, body))
}

/// Splice the two halves of a completed upgrade together.
///
/// Each half fails on its own — a client that hangs up before the tunnel starts,
/// an upstream that resets, a protocol that simply ends — so the task closes what
/// it holds and reports the outcome instead of panicking. The tunnel is opaque
/// bytes: nothing of the handshake (keys, tokens, headers) is ever logged.
async fn splice(downstream: OnUpgrade, upstream: OnUpgrade) {
    let (client, upstream) = tokio::join!(downstream, upstream);
    let (Ok(client), Ok(upstream)) = (client, upstream) else {
        tracing::debug!("the OAGW upgrade tunnel was not established: one end refused");
        return;
    };
    relay(TokioIo::new(client), TokioIo::new(upstream)).await;
}

/// Copy the tunnel bytes both ways until either end closes, then report it.
///
/// `Upgraded` speaks hyper's IO traits rather than tokio's, so both ends go
/// through the adapter that tokio's `copy_bidirectional` reads and writes.
async fn relay<C, U>(mut client: TokioIo<C>, mut upstream: TokioIo<U>)
where
    C: hyper::rt::Read + hyper::rt::Write + Unpin,
    U: hyper::rt::Read + hyper::rt::Write + Unpin,
{
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((to_upstream, to_client)) => {
            tracing::debug!(
                "the OAGW upgrade tunnel closed after {to_upstream} bytes upstream and \
{to_client} bytes downstream"
            );
        }
        Err(error) => {
            tracing::debug!("the OAGW upgrade tunnel closed on a transport error: {error}");
        }
    }
}

/// The `X-OAGW-Target-Host` of the request, when present.
///
/// The value is decoded lossily rather than dropped when it is not visible
/// ASCII: the ADR-0001 matrix keys on the header's *presence* (a present header
/// is always validated), so an undecodable value has to reach
/// [`parse_target_host`] and be reported as `invalid_target_host` — treating it
/// as absent would forward a single-endpoint request with no validation at all.
fn target_host_of(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(TARGET_HOST_HEADER)?;
    Some(String::from_utf8_lossy(value.as_bytes()).into_owned())
}

/// Buffer the inbound request body, enforcing the body rules.
///
/// # Errors
///
/// Returns 400 for a non-numeric or duplicated `Content-Length`, a
/// `Transfer-Encoding` that is not exactly `chunked` or a length mismatch, and
/// 413 as soon as the body outgrows `limit`.
async fn read_inbound_body(
    headers: &HeaderMap,
    body: Body,
    limit: usize,
) -> Result<InboundBody, OagwError> {
    let content_length = declared_content_length(headers)?;
    let chunked = declared_transfer_encoding(headers)?;
    let mut stream = body.into_data_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            OagwError::validation(format!("the request body could not be read: {error}"))
        })?;
        if buffered.len() + chunk.len() > limit {
            return Err(OagwError::payload_too_large(format!(
                "the request body exceeds the configured limit of {limit} bytes"
            )));
        }
        buffered.extend_from_slice(&chunk);
    }
    let bytes = Bytes::from(buffered);
    if let Some(declared) = content_length
        && declared != bytes.len() as u64
    {
        return Err(OagwError::validation(format!(
            "the declared `Content-Length` of {declared} does not match the received body of {} \
             bytes",
            bytes.len()
        )));
    }
    Ok(InboundBody {
        bytes,
        declared: content_length.is_some() || chunked.is_some(),
    })
}

/// The single `Content-Length` of the request, when it declares one.
fn declared_content_length(headers: &HeaderMap) -> Result<Option<u64>, OagwError> {
    let mut values = headers.get_all(header::CONTENT_LENGTH).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(OagwError::validation(
            "the request declares more than one `Content-Length`",
        ));
    }
    let text = header_text(first, "Content-Length")?;
    let length = text.trim().parse::<u64>().map_err(|_| {
        OagwError::validation(format!(
            "the request declares a non-numeric `Content-Length` of `{text}`"
        ))
    })?;
    Ok(Some(length))
}

/// The single `Transfer-Encoding` of the request, when it declares one.
///
/// Only the bare `chunked` coding gets past this point: it describes the
/// connection, not the message, so any other value is rejected instead of being
/// silently translated into a length-delimited upstream request.
fn declared_transfer_encoding(headers: &HeaderMap) -> Result<Option<String>, OagwError> {
    let mut values = headers.get_all(header::TRANSFER_ENCODING).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(OagwError::validation(
            "the request declares more than one `Transfer-Encoding`",
        ));
    }
    let text = header_text(first, "Transfer-Encoding")?;
    if text.trim().eq_ignore_ascii_case("chunked") {
        return Ok(Some(text.trim().to_owned()));
    }
    Err(OagwError::validation(format!(
        "the request carries a `Transfer-Encoding` of `{text}`; only `chunked` is accepted"
    )))
}

/// A header value as text, or a 400 problem naming the header.
fn header_text(value: &HeaderValue, name: &str) -> Result<String, OagwError> {
    value.to_str().map(str::to_owned).map_err(|_| {
        OagwError::validation(format!("the request carries a non-textual `{name}` header"))
    })
}

/// Build the request the upstream receives.
///
/// The path follows the route's `path_suffix_mode` (`append` forwards the proxy
/// suffix verbatim, `disabled` forwards exactly the route path), the query is
/// filtered by `match.http.query_allowlist` and the headers are transformed in
/// the documented order.
fn build_outbound_request(
    inbound_headers: &HeaderMap,
    inbound_query: Option<&str>,
    target: &ResolvedTarget,
    method: &Method,
    path_suffix: &str,
    inbound: InboundBody,
    plugin_added: &[HeaderName],
) -> Result<Request<Body>, OagwError> {
    let match_rules = target.route.spec.match_rules.http();
    let mode = match_rules.map_or(PathSuffixMode::Append, |http| http.path_suffix_mode);
    let allowlist: &[String] = match_rules.map_or(&[], |http| http.query_allowlist.as_slice());
    let route_path = match_rules
        .map_or("/", |http| http.path.as_str())
        .to_owned();
    let path = outbound_path(&route_path, mode, path_suffix);
    let query = outbound_query(inbound_query, allowlist)?;
    let uri = outbound_uri(&target.endpoint, &path, query.as_deref())?;
    let headers = outbound_request_headers(
        inbound_headers,
        target,
        inbound.declared,
        inbound.bytes.len(),
        plugin_added,
    )?;

    let mut outbound = Request::new(Body::from(inbound.bytes));
    *outbound.method_mut() = method.clone();
    *outbound.uri_mut() = uri;
    *outbound.headers_mut() = headers;
    Ok(outbound)
}

/// The path of the upstream request (see the [module documentation](self)).
fn outbound_path(route_path: &str, mode: PathSuffixMode, suffix: &str) -> String {
    match mode {
        PathSuffixMode::Disabled => route_path.to_owned(),
        PathSuffixMode::Append => {
            if suffix.is_empty() {
                return "/".to_owned();
            }
            if suffix.starts_with('/') {
                return suffix.to_owned();
            }
            format!("/{suffix}")
        }
    }
}

/// Filter the proxy query down to the parameters the route allows, preserving
/// the original order and the original percent-encoding.
///
/// `match.http.query_allowlist` is a guard rule before it is a transformation
/// rule — DESIGN "Guard Rules": "Validate against `match.http.query_allowlist`;
/// reject if unknown" — so a parameter the route does not list is a **400**, not
/// a silent drop: a caller that misspells a parameter has to see that, and an
/// upstream that requires a parameter must never receive a request without it.
///
/// An **empty** allowlist allows none (`route.v1.schema.json`: "If empty, allow
/// none."), so a route that configures no allowlist forwards no query parameter
/// at all.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] naming the first parameter the allowlist does not
/// list.
pub fn outbound_query(
    raw_query: Option<&str>,
    allowlist: &[String],
) -> Result<Option<String>, OagwError> {
    let Some(raw) = raw_query else {
        return Ok(None);
    };
    let pairs: Vec<&str> = raw.split('&').filter(|pair| !pair.is_empty()).collect();
    if allowlist.is_empty() {
        return if pairs.is_empty() {
            Ok(None)
        } else {
            Err(unknown_query_param(query_param_name(pairs[0]), allowlist))
        };
    }
    let mut kept: Vec<&str> = Vec::with_capacity(pairs.len());
    for pair in &pairs {
        if is_allowed_query_name(query_param_name(pair), allowlist) {
            kept.push(pair);
        } else {
            return Err(unknown_query_param(query_param_name(pair), allowlist));
        }
    }
    if kept.is_empty() {
        return Ok(None);
    }
    Ok(Some(kept.join("&")))
}

/// 400 problem for a query parameter the route's allowlist does not list.
fn unknown_query_param(name: &str, allowlist: &[String]) -> OagwError {
    OagwError::validation(format!(
        "the query parameter `{name}` is not on the route's `query_allowlist` of [{}]",
        allowlist.join(", ")
    ))
}

/// The name of a `name=value` query pair, without decoding.
fn query_param_name(pair: &str) -> &str {
    match pair.split_once('=') {
        Some((name, _)) => name,
        None => pair,
    }
}

/// Whether a raw (possibly percent-encoded) query name is on the allowlist.
fn is_allowed_query_name(name: &str, allowlist: &[String]) -> bool {
    allowlist
        .iter()
        .any(|allowed| allowed == name || (name.contains('%') && *allowed == percent_decode(name)))
}

/// Percent-decode a query name, so an allowlist entry written as `api-key`
/// matches the raw name `api%2Dkey`.
#[must_use]
fn percent_decode(raw: &str) -> String {
    if !raw.contains('%') {
        return raw.to_owned();
    }
    let bytes = raw.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(value) = hex_pair(bytes, index) {
            decoded.push(value);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// The value of the `%XX` escape starting at `index`, if there is one.
fn hex_pair(bytes: &[u8], index: usize) -> Option<u8> {
    let high = *bytes.get(index + 1)?;
    let low = *bytes.get(index + 2)?;
    Some(hex_value(high)? * 16 + hex_value(low)?)
}

/// The value of a single hexadecimal digit.
fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// The absolute URI of the upstream request: the endpoint's scheme and
/// authority plus the transformed path and query.
fn outbound_uri(endpoint: &Endpoint, path: &str, query: Option<&str>) -> Result<Uri, OagwError> {
    let scheme = match endpoint.scheme {
        EndpointScheme::Http => "http",
        _ => "https",
    };
    let path_and_query = match query {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    };
    Uri::builder()
        .scheme(scheme)
        .authority(endpoint_authority(endpoint))
        .path_and_query(path_and_query)
        .build()
        .map_err(|error| {
            OagwError::internal(format!(
                "the resolved endpoint could not be turned into a request URI: {error}"
            ))
        })
}

/// The `Host` the upstream sees: `host`, or `host:port` for a non-default port.
///
/// An IPv6 literal is bracketed (`[::1]:8443`), because both the request
/// authority and the `Host` header follow RFC 3986: an unbracketed colon would
/// be read as a port separator and the URI builder rejects it.
#[must_use]
pub fn endpoint_authority(endpoint: &Endpoint) -> String {
    let host = normalize_host(&endpoint.host);
    let host = match host.parse::<IpAddr>() {
        Ok(IpAddr::V6(v6)) => format!("[{v6}]"),
        _ => host,
    };
    if endpoint.port == endpoint.scheme.default_port() {
        return host;
    }
    format!("{host}:{}", endpoint.port)
}

/// The outbound header set (see the [module documentation](self)).
fn outbound_request_headers(
    inbound: &HeaderMap,
    target: &ResolvedTarget,
    has_body: bool,
    body_length: usize,
    plugin_added: &[HeaderName],
) -> Result<HeaderMap, OagwError> {
    let mut outbound = HeaderMap::new();
    let authority = endpoint_authority(&target.endpoint);
    let host = HeaderValue::from_str(&authority).map_err(|_| {
        OagwError::internal(format!(
            "the endpoint authority `{authority}` is not a header value"
        ))
    })?;
    outbound.insert(header::HOST, host);
    let rules = target
        .upstream
        .spec
        .headers
        .as_ref()
        .and_then(|headers| headers.request.as_ref());
    // An upstream without a `headers` member carries no rules at all, which is
    // the schema default `passthrough: "none"` — not a mode that forwards
    // anything. The default is applied here so that the one rule every mode
    // shares, the forwarding of the headers a plugin wrote, still holds.
    let default_rules = RequestHeaders::default();
    let rules = rules.unwrap_or(&default_rules);
    apply_passthrough(&mut outbound, inbound, rules, plugin_added);
    if has_body {
        outbound.insert(header::CONTENT_LENGTH, HeaderValue::from(body_length));
    }
    if !outbound.contains_key(header::CONTENT_TYPE)
        && let Some(content_type) = inbound.get(header::CONTENT_TYPE)
    {
        outbound.insert(header::CONTENT_TYPE, content_type.clone());
    }
    apply_request_rules(&mut outbound, Some(rules));
    Ok(outbound)
}

/// Whether an inbound header must never reach the upstream.
fn is_never_forwarded(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(name)
        || name == header::HOST
        || name == header::CONTENT_LENGTH
        || name.as_str() == TARGET_HOST_HEADER
}

/// Copy the inbound headers the upstream's `passthrough` mode allows.
///
/// A header a plugin wrote is forwarded regardless of the mode: the passthrough
/// filter describes what the *client's* request may carry, and the credential a
/// `AuthPlugin` injected is not the client's to strip. `is_never_forwarded`
/// still vetoes everything it vetoes — the hop-by-hop rules are a property of
/// the connection, not of the passthrough mode, so no plugin can smuggle a
/// `Connection` or a second `Host` through.
fn apply_passthrough(
    outbound: &mut HeaderMap,
    inbound: &HeaderMap,
    rules: &RequestHeaders,
    plugin_added: &[HeaderName],
) {
    let allowlist: &[String] = rules.passthrough_allowlist.as_deref().unwrap_or(&[]);
    let forwards = |name: &HeaderName| -> bool {
        if is_never_forwarded(name) {
            return false;
        }
        if plugin_added.contains(name) {
            return true;
        }
        match rules.passthrough {
            HeaderPassthrough::None => false,
            HeaderPassthrough::Allowlist => allowlist
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(name.as_str())),
            HeaderPassthrough::All => true,
        }
    };
    for (name, value) in inbound {
        if forwards(name) {
            outbound.append(name, value.clone());
        }
    }
}

/// Apply the `set`, `add` and `remove` rules, in that order.
///
/// A rule that no HTTP layer could express cannot be stored (the model
/// validates every name and value), so the unreachable entries are skipped
/// rather than aborting a request the upstream could still serve.
fn apply_rules(
    outbound: &mut HeaderMap,
    set: &[(String, String)],
    add: &[(String, String)],
    remove: &[String],
) {
    for (name, value) in set {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            outbound.insert(name, value);
        }
    }
    for (name, value) in add {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if let Ok(value) = HeaderValue::from_str(value) {
            outbound.append(name, value);
        }
    }
    for name in remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            outbound.remove(&name);
        }
    }
}

/// Apply `upstream.headers.request` to the outbound headers.
fn apply_request_rules(outbound: &mut HeaderMap, rules: Option<&RequestHeaders>) {
    let Some(rules) = rules else {
        return;
    };
    let set: Vec<(String, String)> = rules
        .set
        .as_ref()
        .map(|set| {
            set.iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let add: Vec<(String, String)> = rules
        .add
        .as_ref()
        .map(|add| {
            add.iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let remove = rules.remove.clone().unwrap_or_default();
    apply_rules(outbound, &set, &add, &remove);
}

/// Apply `upstream.headers.response` and strip the hop-by-hop response headers.
///
/// The body is not touched: it keeps streaming to the client, with the end of
/// that stream reported by [`ObservedBody`] (PRD §8: "Upstream closes connection:
/// System closes client connection and logs event").
fn map_upstream_response(
    response: axum::http::Response<hyper::body::Incoming>,
    target: &ResolvedTarget,
) -> Response {
    let rules = target
        .upstream
        .spec
        .headers
        .as_ref()
        .and_then(|headers| headers.response.as_ref());
    let (mut parts, body) = response.into_parts();
    apply_response_rules(&mut parts.headers, rules);
    for name in HOP_BY_HOP {
        parts.headers.remove(&name);
    }
    Response::from_parts(
        parts,
        Body::new(ObservedBody::new(body, &target.upstream.alias)),
    )
}

/// A response body that reports how its upstream stream ended.
///
/// The gateway never buffers a response, so the end of the body is the only
/// place the lifecycle of a stream (server-sent events above all) can be
/// observed: PRD §8 requires "Upstream closes connection: System closes client
/// connection and logs event", and a truncated stream must not be
/// indistinguishable from a finished one in the logs. Nothing of the stream —
/// headers, bodies, secrets — is ever logged, only that it ended and why.
struct ObservedBody<B> {
    inner: B,
    alias: String,
    /// Set once the end has been reported, so a poll after an error cannot log
    /// the same stream twice.
    reported: bool,
}

impl<B> ObservedBody<B> {
    /// Wrap `inner`, attributing the stream to the upstream `alias`.
    fn new(inner: B, alias: &str) -> Self {
        Self {
            inner,
            alias: alias.to_owned(),
            reported: false,
        }
    }

    /// Report the end of the stream exactly once.
    fn report(&mut self, outcome: Result<(), &dyn std::fmt::Display>) {
        if self.reported {
            return;
        }
        self.reported = true;
        match outcome {
            Ok(()) => tracing::debug!(
                upstream = %self.alias,
                "the upstream response stream ended cleanly"
            ),
            Err(error) => tracing::warn!(
                upstream = %self.alias,
                "the upstream response stream ended on a transport error: {error}"
            ),
        }
    }
}

impl<B> http_body::Body for ObservedBody<B>
where
    B: http_body::Body<Data = Bytes> + Unpin,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(None) => {
                this.report(Ok(()));
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Ready(Some(Err(error))) => {
                this.report(Err(&error));
                std::task::Poll::Ready(Some(Err(error)))
            }
            other => other,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// Apply `upstream.headers.response` to the response for the client.
fn apply_response_rules(outbound: &mut HeaderMap, rules: Option<&ResponseHeaders>) {
    let Some(rules) = rules else {
        return;
    };
    let set: Vec<(String, String)> = rules
        .set
        .as_ref()
        .map(|set| {
            set.iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let add: Vec<(String, String)> = rules
        .add
        .as_ref()
        .map(|add| {
            add.iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default();
    let remove = rules.remove.clone().unwrap_or_default();
    apply_rules(outbound, &set, &add, &remove);
}

/// Map a transport failure of the upstream client onto the documented error
/// vocabulary (step 6 of the [module documentation](self)).
fn transport_error(error: &ClientError) -> OagwError {
    if error.is_connect() {
        return OagwError::link_unavailable(format!("the upstream connection failed: {error}"));
    }
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if let Some(hyper_error) = current.downcast_ref::<hyper::Error>() {
            return hyper_transport_error(hyper_error, error);
        }
        cause = current.source();
    }
    OagwError::protocol_error(format!("the upstream response could not be read: {error}"))
}

/// Map the `hyper::Error` at the bottom of a transport failure.
fn hyper_transport_error(error: &hyper::Error, source: &ClientError) -> OagwError {
    if error.is_timeout() {
        return OagwError::connection_timeout(format!(
            "the upstream connection timed out: {source}"
        ));
    }
    if error.is_incomplete_message()
        || error.is_body_write_aborted()
        || error.is_canceled()
        || error.is_closed()
    {
        return OagwError::downstream_error(format!(
            "the upstream closed the connection before completing the response: {source}"
        ));
    }
    OagwError::protocol_error(format!("the upstream response is not valid HTTP: {source}"))
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
