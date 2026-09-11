//! Plugin traits and the contexts they mutate (`ADR/0002-plugin-system.md`).
//!
//! Three traits, one per purpose, executed in a fixed order:
//!
//! ```text
//! Auth → Guards → Transform(on_request) → upstream call
//!      → Transform(on_response) | Transform(on_error)
//! ```
//!
//! Upstream bindings always run before route bindings.

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use toolkit_security::SecurityContext;

use super::error::DomainError;

/// Failure raised by a plugin.
#[derive(Debug, Clone)]
pub enum PluginError {
    /// The plugin refused the request for a policy reason.
    Rejected(DomainError),
    /// The plugin could not complete (dependency failure, bad config).
    Internal(String),
}

impl PluginError {
    /// Project onto the gateway's error type.
    #[must_use]
    pub fn into_domain(self, plugin_id: &str) -> DomainError {
        match self {
            Self::Rejected(err) => err,
            Self::Internal(detail) => DomainError::internal(format!(
                "plugin '{plugin_id}' failed: {detail}"
            )),
        }
    }
}

impl From<DomainError> for PluginError {
    fn from(value: DomainError) -> Self {
        Self::Rejected(value)
    }
}

/// Result alias for plugin entry points.
pub type PluginResult<T = ()> = Result<T, PluginError>;

/// Case-insensitive header bag shared by the plugin contexts.
///
/// Keys are stored lowercased so plugins never have to think about casing;
/// a single name may carry several values (`add` semantics).
#[derive(Debug, Clone, Default)]
pub struct HeaderBag(BTreeMap<String, Vec<String>>);

impl HeaderBag {
    /// Empty bag.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace every value for `name`.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.0
            .insert(name.to_ascii_lowercase(), vec![value.into()]);
    }

    /// Append a value for `name`, keeping existing ones.
    pub fn add(&mut self, name: &str, value: impl Into<String>) {
        self.0
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(value.into());
    }

    /// Drop every value for `name`.
    pub fn remove(&mut self, name: &str) {
        self.0.remove(&name.to_ascii_lowercase());
    }

    /// First value for `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(&name.to_ascii_lowercase())
            .and_then(|v| v.first())
            .map(String::as_str)
    }

    /// Whether `name` is present.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.0.contains_key(&name.to_ascii_lowercase())
    }

    /// Iterate `(name, value)` pairs, expanding multi-valued entries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .flat_map(|(k, vs)| vs.iter().map(move |v| (k.as_str(), v.as_str())))
    }

    /// Number of distinct header names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the bag holds no headers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Query parameters carried through the plugin chain, in declaration order.
pub type QueryParams = Vec<(String, String)>;

/// Mutable request state handed to auth, guard and transform plugins.
#[derive(Debug)]
pub struct RequestContext {
    /// Caller identity, used for tenant-scoped credential resolution.
    pub security_context: SecurityContext,
    /// Binding configuration for the executing plugin.
    pub config: Map<String, Value>,
    /// Outbound HTTP method.
    pub method: String,
    /// Outbound path (already merged from route path + suffix).
    pub path: String,
    /// Outbound query parameters.
    pub query: QueryParams,
    /// Outbound headers.
    pub headers: HeaderBag,
    /// Buffered request body, when the request is not streamed.
    pub body: Option<Bytes>,
    /// Alias of the resolved upstream.
    pub upstream_alias: String,
    /// Host of the selected endpoint.
    pub upstream_host: String,
}

impl RequestContext {
    /// Read a plugin config key as a string.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }
}

/// Mutable response state handed to guard and transform plugins.
#[derive(Debug)]
pub struct ResponseContext {
    /// Binding configuration for the executing plugin.
    pub config: Map<String, Value>,
    /// Upstream status code.
    pub status: u16,
    /// Response headers.
    pub headers: HeaderBag,
    /// Buffered response body, when the response is not streamed.
    pub body: Option<Bytes>,
}

impl ResponseContext {
    /// Read a plugin config key as a string.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<&str> {
        self.config.get(key).and_then(Value::as_str)
    }
}

/// State handed to transform plugins on the error path.
#[derive(Debug)]
pub struct ErrorContext {
    /// Binding configuration for the executing plugin.
    pub config: Map<String, Value>,
    /// The error about to be rendered.
    pub error: DomainError,
}

/// A guard's verdict.
#[derive(Debug, Clone)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop and render this error.
    Reject(DomainError),
}

/// Credential injection. One per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// Short human-readable identifier.
    fn id(&self) -> &str;

    /// GTS plugin type identifier.
    fn plugin_type(&self) -> &str;

    /// Inject credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Rejected`] when the credential is unusable, or
    /// [`PluginError::Internal`] when a dependency fails.
    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult;
}

/// Validation and policy enforcement; may reject. Many per upstream/route.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// Short human-readable identifier.
    fn id(&self) -> &str;

    /// GTS plugin type identifier.
    fn plugin_type(&self) -> &str;

    /// Inspect the outbound request.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the guard cannot evaluate the request.
    async fn guard_request(&self, ctx: &RequestContext) -> PluginResult<GuardDecision>;

    /// Inspect the upstream response.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the guard cannot evaluate the response.
    async fn guard_response(&self, ctx: &ResponseContext) -> PluginResult<GuardDecision>;
}

/// Request / response / error mutation. Many per upstream/route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// Short human-readable identifier.
    fn id(&self) -> &str;

    /// GTS plugin type identifier.
    fn plugin_type(&self) -> &str;

    /// Mutate the outbound request.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transform cannot be applied.
    async fn transform_request(&self, _ctx: &mut RequestContext) -> PluginResult {
        Ok(())
    }

    /// Mutate the upstream response.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transform cannot be applied.
    async fn transform_response(&self, _ctx: &mut ResponseContext) -> PluginResult {
        Ok(())
    }

    /// Mutate a gateway error before it is rendered.
    ///
    /// # Errors
    ///
    /// [`PluginError`] when the transform cannot be applied.
    async fn transform_error(&self, _ctx: &mut ErrorContext) -> PluginResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bag_is_case_insensitive() {
        let mut bag = HeaderBag::new();
        bag.set("Content-Type", "application/json");
        assert_eq!(bag.get("content-type"), Some("application/json"));
        assert!(bag.contains("CONTENT-TYPE"));
        bag.remove("Content-TYPE");
        assert!(bag.is_empty());
    }

    #[test]
    fn header_bag_add_keeps_duplicates_but_set_replaces() {
        let mut bag = HeaderBag::new();
        bag.add("x-tag", "a");
        bag.add("x-tag", "b");
        assert_eq!(bag.iter().filter(|(k, _)| *k == "x-tag").count(), 2);
        bag.set("x-tag", "c");
        assert_eq!(bag.iter().filter(|(k, _)| *k == "x-tag").count(), 1);
        assert_eq!(bag.get("x-tag"), Some("c"));
    }

    #[test]
    fn plugin_error_projects_onto_domain_error() {
        let rejected = PluginError::Rejected(DomainError::validation("nope"));
        assert_eq!(rejected.into_domain("p").status(), 400);
        let internal = PluginError::Internal("boom".to_owned());
        let err = internal.into_domain("p");
        assert_eq!(err.status(), 500);
        assert!(err.detail().contains("boom"));
    }
}
