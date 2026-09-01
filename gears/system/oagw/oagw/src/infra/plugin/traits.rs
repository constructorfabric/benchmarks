// Created: 2026-08-31 by Constructor Tech
//! Plugin contracts of ADR-0002 and the per-request context they run against.
//!
//! Three families, in execution order (DESIGN §3.2 "Plugin System"):
//! `AuthPlugin` → `GuardPlugin` → `TransformPlugin`, with the upstream call
//! between the request and the response halves of the last two.
//!
//! # Security boundary
//!
//! A context is the *surface a plugin may touch*. It deliberately exposes the
//! outbound headers and the query string but **no** request or response body,
//! and no log sink: a plugin cannot print what it is not handed. Nothing that
//! holds resolved credential material derives `Debug`; the contexts only ever
//! carry header *maps*, and their `Debug` impls redact the header values so a
//! credential injected into `ctx.headers` cannot leak through a log line or a
//! panic message (PRD `cpt-cf-oagw-fr-auth-injection`).

use std::fmt;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, Method, StatusCode};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::OagwError;

/// Contract of credential injection (ADR-0002 "Plugin Traits").
///
/// One `AuthPlugin` per upstream, executed once per request, before the guards.
/// The resolved credential is written into
/// [`RequestContext::headers`] (or the query string) and is never returned to
/// the caller of the trait.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short name of the plugin (`noop`, `apikey`, …).
    fn id(&self) -> &'static str;

    /// Full GTS id the registry is keyed by.
    fn plugin_type(&self) -> &'static str;

    /// Inject the credentials of one request.
    ///
    /// # Errors
    /// A failure of the credential store, of the credential source or of the
    /// binding itself, mapped onto the DESIGN §3.3 error table.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

/// Contract of request / response policy enforcement (ADR-0002).
///
/// A guard sees the request and the response and may reject either. Guards run
/// after auth and before the transforms.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short name of the plugin.
    fn id(&self) -> &'static str;

    /// Full GTS id the registry is keyed by.
    fn plugin_type(&self) -> &'static str;

    /// Validate the outbound request.
    ///
    /// # Errors
    /// Only a failure of the check itself; a *policy* rejection is
    /// [`GuardDecision::Reject`], not an error.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError>;

    /// Validate the upstream response.
    ///
    /// # Errors
    /// Only a failure of the check itself; a *policy* rejection is
    /// [`GuardDecision::Reject`], not an error.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError>;
}

/// Contract of request / response mutation (ADR-0002).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short name of the plugin.
    fn id(&self) -> &'static str;

    /// Full GTS id the registry is keyed by.
    fn plugin_type(&self) -> &'static str;

    /// Mutate the outbound request.
    ///
    /// # Errors
    /// A failure of the transformation, mapped onto the DESIGN §3.3 table.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;

    /// Mutate the response before it is streamed back.
    ///
    /// # Errors
    /// A failure of the transformation, mapped onto the DESIGN §3.3 table.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), OagwError>;

    /// Mutate a gateway error before it is rendered.
    ///
    /// # Errors
    /// A failure of the transformation, mapped onto the DESIGN §3.3 table.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), OagwError>;
}

/// Outcome of a guard phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The phase found nothing to object to.
    Allow,
    /// The phase rejects the request or the response.
    Reject(Rejection),
}

/// A guard rejection: the status to answer with and the problem behind it.
///
/// The status mirrors the `OagwErrorKind` of `error`, so a caller may use
/// either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// Status the request or response is rejected with.
    pub status: StatusCode,
    /// Problem document the rejection is rendered as.
    pub error: OagwError,
}

impl Rejection {
    /// Reject with `error`; the status follows its kind.
    #[must_use]
    pub fn new(error: OagwError) -> Self {
        let status = StatusCode::from_u16(error.kind().status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        Self { status, error }
    }

    /// The problem document of the rejection.
    #[must_use]
    pub fn into_error(self) -> OagwError {
        self.error
    }
}

/// The upstream a proxied request is served by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamRef {
    /// Upstream record id.
    pub id: Uuid,
    /// Routing key the request was addressed with.
    pub alias: String,
}

/// Configuration of the plugin that is running.
///
/// A view over the `config` member of a `plugins.items[]` binding, or over the
/// raw members of an `auth` binding. It is deliberately opaque: the control
/// plane never interprets plugin configuration, and a plugin reads only the
/// members it knows.
#[derive(Clone, Default)]
pub struct PluginConfig(serde_json::Map<String, Value>);

impl PluginConfig {
    /// Configuration over `map`.
    #[must_use]
    pub fn new(map: serde_json::Map<String, Value>) -> Self {
        Self(map)
    }

    /// A binding without configuration.
    #[must_use]
    pub fn empty() -> Self {
        Self(serde_json::Map::new())
    }

