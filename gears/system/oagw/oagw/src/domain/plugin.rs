//! The three plugin types and their registries.
//!
//! Execution order is Auth → Guards → Transform(request) → upstream call →
//! Transform(response/error). Upstream plugins execute before route plugins.

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::ids;
use crate::domain::model::PluginType;

/// Everything a plugin needs to see or change about a proxied request.
#[derive(Debug, Default, Clone)]
pub struct RequestContext {
    /// Header values observed or mutated, keyed by lowercase header name.
    pub headers: Vec<(String, String)>,
    /// Query parameters, in order.
    pub query: Vec<(String, String)>,
    /// Values shared between plugins for one request.
    pub attributes: std::collections::BTreeMap<String, String>,
    /// The generated or propagated correlation id.
    pub request_id: Option<String>,
    /// Credential injected by the auth plugin.
    pub credential: Option<Credential>,
    /// Plugin execution order recorded for diagnostics.
    pub execution: Vec<String>,
    /// The `URL` the token endpoint was called at, when an auth plugin fetched one.
    pub token_endpoint_hits: usize,
    /// Authenticated caller id, from the host security context.
    pub subject_id: uuid::Uuid,
    /// Authenticated caller tenant id, from the host security context.
    pub subject_tenant_id: uuid::Uuid,
    /// Header names a plugin wrote, so the proxy can forward them even when
    /// the upstream's `passthrough` setting would drop inbound headers.
    pub touched: std::collections::BTreeSet<String>,
    /// Response headers observed or written during the response phase.
    pub response_headers: Vec<(String, String)>,
}

/// A credential an auth plugin injected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A header value, appended to the outbound request.
    Header(String, String),
    /// A query parameter, appended to the outbound query string.
    Query(String, String),
}

impl RequestContext {
    /// Records that a plugin ran.
    pub fn record(&mut self, name: &str) {
        self.execution.push(name.to_owned());
    }

    /// Reads the first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lowered = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == lowered)
            .map(|(_, value)| value.as_str())
    }

    /// Sets a header, replacing any existing values of that name.
    pub fn set_header(&mut self, name: &str, value: &str) {
        let lowered = name.to_ascii_lowercase();
        self.headers.retain(|(key, _)| *key != lowered.clone());
        self.headers.push((lowered.clone(), value.to_owned()));
        self.touched.insert(lowered);
    }

    /// Removes a header.
    pub fn remove_header(&mut self, name: &str) {
        let lowered = name.to_ascii_lowercase();
        self.headers.retain(|(key, _)| *key != lowered.clone());
        self.touched.insert(lowered);
    }

    /// Adds a header, keeping existing values.
    pub fn add_header(&mut self, name: &str, value: &str) {
        let lowered = name.to_ascii_lowercase();
        self.headers.push((lowered.clone(), value.to_owned()));
        self.touched.insert(lowered);
    }
}

/// Credential-injection plugins. One per upstream.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The catalogue identifier this plugin answers to.
    fn id(&self) -> &'static str;

    /// Human readable name.
    fn name(&self) -> &'static str;

    /// Injects credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns an [`PluginError`] when credential resolution or the token
    /// exchange fails.
    async fn authenticate(&self, ctx: &mut RequestContext, config: &serde_json::Value)
        -> Result<(), PluginError>;
}

/// Validation plugins. Many may be bound to an upstream or route.
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The catalogue identifier this plugin answers to.
    fn id(&self) -> &'static str;

    /// Human readable name.
    fn name(&self) -> &'static str;

    /// Validates the request phase.
    ///
    /// # Errors
    ///
    /// Returns an [`PluginError`] when the request must be rejected.
    async fn guard_request(
        &self,
        ctx: &RequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, PluginError>;

    /// Validates the response phase. Defaults to passing.
    ///
    /// # Errors
    ///
    /// Returns an [`PluginError`] when the response must be rejected.
    async fn guard_response(
        &self,
        ctx: &RequestContext,
        config: &serde_json::Value,
    ) -> Result<GuardDecision, PluginError> {
        let _ = (ctx, config);
        Ok(GuardDecision::Allow)
    }
}

/// Mutation plugins. Many may be bound to an upstream or route.
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The catalogue identifier this plugin answers to.
    fn id(&self) -> &'static str;

    /// Human readable name.
    fn name(&self) -> &'static str;

    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns an [`PluginError`] when the mutation fails.
    async fn transform_request(
        &self,
        ctx: &mut RequestContext,
        config: &serde_json::Value,
    ) -> Result<(), PluginError>;

    /// Mutates the response returned to the caller. Defaults to a no-op.
    ///
    /// # Errors
    ///
    /// Returns an [`PluginError`] when the mutation fails.
    async fn transform_response(
        &self,
        ctx: &mut RequestContext,
        config: &serde_json::Value,
    ) -> Result<(), PluginError> {
        let _ = (ctx, config);
        Ok(())
    }
}

