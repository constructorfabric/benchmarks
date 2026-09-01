//! Built-in plugin implementations and registry construction
//! (`infra/plugin`, ADR 0002 "Built-in Plugins").
//!
//! Every built-in implements one of the three `domain::plugin` traits, so the
//! proxy engine executes built-ins and external gears through exactly the same
//! pipeline: Auth → Guard → Transform(request) → upstream → Transform(response).
//!
//! | Family | Identifier | Implementation |
//! |---|---|---|
//! | Auth | `cf.core.oagw.noop.v1` | [`NoopAuthPlugin`] |
//! | Auth | `cf.core.oagw.apikey.v1` | [`apikey::ApiKeyAuthPlugin`] |
//! | Auth | `cf.core.oagw.oauth2_client_cred.v1` | [`oauth2::OAuth2ClientCredAuthPlugin`] (form) |
//! | Auth | `cf.core.oagw.oauth2_client_cred_basic.v1` | [`oauth2::OAuth2ClientCredAuthPlugin`] (basic) |
//! | Guard | `cf.core.oagw.required_headers.v1` | [`guards::RequiredHeadersGuardPlugin`] |
//! | Transform | `cf.core.oagw.request_id.v1` | [`guards::RequestIdTransformPlugin`] |
//!
//! `cf.core.oagw.basic.v1` and `cf.core.oagw.bearer.v1` are **catalogue-only**
//! identifiers: they have no backing implementation, so a chain that references
//! them resolves to [`crate::domain::error::DomainError::PluginNotFound`]
//! instead of silently proxying without credentials.
//!
//! Guard and transform bindings carry a reference only (`plugins.items` is a
//! list of GTS ids / UUIDs), so a built-in guard/transform receives an empty
//! configuration object. For `required_headers` that is exactly ADR 0009's
//! documented fail-open behaviour.

pub mod apikey;
pub mod guards;
pub mod oauth2;
pub mod secret;

use std::sync::Arc;

use async_trait::async_trait;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AuthPlugin, PluginRegistry, RequestContext,
};

use apikey::ApiKeyAuthPlugin;
use guards::{RequiredHeadersGuardPlugin, RequestIdTransformPlugin};
use oauth2::OAuth2ClientCredAuthPlugin;
pub use secret::{CREDENTIAL_SCHEME, LiteralSecretResolver, SecretResolver};

/// Authentication plugin that injects nothing (`cf.core.oagw.noop.v1`).
///
/// Review evidence (privilege boundary — authentication chain):
/// * Guardrail: DESIGN §3.1 lists `noop.v1` as the explicit "no authentication"
///   built-in; ADR 0002 requires it to implement the same trait as every other
///   auth plugin so the execution order is uniform.
/// * Rationale: a permissive default keeps unauthenticated upstreams runnable
///   while still routing them through the plugin chain, so adding a real auth
///   plugin later never changes the pipeline shape.
/// * Validation performed: `plugin_registry_*` tests assert it resolves by both
///   the short name and the GTS id and that it leaves outbound headers
///   untouched.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &'static str {
        "noop"
    }

    fn plugin_type(&self) -> &'static str {
        crate::domain::plugin::builtins::AUTH_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Capability bundle handed to [`registry`].
///
/// The data plane builds one registry per process; the credentials of a
/// request are resolved through the bundled [`SecretResolver`].
pub struct PluginBundle {
    /// Credential resolver shared by every secret-handling plugin.
    pub secrets: Arc<dyn SecretResolver>,
    /// Outbound client used by plugins that need to call a third-party
    /// endpoint (the `OAuth2` token endpoint).
    pub transport: Arc<crate::infra::transport::Transport>,
    /// Ceiling applied to cached auth-plugin tokens.
    pub token_cache_ttl: std::time::Duration,
    /// Capacity of the auth-plugin token cache.
    pub token_cache_capacity: usize,
}

/// Builds a registry populated with the built-in plugins.
///
/// Review evidence (privilege boundary — resolvable plugin surface):
/// * Guardrail: ADR 0002 "Built-in Plugins" — only the five implemented
///   built-ins are registered; `basic`/`bearer` stay catalogue-only so a chain
///   referencing them fails loudly with `503 PluginNotFound`.
/// * Rationale: a half-implemented auth plugin that resolves but injects
///   nothing would look identical to a successful authentication on the wire.
/// * Validation performed: `plugin_registry_rejects_catalogue_only_plugins`
///   asserts the two catalogue identifiers stay unresolvable.
#[must_use]
pub fn registry(bundle: &PluginBundle) -> PluginRegistry {
    let mut plugins = PluginRegistry::new();
    plugins.register_auth(Arc::new(NoopAuthPlugin));
    plugins.register_auth(Arc::new(ApiKeyAuthPlugin::new(Arc::clone(
        &bundle.secrets,
    ))));
    plugins.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::form(
        Arc::clone(&bundle.secrets),
        Arc::clone(&bundle.transport),
        bundle.token_cache_ttl,
        bundle.token_cache_capacity,
    )));
    plugins.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::basic(
        Arc::clone(&bundle.secrets),
        Arc::clone(&bundle.transport),
        bundle.token_cache_ttl,
        bundle.token_cache_capacity,
    )));
    plugins.register_guard(Arc::new(RequiredHeadersGuardPlugin));
    plugins.register_transform(Arc::new(RequestIdTransformPlugin));
    plugins
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
