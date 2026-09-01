//! Plugin system core (ADR-0002).
//!
//! Three plugin types with separate traits, the contexts they operate on and
//! the deterministic execution order the data plane (slice 4) relies on:
//!
//! ```text
//! Auth (auth_plugin)      -> credential injection
//! Guards (guard_plugin)   -> validation, may reject
//! Transforms (transform_plugin) -> request mutation
//!   -> upstream call
//! Transforms (transform_plugin) -> response / error mutation
//! ```
//!
//! ## Ordering
//!
//! Upstream plugins execute before route plugins, and within one tier the
//! declaration order of `plugins.items[]` is preserved. The phase buckets are
//! global: every auth plugin runs before every guard, and every guard runs
//! before every transform, regardless of tier. [`PluginChain`] keeps the
//! `(tier, declaration index)` pair of every link and returns the links of a
//! phase sorted by it.
//!
//! ## Configuration source
//!
//! ADR-0008 refers to `ctx.config` — the configuration payload of the plugin
//! *binding* being executed. The ADR-0002 traits carry no configuration
//! parameter and plugin definitions are immutable (ADR-0002: "updates are
//! performed by creating a new plugin version"), so every built-in plugin is
//! constructed with its binding configuration by the
//! [`crate::infra::plugin::PluginRegistry`] factory and treats it as
//! immutable. [`RequestContext::plugin_config`] stays on the context for data
//! planes that want to surface the same value to external plugins; the
//! built-ins never read it, which is why the guard phase (whose ADR-0002
//! signature takes `&RequestContext`) is unaffected by its per-link nature.
//!
//! ## Result type
//!
//! Every plugin method returns `Result<T, OagwError>`: the gear's own
//! [`crate::domain::error::OagwError`] taxonomy, so a plugin rejection renders
//! as the same `application/problem+json` document a gateway error does.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use bytes::Bytes;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{OagwError, ProblemBody, ProblemContext};
use crate::domain::rate_limit::RateLimitDecision;

/// GTS base type of an auth plugin (`gts.cf.core.oagw.auth_plugin.v1~*`).
pub const AUTH_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.auth_plugin.v1";
/// GTS base type of a guard plugin (`gts.cf.core.oagw.guard_plugin.v1~*`).
pub const GUARD_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.guard_plugin.v1";
/// GTS base type of a transform plugin (`gts.cf.core.oagw.transform_plugin.v1~*`).
pub const TRANSFORM_PLUGIN_TYPE_ID: &str = "gts.cf.core.oagw.transform_plugin.v1";

/// Built-in plugin GTS instance ids and the catalog-only identifiers that have
/// no backing implementation (ADR-0002 / PRD "Built-in Plugins").
pub mod builtin {
    /// Auth no-op: no credential injection.
    pub const NOOP_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
    /// API key injection (header or query parameter).
    pub const APIKEY_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    /// OAuth2 client credentials, client authentication in the form body.
    pub const OAUTH2_CLIENT_CRED: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
    /// OAuth2 client credentials, client authentication in the `Authorization`
    /// header.
    pub const OAUTH2_CLIENT_CRED_BASIC: &str =
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
    /// Required header enforcement (request and response phases).
    pub const REQUIRED_HEADERS_GUARD: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// `X-Request-ID` propagation.
    pub const REQUEST_ID_TRANSFORM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

    /// Catalog-only auth identifier: HTTP Basic, no backing plugin.
    pub const BASIC_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
    /// Catalog-only auth identifier: Bearer injection, no backing plugin.
    pub const BEARER_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
    /// Catalog-only guard identifier: request timeout, core data-plane logic.
    pub const TIMEOUT_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
    /// Catalog-only guard identifier: CORS, core data-plane logic.
    pub const CORS_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
    /// Catalog-only transform identifier: logging, core data-plane logic.
    pub const LOGGING_TRANSFORM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
    /// Catalog-only transform identifier: metrics, core data-plane logic.
    pub const METRICS_TRANSFORM: &str =
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

