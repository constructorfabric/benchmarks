//! Built-in plugins of the data plane (ADR-0002 §“Built-in Plugins”).
//!
//! Named plugins (`gts.cf.core.oagw.{auth,guard,transform}_plugin.v1~
//! cf.core.oagw.{name}.v1`) are resolved here; custom (tenant-defined,
//! Starlark) plugins are out of scope and rejected with
//! [`ErrorKind::PluginNotFound`].

pub mod apikey_auth;
pub mod noop_auth;
pub mod oauth2_client_cred_auth;
pub mod request_id_transform;
pub mod required_headers_guard;

use std::sync::Arc;
use std::time::Duration;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::plugin::{AuthPlugin, GuardPlugin, SecretResolver, TransformPlugin};

/// Short name of a plugin reference: the last segment before the `.v1`
/// suffix of the instance part (`cf.core.oagw.apikey.v1` → `apikey`).
#[must_use]
pub fn plugin_name(plugin_ref: &str) -> &str {
    let instance = plugin_ref.rsplit('~').next().unwrap_or(plugin_ref);
    let stem = instance.strip_suffix(".v1").unwrap_or(instance);
    stem.rsplit('.').next().unwrap_or(stem)
}

/// Factory of the built-in plugin implementations.
#[derive(Clone)]
pub struct BuiltinPlugins {
    secrets: Arc<dyn SecretResolver>,
    token_cache_ttl: Duration,
    token_cache_capacity: usize,
}

impl std::fmt::Debug for BuiltinPlugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinPlugins")
            .field("token_cache_ttl", &self.token_cache_ttl)
            .field("token_cache_capacity", &self.token_cache_capacity)
            .finish_non_exhaustive()
    }
}

impl BuiltinPlugins {
    /// Builds the factory from a secret resolver and the token cache settings.
    #[must_use]
    pub fn new(
        secrets: Arc<dyn SecretResolver>,
        token_cache_ttl: Duration,
        token_cache_capacity: usize,
    ) -> Self {
        Self {
            secrets,
            token_cache_ttl,
            token_cache_capacity,
        }
    }

    /// The shared secret resolver.
    #[must_use]
    pub fn secrets(&self) -> Arc<dyn SecretResolver> {
        Arc::clone(&self.secrets)
    }

    /// Builds the auth plugin a resolved reference names.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] for unknown references.
    pub fn build_auth(&self, plugin_ref: &str) -> Result<Box<dyn AuthPlugin>, DomainError> {
        match plugin_name(plugin_ref) {
            "noop" => Ok(Box::new(noop_auth::NoopAuthPlugin)),
            "apikey" => Ok(Box::new(apikey_auth::ApiKeyAuthPlugin::new(self.secrets()))),
            "oauth2_client_cred" | "oauth2_client_cred_basic" => Ok(Box::new(
                oauth2_client_cred_auth::OAuth2ClientCredAuthPlugin::new(
                    self.secrets(),
                    plugin_name(plugin_ref) == "oauth2_client_cred_basic",
                    self.token_cache_ttl,
                    self.token_cache_capacity,
                ),
            )),
            other => Err(unknown_plugin(other)),
        }
    }

    /// Builds the guard plugin a resolved reference names.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] for unknown references.
    pub fn build_guard(&self, plugin_ref: &str) -> Result<Box<dyn GuardPlugin>, DomainError> {
        match plugin_name(plugin_ref) {
            "required_headers" => Ok(Box::new(required_headers_guard::RequiredHeadersGuardPlugin)),
            other => Err(unknown_plugin(other)),
        }
    }

    /// Builds the transform plugin a resolved reference names.
    ///
    /// # Errors
    /// Returns [`ErrorKind::PluginNotFound`] for unknown references.
    pub fn build_transform(
        &self,
        plugin_ref: &str,
    ) -> Result<Box<dyn TransformPlugin>, DomainError> {
        match plugin_name(plugin_ref) {
            "request_id" => Ok(Box::new(
                request_id_transform::RequestIdTransformPlugin::default(),
            )),
            other => Err(unknown_plugin(other)),
        }
    }
}

fn unknown_plugin(name: &str) -> DomainError {
    DomainError::new(
        ErrorKind::PluginNotFound,
        format!("plugin {name:?} has no built-in implementation"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::LiteralSecretResolver;

    fn factory() -> BuiltinPlugins {
        BuiltinPlugins::new(
            Arc::new(LiteralSecretResolver),
            Duration::from_secs(300),
            128,
        )
    }

    #[test]
    fn plugin_names_are_the_last_segment() {
        assert_eq!(
            plugin_name("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            "apikey"
        );
        assert_eq!(
            plugin_name("cf.core.oagw.required_headers.v1"),
            "required_headers"
        );
        assert_eq!(plugin_name("noop.v1"), "noop");
    }

    #[test]
    fn every_builtin_is_buildable() {
        let factory = factory();
        for name in [
            "noop.v1",
            "apikey.v1",
            "oauth2_client_cred.v1",
            "oauth2_client_cred_basic.v1",
        ] {
            let reference = format!("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.{name}");
            assert!(factory.build_auth(&reference).is_ok(), "{name}");
        }
        assert!(
            factory
                .build_guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
                .is_ok()
        );
        assert!(
            factory
                .build_transform("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1")
                .is_ok()
        );
    }

    #[test]
    fn unknown_plugins_are_not_found() {
        let factory = factory();
        let Err(error) =
            factory.build_auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1")
        else {
            panic!("catalog-only plugin must not build");
        };
        assert_eq!(error.kind, ErrorKind::PluginNotFound);
        assert_eq!(error.status(), 503);
        let Err(error) =
            factory.build_guard("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1")
        else {
            panic!("catalog-only plugin must not build");
        };
        assert_eq!(error.status(), 503);
    }
}
