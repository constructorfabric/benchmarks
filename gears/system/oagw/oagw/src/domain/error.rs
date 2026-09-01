//! Typed error taxonomy for the OAGW gear (DESIGN section 3.3).
//!
//! Every failure the gear reports is one [`OagwError`] variant. Rendering is
//! RFC 9457 `application/problem+json` with:
//!
//! * `type` — the GTS error type id (`gts.cf.core.errors.err.v1~cf.oagw.*.v1`);
//! * `title`, `status`, `detail`, `instance` — the RFC fields;
//! * the gear-specific extension fields (`alias`, `host`, `upstream_id`,
//!   `valid_hosts`, `trace_id`, ...) **at the top level of the body**, which is
//!   where the ADR-0007 wire examples and the DESIGN section 3.3
//!   "Extension Fields" list put them;
//! * the same fields *also* nested under `context` (deliberate duplication, see
//!   [`ProblemBody`]);
//! * the `X-OAGW-Error-Source` header pinned to `gateway` (ADR-0007), since
//!   every error produced by this enum originates in the gateway itself.
//!
//! `X-OAGW-Error-Source: upstream` is stamped by the proxy engine (slice 4)
//! when the *upstream* response is itself the failure, never here.
//!
//! Each variant owns its already-rendered [`ProblemBody`] behind a [`Box`], so
//! the enum is pointer-sized and cheap to pass through `Result` on the hot
//! data-plane path (slice 4).
//!
//! ## GTS type ids beyond the DESIGN section 3.3 table
//!
//! The DESIGN error table does not name the management-plane statuses, so the
//! taxonomy adds three ids to it (the remaining ids are the table's own):
//!
//! | GTS id fragment | HTTP | Used for |
//! |---|---|---|
//! | `conflict` | 409 | duplicate alias / duplicate route match rule |
//! | `not_found` | 404 | a resource the calling tenant does not own |
//! | `tenancy.bind_forbidden` | 403 | an ancestor *enforces* the alias a descendant tries to bind (DESIGN "Hierarchical Configuration") |
//!
//! `cors.forbidden` stays reserved for CORS denials; tenancy denials are
//! reported as `tenancy.bind_forbidden` so a client can tell the two apart.

use std::fmt;

use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Media type of the RFC 9457 problem documents this gear emits.
pub const APPLICATION_PROBLEM_JSON: &str = "application/problem+json";

/// `X-OAGW-Error-Source` header name (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Error-source value produced by the gateway itself.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";

/// Error-source value produced when the upstream response is the failure.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";

/// GTS base type of every OAGW error instance.
pub const ERROR_TYPE_BASE: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// Result alias of the OAGW handlers: the error side is always this gear's own
/// taxonomy rather than the toolkit's canonical catalogue, because an OAGW
/// problem document carries OAGW GTS error types and OAGW extension fields.
pub type ApiResult<T> = Result<T, OagwError>;

/// Extension fields rendered inside the problem `context` object.
///
/// Every field is optional and omitted when `None`; the object itself is
/// always emitted (possibly empty) so clients can rely on `context` being
/// present. The same values are mirrored at the top level of the body
/// ([`ProblemBody`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProblemContext {
    /// Upstream alias that could not be resolved, or the resolved alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Upstream GTS instance id involved in the failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Target host involved in the failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path involved in the failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Hosts advertised by the upstream endpoint pool (for host-mismatch errors).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub valid_hosts: Vec<String>,
    /// Value rejected by validation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    /// Plugin instance id involved in the failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Seconds after which the client may retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Resources preventing a deletion (ADR-0001 `PluginInUse`).
    #[serde(skip_serializing_if = "referenced_by_is_empty")]
    pub referenced_by: Option<Box<ReferencedBy>>,
}

impl ProblemContext {
    /// `true` when no extension field is populated.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alias.is_none()
            && self.upstream_id.is_none()
            && self.host.is_none()
            && self.path.is_none()
            && self.valid_hosts.is_empty()
            && self.invalid_value.is_none()
            && self.plugin_id.is_none()
            && self.retry_after_seconds.is_none()
            && self.referenced_by.is_none()
    }
}