    /// The raw value of a member.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }

    /// The string value of a member, tolerating a non-string by ignoring it.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        self.value(name).and_then(Value::as_str)
    }

    /// The member names the configuration carries.
    #[must_use]
    pub fn member_names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// Whether the configuration is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `Debug` that lists the member *names* only.
///
/// Configuration members may name credentials (a `cred://` reference is an
/// identifier, but an operator may still have pasted material the write path
/// rejected); listing names only keeps the value out of every log line.
impl fmt::Debug for PluginConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginConfig")
            .field("members", &self.member_names())
            .finish()
    }
}

/// Surface a request-side plugin sees.
///
/// `headers` is the **outbound** header set: the upstream header rules have
/// already been applied, so a plugin injects exactly what will be dialled
/// (DESIGN §3.2 pipeline: "Headers | Apply `upstream.headers` transformation
/// rules; plugin mutable").
pub struct RequestContext {
    /// Identity of the caller; the credential store and the token cache key
    /// are derived from it.
    pub security: SecurityContext,
    /// Upstream the request is forwarded to.
    pub upstream: UpstreamRef,
    /// Request method.
    pub method: Method,
    /// Outbound request headers, plugin mutable.
    pub headers: HeaderMap,
    /// Outbound query string, plugin mutable. Empty when the request carries
    /// no query.
    pub query: String,
    /// Configuration of the running plugin.
    pub config: PluginConfig,
}

impl RequestContext {
    /// The tenant the request is served for.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        self.security.subject_tenant_id()
    }

    /// The authenticated subject of the request.
    #[must_use]
    pub fn subject_id(&self) -> Uuid {
        self.security.subject_id()
    }
}

/// The identity of a request, reduced to its identifiers.
///
/// `SecurityContext` is identity material; only the two UUIDs a log line can
/// legitimately carry are exposed.
struct Subject<'a>(&'a SecurityContext);

impl fmt::Debug for Subject<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subject")
            .field("tenant", &self.0.subject_tenant_id())
            .field("subject", &self.0.subject_id())
            .finish()
    }
}

/// `Debug` that redacts every header value.
///
/// A credential plugin writes the resolved secret into `headers`; the header
/// *names* stay readable so a failing test still says which phase ran.
impl fmt::Debug for RequestContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestContext")
            .field("upstream", &self.upstream)
            .field("security", &Subject(&self.security))
            .field("method", &self.method)
            .field("headers", &HeaderNames(&self.headers))
            .field("query_empty", &self.query.is_empty())
            .field("config", &self.config)
            .finish()
    }
}

/// Surface a response-side plugin sees.
pub struct ResponseContext {
    /// Identity of the caller.
    pub security: SecurityContext,
    /// Upstream that produced the response.
    pub upstream: UpstreamRef,
    /// Status the upstream answered with.
    pub status: StatusCode,
    /// Headers bound for the client (the upstream response rules are already
    /// applied), plugin mutable.
    pub headers: HeaderMap,
    /// Headers the upstream answered with, read-only. A plugin that propagates
    /// an upstream value reads it here, because the response rules may have
    /// dropped it from `headers`.
    pub upstream_headers: HeaderMap,
    /// Configuration of the running plugin.
    pub config: PluginConfig,
}

impl ResponseContext {
    /// The tenant the request is served for.
    #[must_use]
    pub fn tenant_id(&self) -> Uuid {
        self.security.subject_tenant_id()
    }
}

/// `Debug` that redacts every header value.
impl fmt::Debug for ResponseContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResponseContext")
            .field("upstream", &self.upstream)
            .field("security", &Subject(&self.security))
            .field("status", &self.status)
            .field("headers", &HeaderNames(&self.headers))
            .field("upstream_headers", &HeaderNames(&self.upstream_headers))
            .field("config", &self.config)
            .finish()
    }
}

/// Surface an error-side plugin sees.
pub struct ErrorContext {
    /// Identity of the caller.
    pub security: SecurityContext,
    /// Upstream the request was addressed to.
    pub upstream: UpstreamRef,
    /// Problem the gateway is about to render, plugin mutable.
    pub error: OagwError,
    /// Configuration of the running plugin.
    pub config: PluginConfig,
}

/// `Debug` that keeps the problem detail but names no header value.
impl fmt::Debug for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ErrorContext")
            .field("upstream", &self.upstream)
            .field("security", &Subject(&self.security))
            .field("error", &self.error)
            .field("config", &self.config)
            .finish()
    }
}

/// Header *names* of a map, for a redacting `Debug` impl.
struct HeaderNames<'a>(&'a HeaderMap);

impl fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.keys().map(HeaderName::to_string))
            .finish()
    }
}
