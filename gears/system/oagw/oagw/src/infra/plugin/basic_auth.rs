//! HTTP Basic auth plugin
//! (`gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1`).
//!
//! PRD and DESIGN list `basic.v1` as a *reserved* identifier: it is cataloged
//! in the types-registry but is **not** part of
//! [`AuthPluginRegistry::with_builtins()`] (DESIGN.md "built-in plugins"), so
//! binding it as `auth.plugin_type` surfaces as `unknown auth plugin` at
//! authoring time.
//!
//! The implementation is nevertheless complete: the credential pair is read
//! from the credstore at request time and injected as
//! `Authorization: Basic base64(user:password)`, so the crate has no
//! "not implemented" code path for a credential-injection plugin.

use async_trait::async_trait;
use std::sync::Arc;

pub use crate::domain::gts_helpers::BASIC_AUTH_PLUGIN_ID;
use crate::domain::plugin::{AuthPlugin, PluginConfig, PluginError, PluginResult, RequestContext};
use crate::infra::proxy::base64::base64_encode;
use crate::infra::proxy::runtime::PluginRuntime;

/// Configuration of the HTTP Basic auth plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BasicAuthConfig {
    /// Credstore secret reference holding the `username:password` pair.
    pub secret_ref: String,
    /// Optional realm reported by the upstream challenge.
    pub realm: Option<String>,
}

impl BasicAuthConfig {
    /// Parse a configuration from a raw `auth.config` value.
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        let s = |k: &str| config.get(k).and_then(|v| v.as_str()).map(str::to_owned);
        Self {
            secret_ref: s("secret_ref").or_else(|| s("key_ref")).unwrap_or_default(),
            realm: s("realm").filter(|r| !r.trim().is_empty()),
        }
    }

    /// Parse a configuration from a merged plugin configuration.
    #[must_use]
    pub fn from_plugin_config(config: &PluginConfig) -> Self {
        Self::from_config(&config.config)
    }

    /// True when the configuration names a credential to resolve.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.secret_ref.trim().is_empty()
    }

    /// Validate the configuration.
    ///
    /// # Errors
    /// [`PluginError::Internal`] when `secret_ref` is missing.
    pub fn validate(&self) -> PluginResult<()> {
        if self.secret_ref.trim().is_empty() {
            return Err(PluginError::Internal {
                plugin_id: BASIC_AUTH_PLUGIN_ID.to_owned(),
                detail:
                    "auth.config.secret_ref (credstore reference to `user:password`) is required"
                        .to_owned(),
            });
        }
        Ok(())
    }

    /// The `Authorization` header value for a resolved `user:password` pair.
    #[must_use]
    pub fn header_value(&self, credentials: &str) -> String {
        format!("Basic {}", base64_encode(credentials.as_bytes()))
    }
}

/// The HTTP Basic auth plugin (not registered by `with_builtins()`).
#[derive(Clone)]
pub struct BasicAuthPlugin {
    runtime: Arc<PluginRuntime>,
}

impl std::fmt::Debug for BasicAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuthPlugin")
            .field("runtime", &self.runtime)
            .finish()
    }
}

impl BasicAuthPlugin {
    /// A plugin over `runtime`.
    #[must_use]
    pub fn new(runtime: Arc<PluginRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl AuthPlugin for BasicAuthPlugin {
    fn id(&self) -> &str {
        BASIC_AUTH_PLUGIN_ID
    }

    fn plugin_type(&self) -> &str {
        BASIC_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> PluginResult<()> {
        let config = BasicAuthConfig::from_plugin_config(&ctx.config);
        config.validate()?;
        let secret = self
            .runtime
            .resolve_secret(ctx.tenant_id, &ctx.security, &config.secret_ref)
            .await?;
        let credentials = secret
            .as_str()
            .ok_or_else(|| PluginError::SecretUnavailable {
                plugin_id: BASIC_AUTH_PLUGIN_ID.to_owned(),
                detail: "the referenced Basic credential is not valid UTF-8".to_owned(),
            })?;
        if !credentials.contains(':') {
            return Err(PluginError::SecretUnavailable {
                plugin_id: BASIC_AUTH_PLUGIN_ID.to_owned(),
                detail: "the referenced Basic credential must be a `username:password` pair"
                    .to_owned(),
            });
        }
        ctx.set_header(
            http::header::AUTHORIZATION.as_str(),
            &config.header_value(credentials),
        )?;
        ctx.attributes
            .insert("auth_plugin".to_owned(), BASIC_AUTH_PLUGIN_ID.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::registry::AuthPluginRegistry;
    use crate::infra::proxy::runtime::PluginRuntime;
    use crate::infra::proxy::secrets::InMemorySecretSource;
    use http::HeaderMap;

    fn request_ctx(config: serde_json::Value) -> RequestContext {
        RequestContext {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            query: String::new(),
            headers: HeaderMap::new(),
            body: None,
            downstream_headers: HeaderMap::new(),
            security: toolkit_security::SecurityContext::anonymous(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id: None,
            route_id: None,
            alias: None,
            trace_id: None,
            config: PluginConfig {
                plugin_id: BASIC_AUTH_PLUGIN_ID.to_owned(),
                position: 0,
                at_upstream_level: true,
                config,
            },
            attributes: Default::default(),
        }
    }

    #[test]
    fn identifier_is_catalog_only() {
        // Deliberately absent from the registry: binding it as auth.plugin_type
        // must surface as "unknown auth plugin" (DESIGN.md).
        assert!(
            AuthPluginRegistry::with_builtins()
                .get(BASIC_AUTH_PLUGIN_ID)
                .is_none()
        );
        assert!(!BasicAuthConfig::from_config(&serde_json::json!({})).is_configured());
    }

    #[test]
    fn header_value_is_base64_of_the_pair() {
        let config = BasicAuthConfig::default();
        assert_eq!(
            config.header_value("alice:s3cret"),
            "Basic YWxpY2U6czNjcmV0"
        );
    }

    #[tokio::test]
    async fn injects_a_basic_authorization_header() {
        let source = InMemorySecretSource::new();
        source.insert("basic-cred", "alice:s3cret");
        let plugin = BasicAuthPlugin::new(Arc::new(PluginRuntime::new(
            Arc::new(source),
            std::time::Duration::from_secs(300),
            10,
            None,
        )));
        let mut ctx = request_ctx(serde_json::json!({"secret_ref": "basic-cred"}));
        plugin.authenticate(&mut ctx).await.unwrap();
        assert_eq!(
            ctx.headers.get("authorization").unwrap(),
            "Basic YWxpY2U6czNjcmV0"
        );
        // The credential material never reaches the request attributes.
        assert!(!format!("{ctx:?}").contains("s3cret"));
    }

    #[tokio::test]
    async fn a_malformed_credential_is_refused() {
        let source = InMemorySecretSource::new();
        source.insert("basic-cred", "alice-no-password");
        let plugin = BasicAuthPlugin::new(Arc::new(PluginRuntime::new(
            Arc::new(source),
            std::time::Duration::from_secs(300),
            10,
            None,
        )));
        let mut ctx = request_ctx(serde_json::json!({"secret_ref": "basic-cred"}));
        let err = plugin.authenticate(&mut ctx).await.unwrap_err();
        assert!(matches!(err, PluginError::SecretUnavailable { .. }));
    }
}