/// Resources that still reference a plugin.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReferencedBy {
    /// Upstream GTS ids referencing the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Route GTS ids referencing the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
}

impl ReferencedBy {
    /// `true` when neither resource kind still references the plugin.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty() && self.routes.is_empty()
    }
}

fn referenced_by_is_empty(value: &Option<Box<ReferencedBy>>) -> bool {
    value
        .as_ref()
        .is_none_or(|referenced| referenced.is_empty())
}

/// Flat RFC 9457 problem document.
///
/// ## Two views of one extension field
///
/// The OAGW extension fields are emitted **twice**, on purpose:
///
/// * **top level** — where the ADR-0007 wire examples and the DESIGN section
///   3.3 "Extension Fields" list put them, so a client reads
///   `body["upstream_id"]` and not `body["context"]["upstream_id"]`;
/// * **inside `context`** — because the platform's
///   `toolkit::api::canonical_error_middleware` parses *every*
///   `application/problem+json` response and requires the
///   `type`/`title`/`status`/`detail`/`context` quintet; dropping `context`
///   would make that middleware log an error and re-emit the body untouched.
///
/// [`ProblemBody::apply_context_extensions`] copies the `context` values into
/// the top-level fields, so both views always carry the same value and the
/// nested object stays the single source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemBody {
    /// GTS error type id.
    pub r#type: String,
    /// Short human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Human-readable failure detail.
    pub detail: String,
    /// Request path that triggered the failure, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Distributed-tracing correlation id, filled from the request by
    /// [`crate::api::rest::error_layer::error_source_layer`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Upstream involved in the failure (ADR-0007 extension field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// Alias involved in the failure (ADR-0007 extension field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Target host involved in the failure (ADR-0007 extension field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Request path involved in the failure (ADR-0007 extension field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Hosts advertised by the upstream endpoint pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub valid_hosts: Vec<String>,
    /// Value rejected by validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
    /// Plugin instance id involved in the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    /// Resources preventing a deletion (ADR-0001 `PluginInUse`).
    #[serde(default, skip_serializing_if = "referenced_by_is_empty")]
    pub referenced_by: Option<Box<ReferencedBy>>,
    /// Seconds after which the client may retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// Gear-specific extension fields, nested for the platform middleware.
    #[serde(default)]
    pub context: ProblemContext,
}

impl ProblemBody {
    /// Builds a body with no extension fields, for the fixed taxonomic shape of
    /// one [`OagwError`] variant.
    #[must_use]
    pub fn bare(r#type: String, title: String, status: u16, detail: String) -> Self {
        Self {
            r#type,
            title,
            status,
            detail,
            instance: None,
            trace_id: None,
            upstream_id: None,
            alias: None,
            host: None,
            path: None,
            valid_hosts: Vec::new(),
            invalid_value: None,
            plugin_id: None,
            referenced_by: None,
            retry_after_seconds: None,
            context: ProblemContext::default(),
        }
    }

    /// Copies the `context` extension fields to the ADR-0007 top-level fields.
    ///
    /// Idempotent: a top-level field already present (e.g. a `trace_id` the
    /// error layer added) is left alone, and the `context` object is never
    /// cleared, so running this on an already-flat body changes nothing.
    pub fn apply_context_extensions(&mut self) {
        macro_rules! mirror {
            ($($field:ident),+ $(,)?) => {
                $(if self.$field.is_none() {
                    self.$field = self.context.$field.clone();
                })+
            };
        }
        mirror!(
            upstream_id,
            alias,
            host,
            path,
            invalid_value,
            plugin_id,
            referenced_by,
            retry_after_seconds,
        );
        if self.valid_hosts.is_empty() {
            self.valid_hosts = self.context.valid_hosts.clone();
        }
    }
}

/// Static classification of one [`OagwError`] variant: its GTS type id, HTTP
/// status and RFC 9457 `title`.
struct ErrorSpec {
    gts_fragment: &'static str,
    status: u16,
    title: &'static str,
}

impl ErrorSpec {
    const fn new(gts_fragment: &'static str, status: u16, title: &'static str) -> Self {
        Self {
            gts_fragment,
            status,
            title,
        }
    }

