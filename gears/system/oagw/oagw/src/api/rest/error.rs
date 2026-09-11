//! RFC 9457 problem details for every gateway-produced error.
//!
//! Every gateway error is `application/problem+json` with a GTS `type`
//! identifier and `X-OAGW-Error-Source: gateway` (ADR 0007). Upstream
//! responses — success or failure — are returned verbatim with
//! `X-OAGW-Error-Source: upstream` and never go through this type.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::domain::error::DomainError;
use crate::domain::gts_helpers as gts;

/// Extension fields OAGW adds on top of the RFC 9457 envelope (ADR 0007
/// Appendix A).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OagwProblem {
    /// GTS error identifier.
    pub type_id: String,
    /// Human-readable summary.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Occurrence-specific explanation.
    pub detail: String,
    /// Request path.
    pub instance: Option<String>,
    /// Resolved upstream id.
    pub upstream_id: Option<String>,
    /// Target host.
    pub host: Option<String>,
    /// Request path forwarded upstream.
    pub path: Option<String>,
    /// Resolved alias.
    pub alias: Option<String>,
    /// Endpoint hosts that could have been selected.
    pub valid_hosts: Vec<String>,
    /// The rejected input value.
    pub invalid_value: Option<String>,
    /// Retry hint in seconds.
    pub retry_after_seconds: Option<u64>,
    /// Correlation id.
    pub trace_id: Option<String>,
    /// Plugin that produced the failure.
    pub plugin_id: Option<String>,
    /// References held to a plugin.
    pub referenced_by: Option<crate::domain::error::ReferencedBy>,
    /// Whether the gateway produced the error (`gateway` / `upstream`).
    pub source: &'static str,
}

impl OagwProblem {
    /// Builds a problem from an error-type suffix, a status and a detail.
    pub fn new(type_suffix: &str, status: u16, title: &str, detail: impl Into<String>) -> Self {
        Self {
            type_id: gts::error_gts_id(type_suffix),
            title: title.to_string(),
            status,
            detail: detail.into(),
            source: "gateway",
            ..OagwProblem::default()
        }
    }

    /// Sets the request instance.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Sets the correlation id.
    pub fn with_trace(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// Sets the resolved upstream id.
    pub fn with_upstream_id(mut self, upstream_id: impl Into<String>) -> Self {
        self.upstream_id = Some(upstream_id.into());
        self
    }

    /// Sets the target host.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Sets the alias.
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    /// Sets the forwarded path.
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Sets the valid endpoint hosts.
    pub fn with_valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.valid_hosts = hosts;
        self
    }

    /// Sets the rejected input value.
    pub fn with_invalid_value(mut self, value: impl Into<String>) -> Self {
        self.invalid_value = Some(value.into());
        self
    }

    /// Sets the retry hint.
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    /// Sets the failing plugin id.
    pub fn with_plugin_id(mut self, plugin_id: impl Into<String>) -> Self {
        self.plugin_id = Some(plugin_id.into());
        self
    }

    /// Sets the plugin reference report.
    pub fn with_referenced_by(mut self, referenced_by: crate::domain::error::ReferencedBy) -> Self {
        self.referenced_by = Some(referenced_by);
        self
    }

    /// Overrides the error source (`gateway` for gateway-produced errors).
    pub fn with_source(mut self, source: &'static str) -> Self {
        self.source = source;
        self
    }