    /// GTS base type of `id`, i.e. the part before the `~` separator.
    ///
    /// Catalog-only identifiers carry their base type too, so an operator
    /// binding `cf.core.oagw.cors.v1` gets a precise "not resolvable" error
    /// instead of a generic not-found.
    #[must_use]
    pub fn base_type(plugin_ref: &str) -> Option<&str> {
        plugin_ref.split('~').next().filter(|base| !base.is_empty())
    }
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// Request body as seen by a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BodyPayload {
    /// No body at all (`GET`, `HEAD`, `204`).
    #[default]
    Empty,
    /// Fully buffered body. Auth and transform plugins only ever see this.
    Buffered(Bytes),
    /// The body is streamed by the data plane and is not materialised here.
    Streaming,
}

impl BodyPayload {
    /// Length of the buffered body in bytes; `None` for streaming bodies.
    #[must_use]
    pub fn buffered_len(&self) -> Option<usize> {
        match self {
            Self::Buffered(bytes) => Some(bytes.len()),
            Self::Empty | Self::Streaming => None,
        }
    }

    /// The buffered bytes, or `None` for streaming and empty bodies.
    #[must_use]
    pub fn as_buffered(&self) -> Option<&Bytes> {
        match self {
            Self::Buffered(bytes) => Some(bytes),
            Self::Empty | Self::Streaming => None,
        }
    }

    /// `true` when the data plane streams this body instead of buffering it.
    #[must_use]
    pub fn is_streaming(&self) -> bool {
        matches!(self, Self::Streaming)
    }
}

/// CORS evaluation outcome the proxy handler records on the request.
#[derive(Debug, Clone)]
pub enum CorsOutcome {
    /// CORS is disabled for the resolved upstream/route, or the request is
    /// not a CORS request (no `Origin` header).
    Disabled,
    /// Preflight answered locally by the gateway; carries the response headers.
    Preflight {
        /// `Access-Control-*` headers to emit on the `204`.
        headers: Vec<(HeaderName, HeaderValue)>,
    },
    /// Actual cross-origin request accepted; carries the response headers.
    Allowed {
        /// `Access-Control-*` headers to merge into the upstream response.
        headers: Vec<(HeaderName, HeaderValue)>,
    },
    /// Actual cross-origin request rejected before reaching the upstream.
    Rejected {
        /// The 403 problem document to return to the caller.
        error: OagwError,
    },
}

/// Everything a plugin can observe or mutate about the proxied request.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// HTTP method of the inbound request (`GET`, `POST`, ...).
    pub method: String,
    /// Host the request is proxied to (the pinned target host).
    pub target_host: String,
    /// Matched route path prefix.
    pub path: String,
    /// Path suffix taken from the proxy URL (`path_suffix_mode: append`).
    pub path_suffix: String,
    /// Raw query string, without the leading `?`.
    pub query: String,
    /// Inbound request headers. Auth and transform plugins mutate this map.
    pub headers: HeaderMap,
    /// Request body.
    pub body: BodyPayload,
    /// Tenant of the authenticated subject.
    pub tenant_id: Uuid,
    /// Authenticated subject id, `None` for anonymous or preflight requests.
    pub subject_id: Option<Uuid>,
    /// Security context handed to the credential store when a plugin resolves
    /// a `cred://` reference.
    pub security: Option<Arc<SecurityContext>>,
    /// Resolved upstream id.
    pub upstream_id: Option<Uuid>,
    /// Upstream alias the request was routed through.
    pub alias: String,
    /// Correlation id, propagated from the inbound `X-Request-ID` or generated.
    pub request_id: Option<String>,
    /// Client address, used by the `ip` rate-limit scope.
    pub peer_ip: Option<String>,
    /// Matched route id, used by the `route` rate-limit scope.
    pub route_id: Option<Uuid>,
    /// CORS evaluation outcome.
    pub cors: CorsOutcome,
    /// Rate-limit outcome, `None` when the request was not rate limited.
    pub rate_limit: Option<RateLimitDecision>,
    /// Headers injected into the outbound request, in injection order.
    pub injected_headers: Vec<(HeaderName, HeaderValue)>,
    /// Query parameters appended to the outbound request, in injection order.
    pub injected_query: Vec<(String, String)>,
    /// Configuration of the plugin currently executing (ADR-0008 `ctx.config`).
    /// Populated by the data plane; the built-in plugins receive the same
    /// payload at construction time and do not read it (see the module docs).
    pub plugin_config: serde_json::Value,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            method: String::new(),
            target_host: String::new(),
            path: String::new(),
            path_suffix: String::new(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: BodyPayload::Empty,
            tenant_id: Uuid::nil(),
            subject_id: None,
            security: None,
            upstream_id: None,
            alias: String::new(),
            request_id: None,
            peer_ip: None,
            route_id: None,
            cors: CorsOutcome::Disabled,
            rate_limit: None,
            injected_headers: Vec::new(),
            injected_query: Vec::new(),
            plugin_config: crate::domain::model::empty_json_object(),
        }
    }
}