    /// Builds a body for `detail`, with no extension fields set yet.
    fn body(&self, detail: String) -> ProblemBody {
        ProblemBody::bare(
            format!("{ERROR_TYPE_BASE}.{}.v1", self.gts_fragment),
            self.title.to_owned(),
            self.status,
            detail,
        )
    }
}

const VALIDATION: ErrorSpec = ErrorSpec::new("validation.error", 400, "Validation Error");
const MISSING_TARGET_HOST: ErrorSpec =
    ErrorSpec::new("routing.missing_target_host", 400, "Missing Target Host");
const INVALID_TARGET_HOST: ErrorSpec =
    ErrorSpec::new("routing.invalid_target_host", 400, "Invalid Target Host");
const UNKNOWN_TARGET_HOST: ErrorSpec =
    ErrorSpec::new("routing.unknown_target_host", 400, "Unknown Target Host");
const AUTHENTICATION_FAILED: ErrorSpec =
    ErrorSpec::new("auth.failed", 401, "Authentication Failed");
const ROUTE_NOT_FOUND: ErrorSpec = ErrorSpec::new("route.not_found", 404, "Route Not Found");
const PLUGIN_IN_USE: ErrorSpec = ErrorSpec::new("plugin.in_use", 409, "Plugin In Use");
const PAYLOAD_TOO_LARGE: ErrorSpec = ErrorSpec::new("payload.too_large", 413, "Payload Too Large");
const RATE_LIMIT_EXCEEDED: ErrorSpec =
    ErrorSpec::new("rate_limit.exceeded", 429, "Rate Limit Exceeded");
const SECRET_NOT_FOUND: ErrorSpec = ErrorSpec::new("secret.not_found", 500, "Secret Not Found");
const PROTOCOL_ERROR: ErrorSpec = ErrorSpec::new("protocol.error", 502, "Protocol Error");
const DOWNSTREAM_ERROR: ErrorSpec = ErrorSpec::new("downstream.error", 502, "Downstream Error");
const STREAM_ABORTED: ErrorSpec = ErrorSpec::new("stream.aborted", 502, "Stream Aborted");
const LINK_UNAVAILABLE: ErrorSpec = ErrorSpec::new("link.unavailable", 503, "Link Unavailable");
const CIRCUIT_BREAKER_OPEN: ErrorSpec =
    ErrorSpec::new("circuit_breaker.open", 503, "Circuit Breaker Open");
const PLUGIN_NOT_FOUND: ErrorSpec = ErrorSpec::new("plugin.not_found", 503, "Plugin Not Found");
const CONNECTION_TIMEOUT: ErrorSpec =
    ErrorSpec::new("timeout.connection", 504, "Connection Timeout");
const REQUEST_TIMEOUT: ErrorSpec = ErrorSpec::new("timeout.request", 504, "Request Timeout");
const IDLE_TIMEOUT: ErrorSpec = ErrorSpec::new("timeout.idle", 504, "Idle Timeout");
const CONFLICT: ErrorSpec = ErrorSpec::new("conflict", 409, "Conflict");
const NOT_FOUND: ErrorSpec = ErrorSpec::new("not_found", 404, "Not Found");
const FORBIDDEN: ErrorSpec = ErrorSpec::new("cors.forbidden", 403, "Forbidden");
const BIND_FORBIDDEN: ErrorSpec = ErrorSpec::new("tenancy.bind_forbidden", 403, "Bind Forbidden");

