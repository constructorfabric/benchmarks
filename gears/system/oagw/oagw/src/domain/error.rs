//! The gear's error type and its mapping onto the problem-details contract.

use std::time::Duration;

use serde_json::{json, Value};


/// Everything the problem renderer needs beyond the standard RFC 9457 fields.
#[derive(Debug, Clone, Default)]
pub struct ProblemMeta {
    /// GTS id or alias of the upstream the failure relates to.
    pub upstream_id: Option<String>,
    /// Alias that was requested.
    pub alias: Option<String>,
    /// Request path that failed.
    pub path: Option<String>,
    /// Seconds the caller should wait before retrying.
    pub retry_after_seconds: Option<u64>,
    /// Endpoint hosts the caller may select with `X-OAGW-Target-Host`.
    pub valid_hosts: Option<Vec<String>>,
    /// The value the caller supplied that was rejected.
    pub invalid_value: Option<String>,
    /// Machine-readable error code (`REQUIRED_HEADER_MISSING`, …).
    pub code: Option<String>,
    /// Free-form additional context.
    pub extra: Vec<(String, Value)>,
}

impl ProblemMeta {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_upstream_id(mut self, id: impl Into<String>) -> Self {
        self.upstream_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_alias(mut self, alias: impl Into<String>) -> Self {
        self.alias = Some(alias.into());
        self
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_valid_hosts(mut self, hosts: Vec<String>) -> Self {
        self.valid_hosts = Some(hosts);
        self
    }

    #[must_use]
    pub fn with_invalid_value(mut self, value: impl Into<String>) -> Self {
        self.invalid_value = Some(value.into());
        self
    }

    #[must_use]
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    #[must_use]
    pub fn with_retry_after(mut self, seconds: u64) -> Self {
        self.retry_after_seconds = Some(seconds);
        self
    }

    #[must_use]
    pub fn with_extra(mut self, key: impl Into<String>, value: Value) -> Self {
        self.extra.push((key.into(), value));
        self
    }

    fn to_json(&self) -> Value {
        let mut obj = serde_json::Map::new();
        if let Some(v) = &self.upstream_id {
            obj.insert("upstream_id".to_string(), json!(v));
        }
        if let Some(v) = &self.alias {
            obj.insert("alias".to_string(), json!(v));
        }
        if let Some(v) = &self.path {
            obj.insert("path".to_string(), json!(v));
        }
        if let Some(v) = self.retry_after_seconds {
            obj.insert("retry_after_seconds".to_string(), json!(v));
        }
        if let Some(v) = &self.valid_hosts {
            obj.insert("valid_hosts".to_string(), json!(v));
        }
        if let Some(v) = &self.invalid_value {
            obj.insert("invalid_value".to_string(), json!(v));
        }
        if let Some(v) = &self.code {
            obj.insert("code".to_string(), json!(v));
        }
        for (k, v) in &self.extra {
            obj.insert(k.clone(), v.clone());
        }
        Value::Object(obj)
    }
}

/// Errors the gateway raises, already shaped for the wire.
#[derive(Debug, thiserror::Error)]
pub enum DomainError {
    /// Input or configuration failed validation (400).
    #[error("{0}")]
    Validation(String),
    /// No route matched the request (404).
    #[error("{0}")]
    RouteNotFound(String),
    /// The alias resolved to no upstream (404).
    #[error("{0}")]
    UpstreamNotFound(String),
    /// The referenced plugin does not exist (404).
    #[error("{0}")]
    PluginNotFound(String),
    /// The alias is already taken in the tenant (409).
    #[error("{0}")]
    AliasConflict(String),
    /// Another route already claims the match rule (409).
    #[error("{0}")]
    MatchConflict(String),
    /// The plugin is still referenced by an upstream or route (409).
    #[error("{0}")]
    PluginInUse(String),
    /// The upstream is disabled (503).
    #[error("{0}")]
    Disabled(String),
    /// Credential acquisition failed (401).
    #[error("{0}")]
    AuthFailed(String),
    /// A referenced secret could not be resolved (500).
    #[error("{0}")]
    SecretNotFound(String),
    /// The body exceeded the configured ceiling (413).
    #[error("{0}")]
    PayloadTooLarge(String),
    /// A rate limit was exhausted (429).
    #[error("{0}")]
    RateLimited(String),
    /// A guard rejected the request (400 by default).
    #[error("{message}")]
    Guard {
        /// Machine-readable code the guard rejected with (ADR-0009).
        code: String,
        /// Human explanation of the rejection.
        message: String,
    },
    /// A guard rejected the upstream response (502 by default).
    #[error("{message}")]
    ResponseGuard {
        /// Machine-readable code the guard rejected with.
        code: String,
        /// Human explanation of the rejection.
        message: String,
    },
    /// CORS refused the origin (403).
    #[error("{0}")]
    CorsOriginDenied(String),
    /// CORS refused the method (403).
    #[error("{0}")]
    CorsMethodDenied(String),
    /// Endpoint selection needs a target host (400).
    #[error("{0}")]
    TargetHostRequired(String),
    /// Endpoint selection got a malformed target host (400).
    #[error("{0}")]
    TargetHostInvalid(String),
    /// Endpoint selection got a host with no endpoint (400).
    #[error("{0}")]
    TargetHostUnknown(String),
    /// The upstream could not be reached (502).
    #[error("{0}")]
    Downstream(String),
    /// The upstream did not answer in time (504).
    #[error("{0}")]
    Timeout(String),
    /// The upstream answered with a protocol violation (502).
    #[error("{0}")]
    Protocol(String),
    /// A proxied stream was interrupted (502).
    #[error("{0}")]
    StreamAborted(String),
    /// The upstream is unreachable because its circuit is open (503).
    #[error("{0}")]
    CircuitOpen(String),
    /// The gear itself failed.
    #[error("{0}")]
    Internal(String),
}

impl DomainError {
    /// HTTP status the error maps to, per PRD §5.6.
    #[must_use]
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            Self::Validation(_)
            | Self::Guard { .. }
            | Self::TargetHostRequired(_)
            | Self::TargetHostInvalid(_)
            | Self::TargetHostUnknown(_) => StatusCode::BAD_REQUEST,
            Self::AuthFailed(_) => StatusCode::UNAUTHORIZED,
            Self::RouteNotFound(_) | Self::UpstreamNotFound(_) => StatusCode::NOT_FOUND,
            Self::AliasConflict(_)
            | Self::MatchConflict(_)
            | Self::PluginInUse(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited(_) => StatusCode::TOO_MANY_REQUESTS,
            Self::SecretNotFound(_) | Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            // The management surface answers 404 for a resource that is not there (DESIGN
            // §Management API, tenant-scoped CRUD); the 503 of the error table is the proxy-time
            // shape, which this release never raises.
            Self::PluginNotFound(_) => StatusCode::NOT_FOUND,
            Self::ResponseGuard { .. } | Self::Downstream(_) | Self::Protocol(_)
            | Self::StreamAborted(_) => StatusCode::BAD_GATEWAY,
            Self::Disabled(_) | Self::CircuitOpen(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            Self::CorsOriginDenied(_) | Self::CorsMethodDenied(_) => StatusCode::FORBIDDEN,
        }
    }

