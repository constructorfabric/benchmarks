//! The three plugin traits of ADR-0002 and the context they are handed.
//!
//! The traits are deliberately narrow: a plugin sees the request context (who
//! is calling, what was resolved, which headers came in) and the header map of
//! the phase it runs in. It never sees a body — a plugin that needs one is a
//! proxy feature, not a plugin — and it never returns data, only an outcome
//! and, for the transforming phases, the mutated header map.
//!
//! # Execution order (ADR-0002 "Execution Order")
//!
//! ```text
//! auth (once) → guards → transforms → upstream → transforms → guards
//! ```
//!
//! The engine (`engine`) sequences them; the traits only define one plugin's
//! behaviour in one phase.

use http::HeaderMap;
use serde_json::Value;

use crate::domain::services::data_plane::ProxyContext;
use crate::error::OagwError;

/// What one plugin sees in one phase (ADR-0002 `RequestContext`).
///
/// `config` is the binding's own configuration: the `auth.config` object for an
/// auth plugin, the `PluginRef` binding's `config` object for a guard or
/// transform plugin (ADR-0009 "Plugin Config (ctx.config keys)"). It is the only
/// configuration a plugin is given — a plugin cannot read the upstream's other
/// settings, which is what keeps a plugin from quietly becoming a second
/// configuration language.
pub struct PluginContext<'a> {
    /// Configuration of the binding, when it was written with one.
    pub config: Option<&'a Value>,
    /// The proxy request the plugin runs in.
    pub request: &'a ProxyContext,
}

impl PluginContext<'_> {
    /// A string entry of the binding's configuration.
    #[must_use]
    pub fn string(&self, key: &str) -> Option<&str> {
        self.config?.as_object()?.get(key)?.as_str()
    }
}

/// Injects the credentials the upstream expects (ADR-0002 "AuthPlugin").
///
/// Executed once per request, before any guard, on the *outbound* header map:
/// the credential is written to what the upstream reads, never to what the
/// caller sent.
#[async_trait::async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn plugin_ref(&self) -> &str;

    /// Inject the credential into `headers`.
    ///
    /// # Errors
    /// A credential that cannot be produced (a missing secret, a malformed
    /// configuration) fails the request; there is no silent fallback to an
    /// unauthenticated forward.
    async fn authenticate(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError>;
}

/// Validates the request and the response, and may reject either
/// (ADR-0002 "GuardPlugin").
#[async_trait::async_trait]
pub trait GuardPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn plugin_ref(&self) -> &str;

    /// Validate the *inbound* request.
    ///
    /// # Errors
    /// A rejection fails the request with the status the plugin documents.
    async fn guard_request(
        &self,
        context: &PluginContext<'_>,
        headers: &HeaderMap,
    ) -> Result<(), OagwError>;

    /// Validate the upstream's response.
    ///
    /// # Errors
    /// A rejection fails the request after the upstream answered.
    async fn guard_response(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError>;
}

/// Modifies the request and the response (ADR-0002 "TransformPlugin").
#[async_trait::async_trait]
pub trait TransformPlugin: Send + Sync {
    /// The full GTS identifier the plugin is registered under.
    fn plugin_ref(&self) -> &str;

    /// Transform the outbound request headers.
    ///
    /// # Errors
    /// An implementation may fail the request.
    async fn transform_request(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError>;

    /// Transform the response headers before they reach the caller.
    ///
    /// # Errors
    /// An implementation may fail the request.
    async fn transform_response(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError>;
}