    /// Serialises the problem to its JSON body.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), Value::String(self.type_id.clone()));
        m.insert("title".into(), Value::String(self.title.clone()));
        m.insert("status".into(), Value::from(self.status));
        if !self.detail.is_empty() {
            m.insert("detail".into(), Value::String(self.detail.clone()));
        }
        if let Some(v) = &self.instance {
            m.insert("instance".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.upstream_id {
            m.insert("upstream_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.host {
            m.insert("host".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.path {
            m.insert("path".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.alias {
            m.insert("alias".into(), Value::String(v.clone()));
        }
        if !self.valid_hosts.is_empty() {
            m.insert("valid_hosts".into(), Value::Array(
                self.valid_hosts.iter().cloned().map(Value::String).collect(),
            ));
        }
        if let Some(v) = &self.invalid_value {
            m.insert("invalid_value".into(), Value::String(v.clone()));
        }
        if let Some(v) = self.retry_after_seconds {
            m.insert("retry_after_seconds".into(), Value::from(v));
        }
        if let Some(v) = &self.trace_id {
            m.insert("trace_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.plugin_id {
            m.insert("plugin_id".into(), Value::String(v.clone()));
        }
        if let Some(v) = &self.referenced_by {
            m.insert(
                "referenced_by".into(),
                serde_json::to_value(v).unwrap_or(Value::Null),
            );
        }
        Value::Object(m)
    }

    /// The HTTP status of the problem.
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

impl IntoResponse for OagwProblem {
    fn into_response(self) -> Response {
        let mut builder = Response::builder()
            .status(self.status_code())
            .header(header::CONTENT_TYPE, HeaderValue::from_static("application/problem+json"))
            .header(
                "X-OAGW-Error-Source",
                HeaderValue::from_static(if self.source.is_empty() { "gateway" } else { self.source }),
            );

        if let Some(secs) = self.retry_after_seconds {
            builder = builder.header(header::RETRY_AFTER, HeaderValue::from(secs));
        }

        let body = serde_json::to_vec(&self.to_json()).unwrap_or_else(|_| {
            br#"{"type":"gts.cf.core.errors.err.v1~cf.oagw.internal.v1","title":"Internal Error","status":500}"#.to_vec()
        });

        builder
            .body(axum::body::Body::from(body))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    }
}

/// Builds a CORS-origin rejection (ADR 0004).
pub fn cors_origin_not_allowed(origin: &str, instance: &str) -> OagwProblem {
    OagwProblem::new(
        "cors.origin_not_allowed.v1",
        403,
        "CORS Origin Not Allowed",
        format!("Origin `{origin}` is not allowed by the upstream CORS policy"),
    )
    .with_instance(instance)
    .with_invalid_value(origin)
}

/// Builds a CORS-method rejection (ADR 0004).
pub fn cors_method_not_allowed(method: &str, instance: &str) -> OagwProblem {
    OagwProblem::new(
        "cors.method_not_allowed.v1",
        403,
        "CORS Method Not Allowed",
        format!("Method `{method}` is not allowed by the upstream CORS policy"),
    )
    .with_instance(instance)
    .with_invalid_value(method)
}

/// Maps a [`DomainError`] onto the documented problem body.
///
/// The table in `specs/.../contracts/errors.md` (and DESIGN §"Error Response
/// Format") is authoritative: one row per variant, `X-OAGW-Error-Source:
/// gateway` for all of them.
pub fn problem_from_domain_error(err: &DomainError, instance: &str) -> OagwProblem {
    use DomainError as E;
    let base = |e: &DomainError| {
        OagwProblem::new(
            e.error_type_suffix(),
            http_status_of(e),
            e.title(),
            detail_of(e),
        )
        .with_instance(instance)
    };

    match err {
        E::ValidationError { detail } => base(err).with_detail(detail.clone()),
        E::MissingTargetHost { alias, valid_hosts, upstream_id } => base(err)
            .with_alias(alias.clone())
            .with_valid_hosts(valid_hosts.clone())
            .with_upstream_id(upstream_id.clone()),
        E::InvalidTargetHost { invalid_value } => {
            base(err).with_invalid_value(invalid_value.clone())
        }
        E::UnknownTargetHost { invalid_value, valid_hosts } => base(err)
            .with_invalid_value(invalid_value.clone())
            .with_valid_hosts(valid_hosts.clone()),
        E::AuthenticationFailed { plugin_id, .. } => {
            let p = base(err);
            match plugin_id {
                Some(id) => p.with_plugin_id(id.clone()),
                None => p,
            }
        }
        E::RouteNotFound { alias, host, path, .. } => {
            let mut p = base(err);
            if let Some(a) = alias {
                p = p.with_alias(a.clone());
            }
            if let Some(h) = host {
                p = p.with_host(h.clone());
            }
            if let Some(pp) = path {
                p = p.with_path(pp.clone());
            }
            p
        }
        E::PluginInUse { plugin_id, referenced_by } => {
            base(err).with_plugin_id(plugin_id.clone()).with_referenced_by(referenced_by.clone())
        }
        E::RateLimitExceeded { snapshot, host, path, upstream_id } => {
            let mut p = base(err)
                .with_retry_after(snapshot.retry_after)
                .with_invalid_value(format!("{}", snapshot.remaining));
            if let Some(h) = host {
                p = p.with_host(h.clone());
            }
            if let Some(pp) = path {
                p = p.with_path(pp.clone());
            }
            if let Some(u) = upstream_id {
                p = p.with_upstream_id(u.clone());
            }
            p
        }
        E::SecretNotFound { plugin_id, .. } => {
            let p = base(err);
            match plugin_id {
                Some(id) => p.with_plugin_id(id.clone()),
                None => p,
            }
        }
        E::LinkUnavailable { upstream_id, alias, .. } => {
            let mut p = base(err);
            if let Some(u) = upstream_id {
                p = p.with_upstream_id(u.clone());
            }
            if let Some(a) = alias {
                p = p.with_alias(a.clone());
            }
            p
        }
        E::CircuitBreakerOpen { host } => {
            let p = base(err);
            match host {
                Some(h) => p.with_host(h.clone()),
                None => p,
            }
        }
        E::PluginNotFound { plugin_id } => base(err).with_plugin_id(plugin_id.clone()),
        E::ServiceUnavailable { retry_after, .. } => {
            let p = base(err);
            match retry_after {
                Some(secs) => p.with_retry_after(*secs),
                None => p,
            }
        }
        E::ConnectionTimeout { host }
        | E::RequestTimeout { host }
        | E::IdleTimeout { host }
        | E::DownstreamError { host, .. }
        | E::ProtocolError { host, .. } => {
            let p = base(err);
            match host {
                Some(h) => p.with_host(h.clone()),
                None => p,
            }
        }
        // Everything else carries no extension fields beyond the envelope.
        _ => base(err),
    }
}

/// Replaces the problem `detail` (used by callers with richer context).
impl OagwProblem {
    /// Overrides the problem detail.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }
}

/// The documented HTTP status of a domain error.
pub fn http_status_of(err: &DomainError) -> u16 {
    use DomainError as E;
    match err {
        E::ValidationError { .. }
        | E::Extra { .. }
        | E::MissingTargetHost { .. }
        | E::InvalidTargetHost { .. }
        | E::UnknownTargetHost { .. } => 400,
        E::AuthenticationFailed { .. } => 401,
        E::PermissionDenied { .. } => 403,
        E::NotFound { .. } | E::RouteNotFound { .. } => 404,
        E::Conflict { .. } | E::PluginInUse { .. } => 409,
        E::PayloadTooLarge { .. } => 413,
        E::RateLimitExceeded { .. } => 429,
        E::SecretNotFound { .. } => 500,
        E::Internal { .. } => 500,
        E::ProtocolError { .. }
        | E::DownstreamError { .. }
        | E::StreamAborted { .. } => 502,
        E::LinkUnavailable { .. }
        | E::CircuitBreakerOpen { .. }
        | E::PluginNotFound { .. }
        | E::ServiceUnavailable { .. } => 503,
        E::ConnectionTimeout { .. }
        | E::RequestTimeout { .. }
        | E::IdleTimeout { .. } => 504,
    }
}

/// The occurrence-specific `detail` of a domain error.
fn detail_of(err: &DomainError) -> String {
    use DomainError as E;
    match err {
        E::ValidationError { detail }
        | E::AuthenticationFailed { detail, .. }
        | E::PermissionDenied { detail }
        | E::NotFound { detail }
        | E::Conflict { detail }
        | E::PayloadTooLarge { detail }
        | E::ServiceUnavailable { detail, .. }
        | E::StreamAborted { detail } => detail.clone(),
        E::MissingTargetHost { valid_hosts, .. } => format!(
            "X-OAGW-Target-Host header required for multi-endpoint upstream with common suffix alias. Valid hosts: {valid_hosts:?}"
        ),
        E::InvalidTargetHost { invalid_value } => format!(
            "X-OAGW-Target-Host header value `{invalid_value}` is not a hostname or IP address"
        ),
        E::UnknownTargetHost { invalid_value, valid_hosts } => format!(
            "X-OAGW-Target-Host header value `{invalid_value}` does not match any configured endpoint. Valid hosts: {valid_hosts:?}"
        ),
        E::RouteNotFound { detail, .. } => detail.clone(),
        E::PluginInUse { plugin_id, referenced_by } => format!(
            "plugin `{plugin_id}` is still referenced by {referenced_by:?}"
        ),
        E::RateLimitExceeded { snapshot, host, .. } => format!(
            "Rate limit exceeded for upstream {}",
            host.clone().unwrap_or_default()
        ) + &format!(" (limit {}, remaining {})", snapshot.limit, snapshot.remaining),
        E::SecretNotFound { detail, .. } => detail.clone(),
        E::ProtocolError { detail, .. } => detail.clone(),
        E::DownstreamError { status, .. } => format!("upstream returned status {status}"),
        E::LinkUnavailable { detail, .. } => detail.clone(),
        E::CircuitBreakerOpen { .. } => "circuit breaker is open for this upstream".to_string(),
        E::PluginNotFound { plugin_id } => {
            format!("plugin `{plugin_id}` could not be resolved")
        }
        E::ConnectionTimeout { .. } => "connection to the upstream timed out".to_string(),
        E::RequestTimeout { .. } => "the upstream did not answer in time".to_string(),
        E::IdleTimeout { .. } => "the upstream stream was idle for too long".to_string(),
        E::Internal { diagnostic } => diagnostic.clone(),
        E::Extra { detail, .. } => detail.clone(),
    }
}