    /// GTS problem-type identifier the error maps to.
    #[must_use]
    pub fn problem_type(&self) -> &'static str {
        match self {
            Self::Validation(_) => super::gts_helpers::ERR_VALIDATION,
            Self::RouteNotFound(_) => super::gts_helpers::ERR_ROUTE_NOT_FOUND,
            Self::UpstreamNotFound(_) => super::gts_helpers::ERR_UPSTREAM_NOT_FOUND,
            Self::PluginNotFound(_) => super::gts_helpers::ERR_PLUGIN_NOT_FOUND,
            Self::PluginInUse(_) => super::gts_helpers::ERR_PLUGIN_IN_USE,
            Self::AliasConflict(_) | Self::MatchConflict(_) => super::gts_helpers::ERR_VALIDATION,
            Self::AuthFailed(_) => super::gts_helpers::ERR_AUTH_FAILED,
            Self::SecretNotFound(_) => super::gts_helpers::ERR_SECRET_NOT_FOUND,
            Self::PayloadTooLarge(_) => super::gts_helpers::ERR_PAYLOAD_TOO_LARGE,
            Self::RateLimited(_) => super::gts_helpers::ERR_RATE_LIMIT,
            Self::Guard { .. } | Self::ResponseGuard { .. } => super::gts_helpers::ERR_VALIDATION,
            Self::CorsOriginDenied(_) => super::gts_helpers::ERR_CORS_ORIGIN,
            Self::CorsMethodDenied(_) => super::gts_helpers::ERR_CORS_METHOD,
            Self::TargetHostRequired(_) => super::gts_helpers::ERR_MISSING_TARGET_HOST,
            Self::TargetHostInvalid(_) => super::gts_helpers::ERR_INVALID_TARGET_HOST,
            Self::TargetHostUnknown(_) => super::gts_helpers::ERR_UNKNOWN_TARGET_HOST,
            Self::Downstream(_) => super::gts_helpers::ERR_DOWNSTREAM,
            Self::Timeout(_) => super::gts_helpers::ERR_TIMEOUT,
            Self::Protocol(_) => super::gts_helpers::ERR_PROTOCOL,
            Self::StreamAborted(_) => super::gts_helpers::ERR_STREAM_ABORTED,
            Self::CircuitOpen(_) => super::gts_helpers::ERR_CIRCUIT_OPEN,
            Self::Disabled(_) | Self::Internal(_) => super::gts_helpers::ERR_DOWNSTREAM,
        }
    }

