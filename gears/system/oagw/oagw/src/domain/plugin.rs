//! Plugin traits and execution contexts (ADR-0002).
//!
//! Three traits with distinct semantics and a deterministic order:
//! `Auth → Guards → Transform(on_request) → upstream → Transform(on_response
//! | on_error)`. Upstream-level bindings always run before route-level ones.

use async_trait::async_trait;
use http::{HeaderMap, Method, StatusCode};
use serde_json::{Map, Value};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::PluginError;

/// Mutable request state handed to auth and transform plugins.
#[derive(Debug)]
pub struct RequestContext {
    /// Inbound method, forwarded verbatim.
    pub method: Method,
    /// Outbound path, already resolved from the route and path suffix.
    pub path: String,
    /// Outbound query parameters, in order.
    pub query: Vec<(String, String)>,
    /// Outbound headers built so far.
    pub headers: HeaderMap,
    /// Configuration of the plugin currently executing.
    pub config: Map<String, Value>,
    /// Caller identity.
    pub security_context: SecurityContext,
    /// Routing alias the request arrived on.
    pub alias: String,
    /// Resolved upstream id.
    pub upstream_id: Uuid,
    /// Matched route id, when a route matched.
    pub route_id: Option<Uuid>,
}

impl RequestContext {
    /// Read a plugin config value as a string, tolerating the first matching
    /// alias so operators are not tripped up by naming variants.
    #[must_use]
    pub fn config_str(&self, keys: &[&str]) -> Option<&str> {
        keys.iter()
            .find_map(|key| self.config.get(*key).and_then(Value::as_str))
    }

    /// Replace or insert an outbound header.
    ///
    /// # Errors
    ///
    /// Returns a plugin error when the name or value is not a legal header.
    pub fn set_header(&mut self, name: &str, value: &str) -> Result<(), PluginError> {
        let name: http::HeaderName = name
            .parse()
            .map_err(|_| PluginError::Config(format!("invalid header name '{name}'")))?;
        let value = http::HeaderValue::from_str(value)
            .map_err(|_| PluginError::Config(format!("invalid value for header '{name}'")))?;
        self.headers.insert(name, value);
        Ok(())
    }

    /// Append an outbound query parameter.
    pub fn add_query(&mut self, name: &str, value: &str) {
        self.query.push((name.to_owned(), value.to_owned()));
    }
}

/// Mutable response state handed to guard and transform plugins.
#[derive(Debug)]
pub struct ResponseContext {
    /// Upstream status.
    pub status: StatusCode,
    /// Response headers on their way to the client.
    pub headers: HeaderMap,
    /// Configuration of the plugin currently executing.
    pub config: Map<String, Value>,
    /// Correlation id of the request, when one is known.
    pub request_id: Option<String>,
}

/// State handed to transform plugins when the upstream call failed.
#[derive(Debug)]
pub struct ErrorContext {
    /// GTS error type identifier.
    pub error_type: String,
    /// Status OAGW is about to return.
    pub status: StatusCode,
    /// Headers on their way to the client.
    pub headers: HeaderMap,
    /// Configuration of the plugin currently executing.
    pub config: Map<String, Value>,
}

/// Outcome of a guard evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop and answer the client.
    Reject {
        /// HTTP status to answer with.
        status: StatusCode,
        /// Stable machine-readable code.
        error_code: String,
        /// Human-readable explanation.
        message: String,
    },
}

impl GuardDecision {
    /// Build a rejection.
    #[must_use]
    pub fn reject(status: StatusCode, error_code: &str, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code: error_code.to_owned(),
            message: message.into(),
        }
    }
}

/// Credential injection. One per upstream, executed before guards.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short, stable plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Inject credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the configuration is unusable or the
    /// credential cannot be obtained.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
}

/// Validation and policy enforcement; may reject a request or a response.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short, stable plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Evaluate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the guard cannot reach a decision.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    /// Evaluate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the guard cannot reach a decision.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Request/response/error mutation.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short, stable plugin name.
    fn id(&self) -> &str;
    /// Full GTS plugin identifier.
    fn plugin_type(&self) -> &str;
    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the transformation cannot be applied.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
    /// Mutate the upstream response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the transformation cannot be applied.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;
    /// Mutate a gateway error response.
    ///
    /// # Errors
    ///
    /// Returns a [`PluginError`] when the transformation cannot be applied.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}

#[cfg(test)]
mod tests {
    use super::{GuardDecision, RequestContext};
    use http::{HeaderMap, Method, StatusCode};
    use serde_json::json;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: Method::GET,
            path: "/v1/models".to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            config: config.as_object().cloned().unwrap_or_default(),
            security_context: SecurityContext::anonymous(),
            alias: "api.openai.com".to_owned(),
            upstream_id: Uuid::nil(),
            route_id: None,
        }
    }

    #[test]
    fn config_str_walks_aliases_in_order() {
        let c = ctx(json!({ "name": "X-Api-Key" }));
        assert_eq!(c.config_str(&["header_name", "name"]), Some("X-Api-Key"));
        assert_eq!(c.config_str(&["missing"]), None);
    }

    #[test]
    fn set_header_rejects_illegal_names() {
        let mut c = ctx(json!({}));
        c.set_header("x-api-key", "secret").expect("legal");
        assert_eq!(c.headers["x-api-key"], "secret");
        assert!(c.set_header("bad header", "v").is_err());
        assert!(c.set_header("x-ok", "bad\nvalue").is_err());
    }

    #[test]
    fn guard_rejection_carries_status_and_code() {
        let decision = GuardDecision::reject(
            StatusCode::BAD_REQUEST,
            "REQUIRED_HEADER_MISSING",
            "missing x-correlation-id",
        );
        match decision {
            GuardDecision::Reject {
                status, error_code, ..
            } => {
                assert_eq!(status, StatusCode::BAD_REQUEST);
                assert_eq!(error_code, "REQUIRED_HEADER_MISSING");
            }
            GuardDecision::Allow => panic!("expected a rejection"),
        }
    }
}
