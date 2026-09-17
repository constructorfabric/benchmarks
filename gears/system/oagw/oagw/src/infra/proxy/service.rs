//! Data-plane orchestration: resolve, authorize, transform and dial.
//!
//! [`ProxyService`] turns one inbound request into one outbound request and
//! one streamed response, applying the effective tenant-chain configuration —
//! plugins, header rules, rate limits and CORS — in ADR-0002 order.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response};
use bytes::Bytes;
use hyper::upgrade::OnUpgrade;
use toolkit_security::SecurityContext;

use crate::api::rest::error::{ERROR_SOURCE_HEADER, ERROR_SOURCE_UPSTREAM};
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin::{Caller, PluginError, RequestContext};
use crate::domain::services::proxy::ProxyResolution;
use crate::infra::plugin::registry::PluginRegistries;
use crate::infra::proxy::body;
use crate::infra::proxy::egress;
use crate::infra::proxy::http::{self, OutboundClient};
use crate::infra::proxy::ws;
use crate::infra::rate_limit::{Admission, RateLimiter, ScopeKey, rate_limit_headers, scope_key};

/// Everything a proxied request carries once the handler has parsed it.
pub struct ProxyRequest {
    /// Caller identity, from the platform security context.
    pub context: SecurityContext,
    /// Request method.
    pub method: Method,
    /// Upstream alias taken from the URL.
    pub alias: String,
    /// Path forwarded upstream, without the `/proxy/{alias}` prefix.
    pub path: String,
    /// Query parameters, in wire order.
    pub query: Vec<(String, String)>,
    /// Inbound headers, before any transformation.
    pub headers: HeaderMap,
    /// Inbound body.
    pub body: Body,
    /// Client half of a WebSocket upgrade, taken from the inbound request.
    pub upgrade: Option<OnUpgrade>,
}

/// Response the gateway hands back to the caller.
pub type ProxyResponse = Response<Body>;

/// The inbound facts the outbound request is derived from.
struct Derived<'a> {
    resolution: &'a ProxyResolution,
    method: &'a Method,
    inbound: &'a HeaderMap,
    query: &'a [(String, String)],
    query_keys: &'a [String],
    alias: &'a str,
    caller: &'a Caller,
}

/// Executes proxied requests against the resolved upstream.
pub struct ProxyService {
    /// Alias, route and endpoint selection.
    data_plane: Arc<crate::domain::services::proxy::DataPlaneService>,
    /// Built-in plugin implementations.
    registries: PluginRegistries,
    /// Process-local token buckets.
    limiter: RateLimiter,
    /// Pooled outbound client.
    client: OutboundClient,
    /// Effective gear configuration.
    config: Arc<crate::config::OagwConfig>,
}

impl ProxyService {
    /// Assemble the service from its parts.
    #[must_use]
    pub fn new(
        data_plane: Arc<crate::domain::services::proxy::DataPlaneService>,
        registries: PluginRegistries,
        config: Arc<crate::config::OagwConfig>,
    ) -> Self {
        Self {
            data_plane,
            registries,
            limiter: RateLimiter::new(),
            client: OutboundClient::new(),
            config,
        }
    }