    /// Human-readable title the error maps to.
    #[must_use]
    pub fn title(&self) -> &'static str {
        match self {
            Self::Validation(_)
            | Self::AliasConflict(_)
            | Self::MatchConflict(_)
            | Self::Guard { .. }
            | Self::ResponseGuard { .. } => "Validation Error",
            Self::TargetHostRequired(_) => "Missing Target Host Header",
            Self::TargetHostInvalid(_) => "Invalid Target Host Format",
            Self::TargetHostUnknown(_) => "Unknown Target Host",
            Self::RouteNotFound(_) => "Route Not Found",
            Self::UpstreamNotFound(_) => "Upstream Not Found",
            Self::PluginNotFound(_) => "Plugin Not Found",
            Self::PluginInUse(_) => "Plugin In Use",
            Self::AuthFailed(_) => "Authentication Failed",
            Self::SecretNotFound(_) => "Secret Not Found",
            Self::PayloadTooLarge(_) => "Payload Too Large",
            Self::RateLimited(_) => "Rate Limit Exceeded",
            Self::CorsOriginDenied(_) | Self::CorsMethodDenied(_) => "CORS Not Allowed",
            Self::Downstream(_) | Self::Internal(_) => "Downstream Error",
            Self::Timeout(_) => "Timeout",
            Self::Protocol(_) => "Protocol Error",
            Self::StreamAborted(_) => "Stream Aborted",
            Self::CircuitOpen(_) => "Circuit Breaker Open",
            Self::Disabled(_) => "Upstream Disabled",
        }
    }

    /// Retriable errors carry `Retry-After` and the `retriable` extension field.
    #[must_use]
    pub fn retriable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Timeout(_))
    }

    /// Suggested retry delay, if the error carries one.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited(_) => Some(Duration::from_secs(1)),
            _ => None,
        }
    }

    /// Detail text of the problem body.
    #[must_use]
    pub fn detail(&self) -> String {
        self.to_string()
    }

    /// Extension fields the error itself contributes.
    #[must_use]
    pub fn meta(&self) -> ProblemMeta {
        let mut meta = ProblemMeta::new();
        if let Self::Guard { code, .. } | Self::ResponseGuard { code, .. } = self {
            meta.code = Some(code.clone());
        }
        if self.retriable() {
            meta.extra
                .push(("retriable".to_string(), Value::Bool(true)));
        }
        if let Some(d) = self.retry_after() {
            meta.retry_after_seconds = Some(d.as_secs());
        }
        meta
    }

    /// Extension fields merged in from the call site.
    #[must_use]
    pub fn meta_with(&self, extra: ProblemMeta) -> ProblemMeta {
        let mut merged = self.meta();
        if extra.upstream_id.is_some() {
            merged.upstream_id = extra.upstream_id;
        }
        if extra.alias.is_some() {
            merged.alias = extra.alias;
        }
        if extra.path.is_some() {
            merged.path = extra.path;
        }
        if extra.retry_after_seconds.is_some() {
            merged.retry_after_seconds = extra.retry_after_seconds;
        }
        if extra.valid_hosts.is_some() {
            merged.valid_hosts = extra.valid_hosts;
        }
        if extra.invalid_value.is_some() {
            merged.invalid_value = extra.invalid_value;
        }
        if extra.code.is_some() {
            merged.code = extra.code;
        }
        merged.extra.extend(extra.extra);
        merged
    }

    /// The extension fields as a JSON object, ready to embed in `context`.
    #[must_use]
    pub fn extensions_json(&self, extra: &ProblemMeta) -> Value {
        let mut base = self.meta().to_json();
        let Value::Object(map) = &mut base else {
            return Value::Null;
        };
        if let Value::Object(other) = extra.to_json() {
            for (k, v) in other {
                if !v.is_null() {
                    map.insert(k, v);
                }
            }
        }
        base
    }
}