impl RequestContext {
    /// Builder for a request context (see [`RequestContextBuilder`]).
    #[must_use]
    pub fn builder() -> RequestContextBuilder {
        RequestContextBuilder::default()
    }

    /// First value of a header, compared case-insensitively on the name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        let lowered = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| key.as_str() == lowered)
            .map(|(_, value)| value)
    }

    /// `true` when a header with this name is present (case-insensitive).
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// Records a header the outbound request must carry.
    ///
    /// An earlier injection of the same header name is replaced rather than
    /// appended, so a plugin that runs twice (or two plugins writing the same
    /// header) still leaves exactly one outbound value.
    pub fn inject_header(&mut self, name: HeaderName, value: HeaderValue) {
        match self
            .injected_headers
            .iter_mut()
            .find(|(candidate, _)| candidate.as_str() == name.as_str())
        {
            Some(entry) => entry.1 = value,
            None => self.injected_headers.push((name, value)),
        }
    }

    /// Records a query parameter the outbound request must carry.
    ///
    /// Like [`Self::inject_header`], an earlier injection of the same name is
    /// replaced.
    pub fn inject_query(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        match self
            .injected_query
            .iter_mut()
            .find(|(candidate, _)| *candidate == name)
        {
            Some(entry) => entry.1 = value,
            None => self.injected_query.push((name, value)),
        }
    }
}

/// Builds a [`RequestContext`] fluently; every field defaults.
#[derive(Debug, Clone, Default)]
pub struct RequestContextBuilder {
    context: RequestContext,
}

impl RequestContextBuilder {
    /// Sets the HTTP method.
    #[must_use]
    pub fn method(mut self, method: impl Into<String>) -> Self {
        self.context.method = method.into();
        self
    }

    /// Sets the target host.
    #[must_use]
    pub fn target_host(mut self, host: impl Into<String>) -> Self {
        self.context.target_host = host.into();
        self
    }

    /// Sets the matched path.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.context.path = path.into();
        self
    }

    /// Sets the path suffix.
    #[must_use]
    pub fn path_suffix(mut self, suffix: impl Into<String>) -> Self {
        self.context.path_suffix = suffix.into();
        self
    }

    /// Sets the raw query string.
    #[must_use]
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.context.query = query.into();
        self
    }

    /// Replaces the header map.
    #[must_use]
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.context.headers = headers;
        self
    }

    /// Sets the request body.
    #[must_use]
    pub fn body(mut self, body: BodyPayload) -> Self {
        self.context.body = body;
        self
    }

    /// Sets the tenant id.
    #[must_use]
    pub fn tenant_id(mut self, tenant_id: Uuid) -> Self {
        self.context.tenant_id = tenant_id;
        self
    }

    /// Sets the subject id.
    #[must_use]
    pub fn subject_id(mut self, subject_id: Uuid) -> Self {
        self.context.subject_id = Some(subject_id);
        self
    }

    /// Sets the security context used for `cred://` resolution.
    #[must_use]
    pub fn security(mut self, security: Arc<SecurityContext>) -> Self {
        self.context.security = Some(security);
        self
    }

    /// Sets the upstream id.
    #[must_use]
    pub fn upstream_id(mut self, upstream_id: Uuid) -> Self {
        self.context.upstream_id = Some(upstream_id);
        self
    }

    /// Sets the upstream alias.
    #[must_use]
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.context.alias = alias.into();
        self
    }

    /// Sets the request id.
    #[must_use]
    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.context.request_id = Some(request_id.into());
        self
    }

    /// Sets the client address.
    #[must_use]
    pub fn peer_ip(mut self, peer_ip: impl Into<String>) -> Self {
        self.context.peer_ip = Some(peer_ip.into());
        self
    }

    /// Sets the matched route id.
    #[must_use]
    pub fn route_id(mut self, route_id: Uuid) -> Self {
        self.context.route_id = Some(route_id);
        self
    }

    /// Sets the CORS outcome.
    #[must_use]
    pub fn cors(mut self, cors: CorsOutcome) -> Self {
        self.context.cors = cors;
        self
    }

    /// Sets the rate-limit outcome.
    #[must_use]
    pub fn rate_limit(mut self, decision: RateLimitDecision) -> Self {
        self.context.rate_limit = Some(decision);
        self
    }

    /// Sets the executing plugin configuration.
    #[must_use]
    pub fn plugin_config(mut self, config: serde_json::Value) -> Self {
        self.context.plugin_config = config;
        self
    }

    /// Consumes the builder, returning the context.
    #[must_use]
    pub fn build(self) -> RequestContext {
        self.context
    }
}

