//! The built-in `apikey` auth plugin
//! (`cpt-cf-oagw-dod-plugin-system-builtin-auth`).
//!
//! `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1` resolves its
//! credential from a `cred://` reference **at request time** — never at
//! configuration time, never into a stored record — and injects it into the
//! configured header or query location of the *outbound* request. The inbound
//! request headers are never read and never modified.

use async_trait::async_trait;

use crate::domain::plugin::{AuthContext, AuthPlugin, PluginError};
use crate::infra::plugin::credentials::{security_context_for, CredentialResolver};

/// The default header name the API key is injected under.
pub const DEFAULT_HEADER_NAME: &str = "x-api-key";

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`
pub const APIKEY_PLUGIN_TYPE: &str =
    crate::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID;

/// The built-in API-key auth plugin.
///
/// Configuration keys (`domain::type_catalog::builtin_config_schema`):
///
/// * `api_key_ref` (required) — the `cred://` reference the key resolves from;
/// * `location` — `header` (default) or `query`;
/// * `name` — the header or query name, default `x-api-key`.
#[derive(Clone)]
pub struct ApiKeyAuthPlugin {
    resolver: CredentialResolver,
}

impl ApiKeyAuthPlugin {
    /// Build the plugin over the `cred_store` handle entry 2.1 resolved.
    #[must_use]
    pub fn new(resolver: CredentialResolver) -> Self {
        Self { resolver }
    }

    /// The credential location a configuration names, defaulting to `header`.
    #[must_use]
    pub fn location(config: Option<&serde_json::Value>) -> &'static str {
        config
            .and_then(|config| config.get("location"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_ascii_lowercase)
            .map(|location| match location.as_str() {
                "query" => "query",
                _ => "header",
            })
            .unwrap_or("header")
    }

    /// The header or query name a configuration names, defaulting to
    /// `x-api-key`.
    #[must_use]
    pub fn field_name(config: Option<&serde_json::Value>) -> String {
        config
            .and_then(|config| config.get("name"))
            .and_then(serde_json::Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| DEFAULT_HEADER_NAME.to_owned())
    }

    /// The `cred://` reference a configuration carries.
    ///
    /// # Errors
    ///
    /// [`PluginError::Internal`] when the required key is absent; the value is
    /// never echoed.
    pub fn reference(config: Option<&serde_json::Value>) -> Result<String, PluginError> {
        config
            .and_then(|config| config.get("api_key_ref"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                PluginError::Internal(
                    "the apikey plugin configuration carries no `api_key_ref`".to_owned(),
                )
            })
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        "apikey"
    }

    fn plugin_type(&self) -> &str {
        APIKEY_PLUGIN_TYPE
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-1
    // `inst-ps-cred-1`/`-2`: the `cred://` reference is read from the plugin
    // configuration and resolved through `cred_store` at request time, and
    // `cred_store` decides whether the reference is accessible to the
    // requesting tenant.
    async fn authenticate(&self, ctx: &mut AuthContext) -> Result<(), PluginError> {
        let reference = Self::reference(ctx.config.as_ref())?;
        let security = security_context_for(
            ctx.principal.subject_id,
            ctx.principal.tenant_id,
            &ctx.principal.scopes,
        );
        // The resolved material lives only for this invocation; dropping it
        // zeroes its buffer, and the header string written below is the
        // residual-plaintext surface ADR 0008 records as its exception.
        let resolved = self.resolver.resolve(&security, &reference).await?;
        let value = resolved.as_str()?.to_owned();
        match Self::location(ctx.config.as_ref()) {
            "query" => ctx.outbound_query.push((Self::field_name(ctx.config.as_ref()), value)),
            _ => ctx.outbound_headers.push((Self::field_name(ctx.config.as_ref()), value)),
        }
        Ok(())
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-1
}

// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-5
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-8
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-9
/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1`
pub const NOOP_PLUGIN_TYPE: &str = crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID;
//
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-9
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-6
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-5
// @cpt-end:cpt-cf-oagw-flow-plugin-system-credential-resolution:p1:inst-ps-cred-4
//

/// The built-in no-op auth plugin: the upstream that declares no credential
/// requirement passes through unauthenticated.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "noop"
    }

    fn plugin_type(&self) -> &str {
        NOOP_PLUGIN_TYPE
    }

    async fn authenticate(&self, _ctx: &mut AuthContext) -> Result<(), PluginError> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "apikey_auth_tests.rs"]
mod apikey_auth_tests;
