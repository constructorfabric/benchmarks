//! Plugin execution contexts.
//!
//! One [`RequestContext`] travels through the whole proxy hop: the auth phase
//! mutates it, guards read it, transform plugins mutate it again. The
//! request/response bodies are optional on purpose — the data plane keeps
//! streaming bodies unbuffered, and a plugin that needs bytes sees
//! [`RequestContext::body`] as `None` for those.
//!
//! The types here use the canonical `http` header map. That is not a transport
//! dependency: the data plane (and the plugins) need a canonical, case-handling
//! header collection, and `http::HeaderMap` is the platform's.
use std::collections::HashMap;

use http::HeaderMap;
use serde_json::Value;
use uuid::Uuid;

/// Error code reported by the [`crate::domain::plugin::GuardPlugin`] phases
/// when a configured header is missing (`docs/ADR/0009`).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Everything a plugin knows about the request it is deciding on.
#[derive(Debug, Clone)]
pub struct RequestContext {
    /// HTTP method, upper-case (`GET`, `POST`, ...).
    pub method: String,
    /// Upstream-relative request path; always starts with `/`.
    pub path: String,
    /// Raw query string, without the leading `?`.
    pub query: Option<String>,
    /// Headers that will reach the upstream.
    pub headers: HeaderMap,
    /// Buffered body, when the transport buffered one.
    pub body: Option<bytes::Bytes>,
    /// `true` when the client asked for a protocol upgrade (WebSocket).
    pub upgrade: bool,
    /// Effective plugin configuration (route config layered over the
    /// upstream's).
    pub config: Value,
    /// Scratch space handed from one phase to the next.
    pub attributes: PluginAttributes,
    /// Tenant of the caller.
    pub tenant_id: Uuid,
    /// Resolved upstream.
    pub upstream_id: Uuid,
    /// Matched route, when the request matched one.
    pub route_id: Option<Uuid>,
    /// Calling principal, as established by the platform's security layer.
    pub subject_id: String,
    /// Subject tenant as established by the platform's security layer.
    pub subject_tenant_id: String,
}

impl RequestContext {
    /// A context with no headers, no body and an empty configuration.
    #[must_use]
    pub fn new(
        method: impl Into<String>,
        path: impl Into<String>,
        query: Option<String>,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Self {
        Self {
            method: method.into(),
            path: path.into(),
            query,
            headers: HeaderMap::new(),
            body: None,
            upgrade: false,
            config: Value::Null,
            attributes: PluginAttributes::default(),
            tenant_id,
            upstream_id,
            route_id: None,
            subject_id: String::new(),
            subject_tenant_id: tenant_id.to_string(),
        }
    }

    /// First value of a header, compared case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        http::HeaderName::try_from(name)
            .ok()
            .and_then(|name| self.headers.get(name))
            .and_then(|value| value.to_str().ok())
    }

    /// Replace (or add) a header value.
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        let Ok(name) = http::HeaderName::try_from(name) else {
            return;
        };
        let value: String = value.into();
        let Ok(value) = http::HeaderValue::try_from(value.as_str()) else {
            return;
        };
        self.headers.insert(name, value);
    }

    /// Append a header value without dropping the existing ones.
    pub fn add_header(&mut self, name: &str, value: impl Into<String>) {
        let Ok(name) = http::HeaderName::try_from(name) else {
            return;
        };
        let value: String = value.into();
        let Ok(value) = http::HeaderValue::try_from(value.as_str()) else {
            return;
        };
        self.headers.append(name, value);
    }

    /// Drop every value of a header.
    pub fn remove_header(&mut self, name: &str) {
        if let Ok(name) = http::HeaderName::try_from(name) {
            self.headers.remove(name);
        }
    }

    /// String-valued attribute, as set by an earlier plugin phase.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get_str(key)
    }

    /// Record a string attribute for later phases.
    pub fn set_attribute(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.attributes.set_str(key, value);
    }

    /// Replace the whole query string.
    pub fn set_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        self.query = Some(query).filter(|query| !query.is_empty());
    }

    /// A plugin rejection that stops the hop.
    #[must_use]
    pub fn reject(
        &self,
        status: u16,
        code: impl Into<String>,
        detail: impl Into<String>,
    ) -> PluginError {
        PluginError::new(status, code, detail)
    }

    /// Configuration value for a key of the effective plugin configuration.
    #[must_use]
    pub fn config_value(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }

    /// Configuration string for a key.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }

    /// Configuration string for a key, or a fallback.
    #[must_use]
    pub fn config_str_or<'a>(&'a self, key: &str, fallback: &'a str) -> &'a str {
        self.config_str(key).unwrap_or(fallback)
    }

    /// Configuration boolean for a key, or a fallback.
    #[must_use]
    pub fn config_bool_or(&self, key: &str, fallback: bool) -> bool {
        self.config
            .get(key)
            .and_then(Value::as_bool)
            .unwrap_or(fallback)
    }
}