/// Everything a plugin can observe about the upstream response.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream response status.
    pub status: StatusCode,
    /// Upstream response headers; transforms may rewrite them.
    pub headers: HeaderMap,
    /// Response body.
    pub body: BodyPayload,
    /// Correlation id propagated from the request, when known.
    pub request_id: Option<String>,
    /// Configuration of the plugin currently executing (ADR-0008 `ctx.config`).
    pub plugin_config: serde_json::Value,
}

impl Default for ResponseContext {
    fn default() -> Self {
        Self {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: BodyPayload::Empty,
            request_id: None,
            plugin_config: crate::domain::model::empty_json_object(),
        }
    }
}

impl ResponseContext {
    /// Builder for a response context (see [`ResponseContextBuilder`]).
    #[must_use]
    pub fn builder() -> ResponseContextBuilder {
        ResponseContextBuilder::default()
    }

    /// Value of a single header (case-insensitive), for diagnostics and for
    /// the proxy handler.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.get(name)
    }

    /// `true` when a header with this name is present (case-insensitive).
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        let lowered = name.to_ascii_lowercase();
        self.headers.iter().any(|(key, _)| key.as_str() == lowered)
    }
}

/// Builds a [`ResponseContext`] fluently; every field defaults.
#[derive(Debug, Clone, Default)]
pub struct ResponseContextBuilder {
    context: ResponseContext,
}

impl ResponseContextBuilder {
    /// Sets the response status.
    #[must_use]
    pub fn status(mut self, status: StatusCode) -> Self {
        self.context.status = status;
        self
    }

    /// Replaces the header map.
    #[must_use]
    pub fn headers(mut self, headers: HeaderMap) -> Self {
        self.context.headers = headers;
        self
    }

    /// Sets the response body.
    #[must_use]
    pub fn body(mut self, body: BodyPayload) -> Self {
        self.context.body = body;
        self
    }

    /// Sets the propagated request id.
    #[must_use]
    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.context.request_id = Some(request_id.into());
        self
    }

    /// Sets the executing plugin configuration.
    #[must_use]
    pub fn plugin_config(mut self, config: serde_json::Value) -> Self {
        self.context.plugin_config = config;
        self
    }

    /// Consumes the builder, returning the context.
    #[must_use]
    pub fn build(self) -> ResponseContext {
        self.context
    }
}

/// Everything a transform plugin can rewrite about a gateway error.
///
/// `status` and `retry_after` are snapshots of `error` taken at construction
/// so a transform can rewrite the rendered response without re-deriving them
/// from the taxonomy.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// The gateway error being rendered.
    pub error: OagwError,
    /// HTTP status the error renders as.
    pub status: StatusCode,
    /// Response headers for the problem document (including `Retry-After`).
    pub headers: HeaderMap,
    /// `Retry-After` value in seconds, when the error carries one.
    pub retry_after: Option<u64>,
    /// Correlation id propagated from the request, when known.
    pub request_id: Option<String>,
    /// Configuration of the plugin currently executing (ADR-0008 `ctx.config`).
    pub plugin_config: serde_json::Value,
}

impl ErrorContext {
    /// Wraps a gateway error, taking its status and `Retry-After` hint.
    #[must_use]
    pub fn from_error(error: OagwError) -> Self {
        let status = error.status();
        let retry_after = error.context().retry_after_seconds;
        Self {
            error,
            status,
            headers: HeaderMap::new(),
            retry_after,
            request_id: None,
            plugin_config: crate::domain::model::empty_json_object(),
        }
    }