    /// Timeout bounding the upstream response-head phase.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.proxy_timeout_secs.max(1))
    }

    /// Run one proxied request end to end.
    ///
    /// # Errors
    ///
    /// Propagates the resolution, validation, plugin, rate-limit, CORS and
    /// transport errors of the data plane.
    pub async fn execute(&self, request: ProxyRequest) -> Result<ProxyResponse, DomainError> {
        let ProxyRequest {
            context,
            method,
            alias,
            path,
            query,
            headers,
            body,
            upgrade,
        } = request;
        let query_keys: Vec<String> = query.iter().map(|(key, _)| key.clone()).collect();
        let framing = body::framing(&headers);
        body::validate(&framing)?;
        let pinned = target_host(&headers);
        let resolution = self
            .data_plane
            .resolve(
                &context,
                &alias,
                method.as_str(),
                &path,
                &query_keys,
                pinned.as_deref(),
            )
            .await
            .map_err(|error| stamped(error, &alias))?;
        let caller = Caller::from_context(&context);
        let derived = Derived {
            resolution: &resolution,
            method: &method,
            inbound: &headers,
            query: &query,
            query_keys: &query_keys,
            alias: &alias,
            caller: &caller,
        };
        let admission = self.authorize(&derived).await?;
        let payload = body::read(body, framing.content_length).await?;
        if ws::is_upgrade(&method, &headers) {
            return self.upgrade(&derived, upgrade).await;
        }
        let outbound = self.prepare(&derived, payload).await?;
        self.forward(&derived, outbound, admission).await
    }

    /// Rate-limit, CORS-check and plugin-guard the request.
    ///
    /// Returns the admission decision so its budget headers can be stamped on
    /// the response.
    async fn authorize(&self, derived: &Derived<'_>) -> Result<Option<Admission>, DomainError> {
        let admission = self.enforce_rate_limit(derived).await?;
        if let Some(cors) = cors_of(derived.resolution) {
            crate::infra::cors::check_request(cors, derived.inbound, derived.method.as_str())?;
        }
        self.guard_request(derived).await?;
        Ok(admission)
    }

    /// Enforce the effective token bucket, when one is configured.
    async fn enforce_rate_limit(
        &self,
        derived: &Derived<'_>,
    ) -> Result<Option<Admission>, DomainError> {
        let Some(limit) = derived.resolution.effective.rate_limit else {
            return Ok(None);
        };
        let scope = rate_limit_scope(&limit, derived);
        let now = std::time::Instant::now();
        let admission = self.limiter.check(&limit, &scope, now).await;
        if admission.allowed {
            return Ok(Some(admission));
        }
        let sustained = &limit.sustained;
        let window = format!("{:?}", sustained.window).to_ascii_lowercase();
        let mut error = DomainError::rate_limit_exceeded(format!(
            "rate limit of {} requests per {} exceeded for `{}`",
            sustained.rate, window, derived.alias
        ));
        error.extensions.retry_after_seconds = Some(admission.retry_after_seconds);
        error.extensions.rate_limit = Some(crate::domain::error::RateLimitBudget {
            limit: admission.limit,
            remaining: admission.remaining,
            reset_at: admission.reset_at,
        });
        Err(stamped(error, derived.alias))
    }

    /// Run the guard plugins of the effective chain.
    async fn guard_request(&self, derived: &Derived<'_>) -> Result<(), DomainError> {
        let mut guarded = RequestContext {
            caller: derived.caller.clone(),
            config: serde_json::Value::Null,
            method: derived.method.as_str().to_owned(),
            path: derived.resolution.path.clone(),
            query: Vec::new(),
            headers: outbound_of(derived, false),
            body: Bytes::new(),
            attributes: BTreeMap::new(),
        };
        for binding in &derived.resolution.effective.plugins {
            let Ok(guard) = self.registries.guard.resolve(&binding.plugin_ref) else {
                continue;
            };
            let mut scoped = guarded.clone();
            scoped.config = binding.config.clone();
            let decision = guard.guard_request(&scoped).await.map_err(plugin_error)?;
            if !decision.allowed {
                return Err(stamped(
                    crate::domain::plugin::guard_rejection(&decision),
                    derived.alias,
                ));
            }
            guarded.attributes.extend(scoped.attributes);
        }
        Ok(())
    }

    /// Dial the upstream and stream the response back.
    async fn forward(
        &self,
        derived: &Derived<'_>,
        outbound: Request<Body>,
        admission: Option<Admission>,
    ) -> Result<ProxyResponse, DomainError> {
        let response = self.client.send(outbound, self.timeout()).await?;
        let status = response.status();
        let mut builder = Response::builder().status(status);
        for (name, value) in &http::response_headers(
            response.headers(),
            &derived.resolution.effective.headers.response,
        ) {
            builder = builder.header(name, value);
        }
        if let Some(limit) = derived.resolution.effective.rate_limit
            && let Some(admission) = admission
        {
            for (name, value) in rate_limit_headers(&admission, &limit) {
                if let (Ok(name), Ok(value)) = (
                    axum::http::HeaderName::try_from(name.as_str()),
                    axum::http::HeaderValue::from_str(&value),
                ) {
                    builder = builder.header(name, value);
                }
            }
        }
        // Responses stream to the caller unbuffered, so a response-phase
        // plugin that needs the whole body cannot run on this path; the
        // response header rules and CORS have already been applied above.
        let body = Body::new(response.into_body());
        let mut response = builder
            .body(body)
            .map_err(|error| DomainError::protocol_error(format!("upstream response: {error}")))?;
        if let Some(cors) = cors_of(derived.resolution) {
            crate::infra::cors::apply_response_headers(cors, response.headers_mut());
        }
        response.headers_mut().insert(
            ERROR_SOURCE_HEADER,
            HeaderValue::from_static(ERROR_SOURCE_UPSTREAM),
        );
        Ok(response)
    }

    /// Dial the upstream with the upgrade headers and bridge the streams.
    async fn upgrade(
        &self,
        derived: &Derived<'_>,
        upgrade: Option<OnUpgrade>,
    ) -> Result<ProxyResponse, DomainError> {
        let mut prepared = self.prepare(derived, Bytes::new()).await?;
        prepared.headers_mut().insert(
            axum::http::header::CONNECTION,
            axum::http::HeaderValue::from_static("Upgrade"),
        );
        ws::bridge(&self.client, prepared, upgrade, self.timeout()).await
    }

    /// Build the outbound request: header rules, plugins and body.
    ///
    /// The request body is buffered (the plugin contract carries `Bytes`), so
    /// [`body::MAX_BODY_BYTES`] bounds how much a proxied request may carry.
    async fn prepare(
        &self,
        derived: &Derived<'_>,
        payload: Bytes,
    ) -> Result<Request<Body>, DomainError> {
        let endpoint = &derived.resolution.endpoint;
        egress::gate(&self.config, endpoint).await?;
        let mut context = RequestContext {
            caller: derived.caller.clone(),
            config: serde_json::Value::Null,
            method: derived.method.as_str().to_owned(),
            path: derived.resolution.path.clone(),
            query: derived
                .query
                .iter()
                .filter(|(key, _)| derived.query_keys.contains(key))
                .cloned()
                .collect(),
            headers: outbound_of(derived, ws::is_upgrade(derived.method, derived.inbound)),
            body: payload,
            attributes: BTreeMap::new(),
        };
        self.run_request_plugins(derived, &mut context).await?;
        let body = Body::from(context.body.clone());
        let uri = http::upstream_uri(endpoint, &context.path, Some(&encode_query(&context.query)))?;
        let mut builder = Request::builder()
            .method(derived.method.clone())
            .uri(uri)
            .version(axum::http::Version::HTTP_11);
        for (name, value) in &context.headers {
            builder = builder.header(name, value);
        }
        builder.body(body).map_err(|error| {
            stamped(
                DomainError::validation(format!("outbound request: {error}")),
                derived.alias,
            )
        })
    }

    /// Execute auth, guard and transform plugins in chain order.
    async fn run_request_plugins(
        &self,
        derived: &Derived<'_>,
        context: &mut RequestContext,
    ) -> Result<(), DomainError> {
        let effective = &derived.resolution.effective;
        if let Some(auth) = &effective.auth {
            let plugin = self.registries.auth.resolve(&auth.plugin_type)?;
            let mut scoped = context.clone();
            scoped.config = auth.config.clone();
            plugin
                .authenticate(&mut scoped)
                .await
                .map_err(plugin_error)?;
            context.headers = scoped.headers;
            context.body = scoped.body;
            context.attributes.extend(scoped.attributes);
        }
        for binding in &effective.plugins {
            let Ok(transform) = self.registries.transform.resolve(&binding.plugin_ref) else {
                continue;
            };
            let mut scoped = context.clone();
            scoped.config = binding.config.clone();
            transform
                .transform_request(&mut scoped)
                .await
                .map_err(plugin_error)?;
            context.headers = scoped.headers;
            context.body = scoped.body;
            context.attributes.extend(scoped.attributes);
        }
        Ok(())
    }
}

