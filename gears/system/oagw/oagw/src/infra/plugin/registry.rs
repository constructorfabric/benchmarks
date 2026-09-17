//! Assembly of the built-in plugin registry.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use toolkit_auth::oauth2::ClientAuthMethod;

use crate::domain::plugin::PluginRegistry;
use crate::infra::plugin::apikey_auth::ApiKeyAuthPlugin;
use crate::infra::plugin::noop_auth::NoopAuthPlugin;
use crate::infra::plugin::oauth2_client_cred_auth::OAuth2ClientCredentialsPlugin;
use crate::infra::plugin::request_id_transform::RequestIdTransformPlugin;
use crate::infra::plugin::required_headers_guard::RequiredHeadersGuardPlugin;
use crate::infra::plugin::secret::CredentialSource;

/// Build the built-in plugin registry for the given credential source.
#[must_use]
pub fn builtin_registry(secrets: CredentialSource, token_cache_capacity: usize) -> PluginRegistry {
    let mut registry = PluginRegistry::new();
    registry.register_auth(Arc::new(NoopAuthPlugin::new()));
    registry.register_auth(Arc::new(ApiKeyAuthPlugin::new(secrets.clone())));
    if let Ok(oauth2) =
        OAuth2ClientCredentialsPlugin::new(secrets.clone(), token_cache_capacity)
    {
        registry.register_auth(Arc::new(oauth2));
    } else {
        tracing::warn!("oauth2_client_credentials plugin unavailable (http client build failed)");
    }
    if let Ok(oauth2_basic) = OAuth2ClientCredentialsPlugin::with_auth_method(
        secrets,
        token_cache_capacity,
        ClientAuthMethod::Basic,
    ) {
        registry.register_auth(Arc::new(oauth2_basic));
    } else {
        tracing::warn!(
            "oauth2_client_cred_basic plugin unavailable (http client build failed)"
        );
    }
    registry.register_guard(Arc::new(RequiredHeadersGuardPlugin));
    registry.register_transform(Arc::new(RequestIdTransformPlugin));
    registry
}

/// Build the built-in plugin registry using a credstore client from the hub.
#[must_use]
pub fn builtin_registry_with_credstore(
    client: Option<Arc<dyn CredStoreClientV1>>,
    token_cache_capacity: usize,
) -> PluginRegistry {
    builtin_registry(CredentialSource::new(client), token_cache_capacity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;

    /// Install a rustls crypto provider: building an HTTP client (as the two
    /// OAuth2 plugins do) needs one, and nothing else in a unit test ran the
    /// toolkit bootstrap.
    fn init_test_crypto() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let _ = rustls::crypto::CryptoProvider::install_default(
                rustls::crypto::aws_lc_rs::default_provider(),
            );
        });
    }

    #[tokio::test]
    async fn registry_exposes_the_built_in_plugins() {
        init_test_crypto();
        let registry = builtin_registry(CredentialSource::inline_only(), 64);
        assert!(registry.auth("apikey").is_some());
        // The short name follows the DESIGN §Plugin catalog id
        // `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1`.
        assert!(registry.auth("oauth2_client_cred").is_some());
        assert!(registry.auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1").is_some());
        assert!(registry.guard("required_headers").is_some());
        assert!(registry.transform("request_id").is_some());
        assert!(registry.auth("unknown").is_none());
    }

    #[tokio::test]
    async fn registry_lookups_accept_fully_qualified_ids() {
        init_test_crypto();
        let registry = builtin_registry_with_credstore(
            Some(Arc::new(MockCredStoreClient::empty()) as Arc<dyn CredStoreClientV1>),
            64,
        );
        assert!(
            registry
                .auth("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1")
                .is_some()
        );
    }
}