/// OAGW error taxonomy (DESIGN section 3.3).
#[derive(Debug, Clone)]
pub enum OagwError {
    /// Request payload failed structural or semantic validation.
    Validation(Box<ProblemBody>),
    /// Proxy request carried no target host.
    MissingTargetHost(Box<ProblemBody>),
    /// Proxy target host is malformed or not allowed.
    InvalidTargetHost(Box<ProblemBody>),
    /// Proxy target host does not match any endpoint of the upstream.
    UnknownTargetHost(Box<ProblemBody>),
    /// Credentials were rejected by the auth plugin or the credential store.
    AuthenticationFailed(Box<ProblemBody>),
    /// No route matched the request.
    RouteNotFound(Box<ProblemBody>),
    /// Plugin is still referenced by an upstream or a route.
    PluginInUse(Box<ProblemBody>),
    /// Request or response body exceeded the configured cap.
    PayloadTooLarge(Box<ProblemBody>),
    /// Rate-limit budget exhausted.
    RateLimitExceeded(Box<ProblemBody>),
    /// A credential secret referenced by a plugin does not exist.
    SecretNotFound(Box<ProblemBody>),
    /// Upstream violated the HTTP protocol contract.
    ProtocolError(Box<ProblemBody>),
    /// Upstream returned an error the gateway maps through.
    DownstreamError(Box<ProblemBody>),
    /// Streaming exchange was aborted mid-flight.
    StreamAborted(Box<ProblemBody>),
    /// No healthy link to the upstream.
    LinkUnavailable(Box<ProblemBody>),
    /// Circuit breaker for the upstream is open.
    CircuitBreakerOpen(Box<ProblemBody>),
    /// A plugin reference does not resolve to a known plugin.
    PluginNotFound(Box<ProblemBody>),
    /// Upstream connection phase exceeded the budget.
    ConnectionTimeout(Box<ProblemBody>),
    /// Full request/response exchange exceeded the budget.
    RequestTimeout(Box<ProblemBody>),
    /// Idle read/write window elapsed.
    IdleTimeout(Box<ProblemBody>),
    /// Uniqueness conflict (duplicate alias, duplicate route match rule).
    Conflict(Box<ProblemBody>),
    /// Requested resource does not exist.
    NotFound(Box<ProblemBody>),
    /// CORS preflight or actual-request check rejected the request.
    Forbidden(Box<ProblemBody>),
    /// A hierarchical bind rule of an ancestor refused the alias (DESIGN
    /// "Hierarchical Configuration"): the ancestor *enforces* its sections.
    ///
    /// Distinct from [`OagwError::Forbidden`], which stays the CORS denial, so
    /// a client can tell a tenancy refusal from an origin refusal.
    BindForbidden(Box<ProblemBody>),
}

impl OagwError {
    /// The already-rendered problem document for this variant.
    #[must_use]
    pub const fn problem_body(&self) -> &ProblemBody {
        match self {
            Self::Validation(body)
            | Self::MissingTargetHost(body)
            | Self::InvalidTargetHost(body)
            | Self::UnknownTargetHost(body)
            | Self::AuthenticationFailed(body)
            | Self::RouteNotFound(body)
            | Self::PluginInUse(body)
            | Self::PayloadTooLarge(body)
            | Self::RateLimitExceeded(body)
            | Self::SecretNotFound(body)
            | Self::ProtocolError(body)
            | Self::DownstreamError(body)
            | Self::StreamAborted(body)
            | Self::LinkUnavailable(body)
            | Self::CircuitBreakerOpen(body)
            | Self::PluginNotFound(body)
            | Self::ConnectionTimeout(body)
            | Self::RequestTimeout(body)
            | Self::IdleTimeout(body)
            | Self::Conflict(body)
            | Self::NotFound(body)
            | Self::Forbidden(body)
            | Self::BindForbidden(body) => body,
        }
    }

    /// Mutable problem document, for builders that enrich an error in place.
    const fn problem_body_mut(&mut self) -> &mut ProblemBody {
        match self {
            Self::Validation(body)
            | Self::MissingTargetHost(body)
            | Self::InvalidTargetHost(body)
            | Self::UnknownTargetHost(body)
            | Self::AuthenticationFailed(body)
            | Self::RouteNotFound(body)
            | Self::PluginInUse(body)
            | Self::PayloadTooLarge(body)
            | Self::RateLimitExceeded(body)
            | Self::SecretNotFound(body)
            | Self::ProtocolError(body)
            | Self::DownstreamError(body)
            | Self::StreamAborted(body)
            | Self::LinkUnavailable(body)
            | Self::CircuitBreakerOpen(body)
            | Self::PluginNotFound(body)
            | Self::ConnectionTimeout(body)
            | Self::RequestTimeout(body)
            | Self::IdleTimeout(body)
            | Self::Conflict(body)
            | Self::NotFound(body)
            | Self::Forbidden(body)
            | Self::BindForbidden(body) => body,
        }
    }