    /// The rendered problem document of the wrapped error.
    #[must_use]
    pub fn problem_body(&self) -> &ProblemBody {
        self.error.problem_body()
    }
}

// ---------------------------------------------------------------------------
// Guard decisions
// ---------------------------------------------------------------------------

/// Verdict of a guard plugin phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The phase accepted the request or response.
    Allow,
    /// The phase rejected it; carries the status and the machine-readable
    /// error code the problem document reports.
    Reject {
        /// HTTP status to return (`400` for request-phase rejections, `502`
        /// for response-phase rejections, ...).
        status: StatusCode,
        /// Stable error code, e.g. `REQUIRED_HEADER_MISSING` (ADR-0009).
        error_code: String,
        /// Human-readable detail for the problem document.
        detail: String,
    },
}

impl GuardDecision {
    /// An [`GuardDecision::Allow`] verdict.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }

    /// Builds a [`GuardDecision::Reject`] verdict.
    #[must_use]
    pub fn reject(
        status: StatusCode,
        error_code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::Reject {
            status,
            error_code: error_code.into(),
            detail: detail.into(),
        }
    }

    /// `true` for [`GuardDecision::Allow`].
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// HTTP status of the verdict; `None` when the phase allowed.
    #[must_use]
    pub const fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Allow => None,
            Self::Reject { status, .. } => Some(*status),
        }
    }

    /// Stable error code of the verdict; `None` when the phase allowed.
    #[must_use]
    pub const fn error_code(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Reject { error_code, .. } => Some(error_code.as_str()),
        }
    }

    /// Converts a rejection into the gear's error taxonomy so a guard
    /// rejection renders like any other gateway error.
    ///
    /// # Errors
    ///
    /// Returns the rejection mapped onto [`OagwError`]; allowing verdicts
    /// never reach this method, but when they do the error is
    /// [`OagwError::validation`] with the rejection detail.
    pub fn into_error(self) -> OagwError {
        let Self::Reject {
            status,
            error_code,
            detail,
        } = self
        else {
            return OagwError::validation("guard phase allowed, no rejection to map");
        };
        let detail = format!("{error_code}: {detail}");
        match status {
            StatusCode::BAD_REQUEST => OagwError::validation(detail),
            StatusCode::UNAUTHORIZED => OagwError::authentication_failed(detail),
            StatusCode::FORBIDDEN => OagwError::forbidden(detail),
            StatusCode::TOO_MANY_REQUESTS => OagwError::rate_limit_exceeded(detail),
            StatusCode::NOT_FOUND => OagwError::route_not_found(detail),
            StatusCode::BAD_GATEWAY => OagwError::downstream_error(detail),
            StatusCode::SERVICE_UNAVAILABLE => OagwError::link_unavailable(detail),
            _ => OagwError::downstream_error(detail),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin traits (ADR-0002 signatures, verbatim)
// ---------------------------------------------------------------------------

/// Credential injection. Executed once per request, before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Registry key of this plugin (the full GTS plugin id for built-ins).
    fn id(&self) -> &str;

    /// GTS base type of this plugin (`gts.cf.core.oagw.auth_plugin.v1`).
    fn plugin_type(&self) -> &str;

    /// Injects credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the credential cannot be resolved or
    /// does not match the configured expectation.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Validation and policy enforcement. Executed after auth, before transform.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Registry key of this plugin (the full GTS plugin id for built-ins).
    fn id(&self) -> &str;

    /// GTS base type of this plugin (`gts.cf.core.oagw.guard_plugin.v1`).
    fn plugin_type(&self) -> &str;

    /// Validates the request before it is proxied.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the guard itself fails (as opposed to
    /// rejecting the request, which is reported through [`GuardDecision`]).
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError>;

    /// Validates the upstream response before it is returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the guard itself fails.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Request/response/error mutation. Executed around the proxy call.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Registry key of this plugin (the full GTS plugin id for built-ins).
    fn id(&self) -> &str;

    /// GTS base type of this plugin (`gts.cf.core.oagw.transform_plugin.v1`).
    fn plugin_type(&self) -> &str;

    /// Rewrites the request before it leaves the gateway.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;

    /// Rewrites the upstream response before it is returned to the caller.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;

    /// Rewrites the problem document the gateway is about to return.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

// ---------------------------------------------------------------------------
// Chain
// ---------------------------------------------------------------------------

/// Tier of a plugin binding: the upstream chain runs before the route chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PluginTier {
    /// Plugins bound on the upstream (`upstream.plugins` / `upstream.auth`).
    #[default]
    Upstream,
    /// Plugins bound on the matched route (`route.plugins`).
    Route,
}

/// The concrete plugin a chain link resolves to.
#[derive(Clone)]
enum PluginHandle {
    Auth(Arc<dyn AuthPlugin>),
    Guard(Arc<dyn GuardPlugin>),
    Transform(Arc<dyn TransformPlugin>),
}

impl std::fmt::Debug for PluginHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Auth(_) => "auth",
            Self::Guard(_) => "guard",
            Self::Transform(_) => "transform",
        };
        formatter.write_str(kind)
    }
}