/// The effective CORS configuration, when it is switched on.
fn cors_of(resolution: &ProxyResolution) -> Option<&crate::domain::model::CorsConfig> {
    resolution
        .effective
        .cors
        .as_ref()
        .filter(|cors| cors.enabled)
}

/// Request headers forwarded upstream for `derived`.
fn outbound_of(derived: &Derived<'_>, upgrade: bool) -> HeaderMap {
    http::outbound_headers(
        derived.inbound,
        &derived.resolution.effective.headers.request,
        &host(&derived.resolution.endpoint),
        upgrade,
    )
}

/// Scope key of the request under `limit`.
fn rate_limit_scope(
    limit: &crate::domain::model::RateLimitConfig,
    derived: &Derived<'_>,
) -> ScopeKey {
    scope_key(
        limit,
        &derived.resolution.upstream.id.to_string(),
        derived.caller.tenant_id,
        derived.caller.subject_id,
        forwarded_ip(derived.inbound),
        derived.resolution.route.as_ref().map(|route| route.id),
    )
}

/// `host:port` of the selected endpoint.
fn host(endpoint: &crate::domain::model::Endpoint) -> String {
    format!("{}:{}", endpoint.host, endpoint.port)
}

/// Attach the alias to a gateway error.
fn stamped(mut error: DomainError, alias: &str) -> DomainError {
    error.extensions.alias = Some(alias.to_owned());
    error
}

/// Convert a plugin failure into a contract error.
fn plugin_error(error: PluginError) -> DomainError {
    match error {
        PluginError::Config(detail) => DomainError::new(ErrorKind::Validation, detail),
        PluginError::Secret(detail) => DomainError::new(ErrorKind::SecretNotFound, detail),
        PluginError::Auth(detail) => DomainError::new(ErrorKind::AuthenticationFailed, detail),
        PluginError::Internal(detail) => DomainError::new(ErrorKind::PluginNotFound, detail),
    }
}

/// `x-oagw-target-host` of the request, when the caller pinned an endpoint.
fn target_host(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-oagw-target-host")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Left-most `X-Forwarded-For` entry, when the caller chain declared one.
fn forwarded_ip(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Encode query parameters for the outbound URI.
fn encode_query(query: &[(String, String)]) -> String {
    let pairs: Vec<String> = query
        .iter()
        .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
        .collect();
    pairs.join("&")
}

/// Percent-encode one query component, leaving the unreserved set alone.
fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            _ => drop(write!(encoded, "%{byte:02X}")),
        }
    }
    encoded
}