    /// Full GTS error type id for this variant, e.g.
    /// `gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1`.
    #[must_use]
    pub fn gts_type(&self) -> String {
        self.problem_body().r#type.clone()
    }

    /// HTTP status this variant renders as.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        StatusCode::from_u16(self.problem_body().status)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// Human-readable failure detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.problem_body().detail
    }

    /// Extension fields carried by this error.
    #[must_use]
    pub fn context(&self) -> &ProblemContext {
        &self.problem_body().context
    }

    /// Sets the `alias` extension field.
    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.problem_body_mut().context.alias = Some(alias.into());
        self
    }

    /// Sets the `host` extension field.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.problem_body_mut().context.host = Some(host.into());
        self
    }

    /// Sets the `path` extension field, which also becomes the problem
    /// `instance`.
    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        let body = self.problem_body_mut();
        body.context.path = Some(path.clone());
        body.instance = Some(path);
        self
    }

    /// Sets the `upstream_id` extension field to the GTS instance id of `id`.
    #[must_use]
    pub fn with_upstream_id(mut self, id: uuid::Uuid) -> Self {
        self.problem_body_mut().context.upstream_id = Some(id.to_string());
        self
    }

    /// Sets the `valid_hosts` extension field.
    #[must_use]
    pub fn with_valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.problem_body_mut().context.valid_hosts = hosts;
        self
    }

    /// Sets the `invalid_value` extension field.
    #[must_use]
    pub fn with_invalid_value(mut self, value: impl Into<String>) -> Self {
        self.problem_body_mut().context.invalid_value = Some(value.into());
        self
    }

    /// Sets the `plugin_id` extension field.
    #[must_use]
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.problem_body_mut().context.plugin_id = Some(plugin_id.into());
        self
    }

    /// Sets the `retry_after_seconds` extension field, which also becomes the
    /// `Retry-After` response header.
    #[must_use]
    pub fn with_retry_after_seconds(mut self, seconds: u64) -> Self {
        self.problem_body_mut().context.retry_after_seconds = Some(seconds);
        self
    }

    /// Sets the `referenced_by` extension field.
    #[must_use]
    pub fn with_referenced_by(mut self, referenced_by: ReferencedBy) -> Self {
        self.problem_body_mut().context.referenced_by = Some(Box::new(referenced_by));
        self
    }

    /// Renders this error as an axum [`Response`].
    ///
    /// `source` becomes the `X-OAGW-Error-Source` header value (`gateway` for
    /// errors produced by this enum, `upstream` when the proxy engine maps a
    /// failing upstream response through this taxonomy).
    ///
    /// The `context` extension fields are mirrored to the top level of the body
    /// (ADR-0007) at this boundary — the only place a [`ProblemBody`] reaches
    /// the wire — and `context` itself is kept for the platform middleware (see
    /// the [`ProblemBody`] documentation).
    #[must_use]
    pub fn into_response_with_source(self, source: &str) -> Response {
        let status = self.status();
        let retry_after = self.context().retry_after_seconds;
        let mut body = self.problem_body().clone();
        body.apply_context_extensions();
        let mut response = (status, axum::Json(body)).into_response();
        let headers = response.headers_mut();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(APPLICATION_PROBLEM_JSON),
        );
        headers.insert(
            HeaderName::from_static(ERROR_SOURCE_HEADER),
            HeaderValue::from_str(source)
                .unwrap_or_else(|_| HeaderValue::from_static(ERROR_SOURCE_GATEWAY)),
        );
        if let Some(retry) = retry_after
            && let Ok(value) = HeaderValue::from_str(&retry.to_string())
        {
            headers.insert(axum::http::header::RETRY_AFTER, value);
        }
        response
    }
}

impl fmt::Display for OagwError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.detail(), self.gts_type())
    }
}

impl std::error::Error for OagwError {}

impl From<OagwError> for Response {
    fn from(error: OagwError) -> Self {
        error.into_response_with_source(ERROR_SOURCE_GATEWAY)
    }
}

impl IntoResponse for OagwError {
    fn into_response(self) -> Response {
        Response::from(self)
    }
}

/// Creates a [`ProblemContext`] extension payload.
#[must_use]
pub fn problem_context() -> ProblemContextBuilder {
    ProblemContextBuilder::default()
}