/// The auth phase's view of the request (`docs/ADR/0008` names it
/// `AuthContext`; it is the same object the guard and transform phases see).
pub type AuthContext = RequestContext;

/// An upstream response, as seen by the guard and transform phases.
#[derive(Debug, Clone)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: u16,
    /// Upstream response headers, before the gateway adds its own.
    pub headers: HeaderMap,
    /// Buffered body, when the transport buffered one.
    pub body: Option<bytes::Bytes>,
    /// `true` when the body is being streamed and was therefore not buffered.
    pub streaming: bool,
    /// Effective plugin configuration of the binding under execution.
    pub config: Value,
    /// Attributes carried over from the request phases.
    pub attributes: PluginAttributes,
}

impl ResponseContext {
    /// A response with no body and no headers.
    #[must_use]
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: HeaderMap::new(),
            body: None,
            streaming: false,
            config: Value::Null,
            attributes: PluginAttributes::default(),
        }
    }

    /// Configuration string for a key of the binding's configuration.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }

    /// First value of a header, compared case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        http::HeaderName::try_from(name)
            .ok()
            .and_then(|name| self.headers.get(name))
            .and_then(|value| value.to_str().ok())
    }

    /// Whether the response carries a header, compared case-insensitively.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        http::HeaderName::try_from(name).is_ok_and(|name| self.headers.contains_key(name))
    }
}

/// A failure the proxy turns into a problem document.
#[derive(Debug, Clone)]
pub struct ErrorContext {
    /// Status the gateway will return.
    pub status: u16,
    /// GTS error identifier the gateway will report.
    pub code: String,
    /// Opaque wire detail.
    pub detail: String,
    /// Response headers the gateway will send alongside the problem.
    pub headers: HeaderMap,
    /// Upstream status, when the failure came from an upstream exchange.
    pub upstream_status: Option<u16>,
    /// Attributes carried over from the request phases.
    pub attributes: PluginAttributes,
}

impl ErrorContext {
    /// A gateway-side failure with no upstream contribution.
    #[must_use]
    pub fn new(status: u16, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            detail: detail.into(),
            headers: HeaderMap::new(),
            upstream_status: None,
            attributes: PluginAttributes::default(),
        }
    }
}

/// A guard's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// The hop continues.
    Allow,
    /// The hop stops with a problem document.
    Reject {
        /// Status the gateway returns.
        status: u16,
        /// GTS error identifier.
        code: String,
        /// Opaque wire detail.
        detail: String,
    },
}

impl GuardDecision {
    /// Continue the hop.
    #[must_use]
    pub const fn allow() -> Self {
        Self::Allow
    }

    /// Stop the hop with a problem document.
    #[must_use]
    pub fn reject(status: u16, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Reject {
            status,
            code: code.into(),
            detail: detail.into(),
        }
    }

    /// Whether the hop continues.
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// A plugin failure, carrying the wire shape the proxy must render.
#[derive(Debug, Clone)]
pub struct PluginError {
    /// Status the gateway returns.
    pub status: u16,
    /// GTS error identifier.
    pub code: String,
    /// Opaque wire detail.
    pub detail: String,
}

impl PluginError {
    /// An error with an explicit status and GTS identifier.
    #[must_use]
    pub fn new(status: u16, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            detail: detail.into(),
        }
    }

    /// A 500 the plugin could not classify.
    #[must_use]
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(
            500,
            crate::domain::plugin::problem_type(crate::domain::plugin::INTERNAL),
            detail,
        )
    }

    /// A 400 the plugin raised while validating the request.
    #[must_use]
    pub fn bad_request(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(400, code, detail)
    }
}

/// Free-form scratch space handed from one plugin phase to the next.
#[derive(Debug, Clone, Default)]
pub struct PluginAttributes(HashMap<String, Value>);

impl PluginAttributes {
    /// Whether a key is present.
    #[must_use]
    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// String value of a key.
    #[must_use]
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// Record a string value.
    pub fn set_str(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.0.insert(key.into(), Value::from(value.into()));
    }

    /// Remove a key.
    pub fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }
}
