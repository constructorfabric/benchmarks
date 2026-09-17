//! Plugin contract surface (ADR 0002).
//!
//! Three independent traits — auth, guard, transform — with a deterministic
//! execution order: Auth → Guards (request) → Transforms (request) → upstream →
//! Transforms (response) → Guards (response) → Transforms (error).
//!
//! Plugins never observe secret material: auth plugins receive an
//! [`AuthPluginRuntime`] handle that resolves `cred://` references through the
//! CredStore and hands back an opaque `SecretString` which the plugin must
//! inject into a header/query value.

use std::collections::BTreeMap;

use async_trait::async_trait;
use toolkit_security::SecurityContext;

/// Failure raised by a plugin. Carries only a reason code and a safe message —
/// never credential material.
#[derive(Debug, Clone)]
pub struct PluginError {
    /// Static reason code (e.g. `REQUIRED_HEADER_MISSING`).
    pub code: String,
    /// Human-readable, credential-free explanation.
    pub message: String,
}

impl PluginError {
    /// Builds a failure.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

/// Guard verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardDecision {
    /// Continue the chain.
    Allow,
    /// Stop the chain and emit a gateway problem response.
    Reject {
        /// HTTP status to return.
        status: u16,
        /// Machine-readable reason.
        error_code: String,
        /// Credential-free explanation.
        message: String,
    },
}

impl GuardDecision {
    /// Builds a rejection.
    #[must_use]
    pub fn reject(status: u16, error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Reject {
            status,
            error_code: error_code.into(),
            message: message.into(),
        }
    }

    /// `true` for [`GuardDecision::Allow`].
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

/// Opaque credential handle.
///
/// Backed by [`toolkit_auth::SecretString`], which redacts `Debug`/`Display`
/// and zeroizes the buffer on drop. Reading the value requires an explicit
/// `expose()` call, so accidental logging is unlikely.
#[derive(Clone)]
pub struct ResolvedSecret {
    value: std::sync::Arc<toolkit_auth::SecretString>,
}

impl ResolvedSecret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self {
            value: std::sync::Arc::new(toolkit_auth::SecretString::new(value)),
        }
    }

    /// Renders the value prefixed, e.g. `Bearer <token>`.
    ///
    /// The result is a plain `String` with a short, request-scoped lifetime.
    #[must_use]
    pub fn render_prefixed(&self, prefix: &str) -> String {
        format!("{}{}", prefix, self.expose())
    }

    /// Exposes the secret value.
    #[must_use]
    pub fn expose(&self) -> &str {
        self.value.expose()
    }
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResolvedSecret(REDACTED)")
    }
}

/// Credential resolution provided to auth plugins.
///
/// Splitting resolution out of the plugin keeps credential material out of the
/// plugin API surface entirely.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves a `cred://` reference (or bare key) for the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns an error when the CredStore itself fails. A missing or
    /// inaccessible reference resolves to `Ok(None)`.
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError>;
}

/// Mutating view of the outbound request handed to plugins.
#[derive(Debug, Default, Clone)]
pub struct RequestContext {
    /// Security context of the caller (never serialized).
    pub security_context: Option<SecurityContext>,
    /// Outbound request headers (already hop-by-hop and routing stripped).
    pub headers: Vec<(String, String)>,
    /// Outbound query pairs, in request order.
    pub query: Vec<(String, String)>,
    /// Outbound path (route path with suffix already applied).
    pub path: String,
    /// Effective plugin configuration (`ctx.config`).
    pub config: BTreeMap<String, String>,
    /// Owning tenant of the resolved upstream.
    pub tenant_id: Option<uuid::Uuid>,
}

impl RequestContext {
    /// Reads the first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Replaces the first value of a header, case-insensitively.
    pub fn set_header(&mut self, name: &str, value: String) {
        if let Some(slot) = self
            .headers
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            slot.1 = value;
        } else {
            self.headers.push((name.to_owned(), value));
        }
    }

    /// Removes every occurrence of a header, case-insensitively.
    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    /// `true` when a header is present.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

/// Mutating view of the upstream response handed to plugins.
#[derive(Debug, Default)]
pub struct ResponseContext {
    /// Upstream status code.
    pub status: u16,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Effective plugin configuration.
    pub config: std::collections::BTreeMap<String, String>,
}

impl ResponseContext {
    /// Reads the first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// `true` when a header is present.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// Removes every occurrence of a header, case-insensitively.
    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }
}

/// Mutating view of a gateway error handed to transform plugins.
#[derive(Debug)]
pub struct ErrorContext {
    /// The gateway error about to be rendered.
    pub error: crate::domain::error::DomainError,
}

/// Injects outbound credentials (ADR 0002).
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin family (`auth_plugin`).
    fn plugin_type(&self) -> &'static str {
        "auth_plugin"
    }
    /// Injects credentials into `ctx`.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when credentials cannot be resolved.
    async fn authenticate(
        &self,
        ctx: &mut RequestContext,
        secrets: &dyn SecretResolver,
    ) -> Result<(), PluginError>;
}

/// Validates requests and responses (ADR 0002).
#[async_trait]
pub trait GuardPlugin: Send + Sync {
    /// GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin family (`guard_plugin`).
    fn plugin_type(&self) -> &'static str {
        "guard_plugin"
    }
    /// Validates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on infrastructure failure.
    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError>;
    /// Validates the upstream response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on infrastructure failure.
    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError>;
}

/// Modifies request/response/error data (ADR 0002).
#[async_trait]
pub trait TransformPlugin: Send + Sync {
    /// GTS identifier of the plugin.
    fn id(&self) -> &str;
    /// Plugin family (`transform_plugin`).
    fn plugin_type(&self) -> &'static str {
        "transform_plugin"
    }
    /// Mutates the outbound request.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), PluginError>;
    /// Mutates the inbound response.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), PluginError>;
    /// Mutates a gateway error.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] on failure.
    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), PluginError>;
}