/// Builds a [`ProblemContext`] extension payload fluently.
#[derive(Debug, Clone, Default)]
pub struct ProblemContextBuilder {
    context: ProblemContext,
}

impl ProblemContextBuilder {
    /// Sets `alias`.
    #[must_use]
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.context.alias = Some(alias.into());
        self
    }

    /// Sets `upstream_id`.
    #[must_use]
    pub fn upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.context.upstream_id = Some(upstream_id.into());
        self
    }

    /// Sets `host`.
    #[must_use]
    pub fn host(mut self, host: impl Into<String>) -> Self {
        self.context.host = Some(host.into());
        self
    }

    /// Sets `path`.
    #[must_use]
    pub fn path(mut self, path: impl Into<String>) -> Self {
        self.context.path = Some(path.into());
        self
    }

    /// Sets `valid_hosts`.
    #[must_use]
    pub fn valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.context.valid_hosts = hosts;
        self
    }

    /// Sets `invalid_value`.
    #[must_use]
    pub fn invalid_value(mut self, value: impl Into<String>) -> Self {
        self.context.invalid_value = Some(value.into());
        self
    }

    /// Sets `plugin_id`.
    #[must_use]
    pub fn plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.context.plugin_id = Some(plugin_id.into());
        self
    }

    /// Sets `retry_after_seconds`.
    #[must_use]
    pub fn retry_after_seconds(mut self, seconds: u64) -> Self {
        self.context.retry_after_seconds = Some(seconds);
        self
    }

    /// Sets `referenced_by`.
    #[must_use]
    pub fn referenced_by(mut self, referenced_by: ReferencedBy) -> Self {
        self.context.referenced_by = Some(Box::new(referenced_by));
        self
    }

    /// Consumes the builder, returning the populated context.
    #[must_use]
    pub fn build(self) -> ProblemContext {
        self.context
    }
}

macro_rules! oagw_error_constructors {
    ($($constructor:ident => $variant:ident, $spec:ident),+ $(,)?) => {
        impl OagwError {
            $(
                #[doc = concat!("Builds [`OagwError::", stringify!($variant), "`] with a detail message.")]
                #[must_use]
                pub fn $constructor(detail: impl Into<String>) -> Self {
                    Self::$variant(Box::new($spec.body(detail.into())))
                }
            )+
        }
    };
}

oagw_error_constructors! {
    validation => Validation, VALIDATION,
    missing_target_host => MissingTargetHost, MISSING_TARGET_HOST,
    invalid_target_host => InvalidTargetHost, INVALID_TARGET_HOST,
    unknown_target_host => UnknownTargetHost, UNKNOWN_TARGET_HOST,
    authentication_failed => AuthenticationFailed, AUTHENTICATION_FAILED,
    route_not_found => RouteNotFound, ROUTE_NOT_FOUND,
    plugin_in_use => PluginInUse, PLUGIN_IN_USE,
    payload_too_large => PayloadTooLarge, PAYLOAD_TOO_LARGE,
    rate_limit_exceeded => RateLimitExceeded, RATE_LIMIT_EXCEEDED,
    secret_not_found => SecretNotFound, SECRET_NOT_FOUND,
    protocol_error => ProtocolError, PROTOCOL_ERROR,
    downstream_error => DownstreamError, DOWNSTREAM_ERROR,
    stream_aborted => StreamAborted, STREAM_ABORTED,
    link_unavailable => LinkUnavailable, LINK_UNAVAILABLE,
    circuit_breaker_open => CircuitBreakerOpen, CIRCUIT_BREAKER_OPEN,
    plugin_not_found => PluginNotFound, PLUGIN_NOT_FOUND,
    connection_timeout => ConnectionTimeout, CONNECTION_TIMEOUT,
    request_timeout => RequestTimeout, REQUEST_TIMEOUT,
    idle_timeout => IdleTimeout, IDLE_TIMEOUT,
    conflict => Conflict, CONFLICT,
    not_found => NotFound, NOT_FOUND,
    forbidden => Forbidden, FORBIDDEN,
    bind_forbidden => BindForbidden, BIND_FORBIDDEN,
}

#[cfg(test)]
#[path = "../error_tests.rs"]
mod tests;