/// One resolved plugin occurrence in a chain.
#[derive(Debug, Clone)]
struct ChainLink {
    tier: PluginTier,
    declaration: usize,
    plugin_ref: String,
    plugin: PluginHandle,
}

/// Ordered plugin chain (ADR-0002 execution order).
///
/// Built by [`crate::infra::plugin::PluginRegistry::build_chain`] from an
/// upstream and its matched route; the runner methods below are the only
/// execution path the data plane needs.
#[derive(Debug, Clone)]
pub struct PluginChain {
    links: Vec<ChainLink>,
}

impl PluginChain {
    /// Builds an empty chain.
    #[must_use]
    pub const fn new() -> Self {
        Self { links: Vec::new() }
    }

    /// Appends an auth plugin.
    pub fn push_auth(
        &mut self,
        tier: PluginTier,
        declaration: usize,
        plugin_ref: impl Into<String>,
        plugin: Arc<dyn AuthPlugin>,
    ) {
        self.links.push(ChainLink {
            tier,
            declaration,
            plugin_ref: plugin_ref.into(),
            plugin: PluginHandle::Auth(plugin),
        });
    }

    /// Appends a guard plugin.
    pub fn push_guard(
        &mut self,
        tier: PluginTier,
        declaration: usize,
        plugin_ref: impl Into<String>,
        plugin: Arc<dyn GuardPlugin>,
    ) {
        self.links.push(ChainLink {
            tier,
            declaration,
            plugin_ref: plugin_ref.into(),
            plugin: PluginHandle::Guard(plugin),
        });
    }

    /// Appends a transform plugin.
    pub fn push_transform(
        &mut self,
        tier: PluginTier,
        declaration: usize,
        plugin_ref: impl Into<String>,
        plugin: Arc<dyn TransformPlugin>,
    ) {
        self.links.push(ChainLink {
            tier,
            declaration,
            plugin_ref: plugin_ref.into(),
            plugin: PluginHandle::Transform(plugin),
        });
    }

    /// `true` when no plugin is bound.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    /// Number of resolved plugin links.
    #[must_use]
    pub fn len(&self) -> usize {
        self.links.len()
    }

    /// Plugin references in execution order (upstream first, then route).
    #[must_use]
    pub fn plugin_refs(&self) -> Vec<String> {
        self.ordered()
            .into_iter()
            .map(|link| link.plugin_ref.clone())
            .collect()
    }

    /// The links of this chain in ADR-0002 execution order.
    fn ordered(&self) -> Vec<&ChainLink> {
        let mut links: Vec<&ChainLink> = self.links.iter().collect();
        links.sort_by_key(|link| (link.tier, link.declaration));
        links
    }

    fn links_of(&self, filter: impl Fn(&PluginHandle) -> bool) -> Vec<&ChainLink> {
        self.ordered()
            .into_iter()
            .filter(|link| filter(&link.plugin))
            .collect()
    }

    /// Auth plugins in execution order.
    #[must_use]
    pub fn auth_plugins(&self) -> Vec<Arc<dyn AuthPlugin>> {
        self.links_of(|handle| matches!(handle, PluginHandle::Auth(_)))
            .into_iter()
            .filter_map(|link| match &link.plugin {
                PluginHandle::Auth(plugin) => Some(Arc::clone(plugin)),
                _ => None,
            })
            .collect()
    }