/// Why a plugin failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    /// A credential reference could not be resolved.
    SecretNotFound(String),
    /// Credential injection or the token exchange failed.
    AuthFailed(String),
    /// A guard rejected the request.
    GuardRejected {
        /// `HTTP` status to return.
        status: u16,
        /// GTS error identifier.
        error_type: String,
        /// Machine-readable code.
        code: String,
        /// Human readable detail.
        detail: String,
    },
    /// The plugin is known to the catalogue but has no implementation.
    NotImplemented,
}

impl PluginError {
    /// Builds a guard rejection.
    #[must_use]
    pub fn guard(status: u16, error_type: &str, code: &str, detail: impl Into<String>) -> Self {
        Self::GuardRejected {
            status,
            error_type: error_type.to_owned(),
            code: code.to_owned(),
            detail: detail.into(),
        }
    }

    /// `HTTP` status for this failure.
    #[must_use]
    pub fn status(&self) -> u16 {
        match self {
            Self::SecretNotFound(_) => 500,
            Self::AuthFailed(_) => 401,
            Self::GuardRejected { status, .. } => *status,
            Self::NotImplemented => 503,
        }
    }

    /// GTS error identifier for this failure.
    #[must_use]
    pub fn error_type(&self) -> String {
        match self {
            Self::SecretNotFound(_) => ids::ERR_SECRET_NOT_FOUND,
            Self::AuthFailed(_) => ids::ERR_AUTH_FAILED,
            Self::GuardRejected { error_type, .. } => error_type.as_str(),
            Self::NotImplemented => ids::ERR_PLUGIN_NOT_FOUND,
        }
        .to_owned()
    }

    /// Machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::SecretNotFound(_) => "SECRET_NOT_FOUND",
            Self::AuthFailed(_) => "AUTHENTICATION_FAILED",
            Self::GuardRejected { code, .. } => code,
            Self::NotImplemented => "PLUGIN_NOT_FOUND",
        }
    }

    /// Human readable detail.
    #[must_use]
    pub fn detail(&self) -> String {
        match self {
            Self::SecretNotFound(name) => format!("secret '{name}' not found"),
            Self::AuthFailed(detail) | Self::GuardRejected { detail, .. } => detail.clone(),
            Self::NotImplemented => "plugin has no implementation".to_owned(),
        }
    }
}

/// A guard's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardDecision {
    /// Continue.
    Allow,
    /// Reject with an error.
    Reject(PluginError),
}

impl GuardDecision {
    /// True when the guard allows the request through.
    #[must_use]
    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Registry of auth plugins.
#[derive(Default)]
pub struct AuthPluginRegistry {
    plugins: std::collections::BTreeMap<String, Arc<dyn AuthPlugin>>,
}

/// Registry of guard plugins.
#[derive(Default)]
pub struct GuardPluginRegistry {
    plugins: std::collections::BTreeMap<String, Arc<dyn GuardPlugin>>,
}

/// Registry of transform plugins.
#[derive(Default)]
pub struct TransformPluginRegistry {
    plugins: std::collections::BTreeMap<String, Arc<dyn TransformPlugin>>,
}

impl AuthPluginRegistry {
    /// Registers a plugin.
    pub fn register(&mut self, plugin: Arc<dyn AuthPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by its GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn AuthPlugin>> {
        self.plugins.get(&instance_of(id)).cloned()
    }

    /// All registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Whether a GTS identifier is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.resolve(id).is_some()
    }
}

impl GuardPluginRegistry {
    /// Registers a plugin.
    pub fn register(&mut self, plugin: Arc<dyn GuardPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by its GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn GuardPlugin>> {
        self.plugins.get(&instance_of(id)).cloned()
    }

    /// All registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Whether a GTS identifier is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.resolve(id).is_some()
    }
}

impl TransformPluginRegistry {
    /// Registers a plugin.
    pub fn register(&mut self, plugin: Arc<dyn TransformPlugin>) {
        self.plugins.insert(plugin.id().to_owned(), plugin);
    }

    /// Resolves a plugin by its GTS identifier.
    #[must_use]
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn TransformPlugin>> {
        self.plugins.get(&instance_of(id)).cloned()
    }

    /// All registered plugin ids.
    #[must_use]
    pub fn ids(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    /// Whether a GTS identifier is registered.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.resolve(id).is_some()
    }
}

/// Strips the catalogue type prefix from a plugin GTS identifier.
#[must_use]
fn instance_of(id: &str) -> String {
    match ids::split_gts_id(id) {
        Some((_, instance)) => instance.to_owned(),
        None => id.to_owned(),
    }
}

/// Combined plugin registries handed to the data plane.
#[derive(Default)]
pub struct PluginRegistries {
    /// Auth plugins.
    pub auth: AuthPluginRegistry,
    /// Guard plugins.
    pub guard: GuardPluginRegistry,
    /// Transform plugins.
    pub transform: TransformPluginRegistry,
}

impl PluginRegistries {
    /// Whether `plugin_ref` resolves in any registry.
    #[must_use]
    pub fn resolves(&self, plugin_ref: &str) -> Option<PluginType> {
        if self.auth.contains(plugin_ref) {
            return Some(PluginType::Auth);
        }
        if self.guard.contains(plugin_ref) {
            return Some(PluginType::Guard);
        }
        if self.transform.contains(plugin_ref) {
            return Some(PluginType::Transform);
        }
        None
    }
}