    /// Guard plugins in execution order.
    #[must_use]
    pub fn guard_plugins(&self) -> Vec<Arc<dyn GuardPlugin>> {
        self.links_of(|handle| matches!(handle, PluginHandle::Guard(_)))
            .into_iter()
            .filter_map(|link| match &link.plugin {
                PluginHandle::Guard(plugin) => Some(Arc::clone(plugin)),
                _ => None,
            })
            .collect()
    }

    /// Transform plugins in execution order.
    #[must_use]
    pub fn transform_plugins(&self) -> Vec<Arc<dyn TransformPlugin>> {
        self.links_of(|handle| matches!(handle, PluginHandle::Transform(_)))
            .into_iter()
            .filter_map(|link| match &link.plugin {
                PluginHandle::Transform(plugin) => Some(Arc::clone(plugin)),
                _ => None,
            })
            .collect()
    }

    /// Runs every auth plugin in order, stopping at the first failure.
    ///
    /// # Errors
    ///
    /// Returns the error of the failing plugin; earlier injections stay in
    /// `ctx` because the data plane aborts the request on the next line.
    pub async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Auth(_))) {
            if let PluginHandle::Auth(plugin) = &link.plugin {
                plugin.authenticate(ctx).await?;
            }
        }
        Ok(())
    }

    /// Runs the request phase of every guard plugin in execution order.
    ///
    /// # Errors
    ///
    /// Returns the mapped rejection of the first guard that rejects.
    pub async fn guard_request(&self, ctx: &RequestContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Guard(_))) {
            if let PluginHandle::Guard(plugin) = &link.plugin {
                let decision = plugin.guard_request(ctx).await?;
                if let GuardDecision::Reject { .. } = decision {
                    return Err(decision.into_error());
                }
            }
        }
        Ok(())
    }

    /// Runs the response phase of every guard plugin in execution order.
    ///
    /// # Errors
    ///
    /// Returns the mapped rejection of the first guard that rejects.
    pub async fn guard_response(&self, ctx: &ResponseContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Guard(_))) {
            if let PluginHandle::Guard(plugin) = &link.plugin {
                let decision = plugin.guard_response(ctx).await?;
                if let GuardDecision::Reject { .. } = decision {
                    return Err(decision.into_error());
                }
            }
        }
        Ok(())
    }

    /// Runs the request phase of every transform plugin in execution order.
    ///
    /// # Errors
    ///
    /// Returns the error of the failing transform.
    pub async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Transform(_))) {
            if let PluginHandle::Transform(plugin) = &link.plugin {
                plugin.transform_request(ctx).await?;
            }
        }
        Ok(())
    }

    /// Runs the response phase of every transform plugin in execution order.
    ///
    /// # Errors
    ///
    /// Returns the error of the failing transform.
    pub async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Transform(_))) {
            if let PluginHandle::Transform(plugin) = &link.plugin {
                plugin.transform_response(ctx).await?;
            }
        }
        Ok(())
    }

    /// Runs the error phase of every transform plugin in execution order.
    ///
    /// # Errors
    ///
    /// Returns the error of the failing transform.
    pub async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError> {
        for link in self.links_of(|handle| matches!(handle, PluginHandle::Transform(_))) {
            if let PluginHandle::Transform(plugin) = &link.plugin {
                plugin.transform_error(ctx).await?;
            }
        }
        Ok(())
    }
}

impl Default for PluginChain {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds a CORS problem document with the ADR-0004 GTS error ids.
///
/// The two CORS error ids are not part of the [`crate::domain::error`]
/// taxonomy (which has a single `cors.forbidden` variant), so they are
/// rendered here through the public [`OagwError::Forbidden`] variant with an
/// explicit GTS type id.
#[must_use]
pub fn cors_error(gts_type: &str, title: &str, detail: String, invalid_value: String) -> OagwError {
    OagwError::Forbidden(Box::new(ProblemBody {
        context: ProblemContext {
            invalid_value: Some(invalid_value),
            ..ProblemContext::default()
        },
        ..ProblemBody::bare(gts_type.to_owned(), title.to_owned(), 403, detail)
    }))
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
