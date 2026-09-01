//! Plugin registry wiring: registers the built-in plugins with the engine's
//! dependency services (credstore client + token cache sizing from config).

use std::sync::Arc;

use toolkit_auth::oauth2::ClientAuthMethod;

use credstore_sdk::CredStoreClientV1;

use crate::config::OagwConfig;
use crate::domain::plugin::builtins::{
    ApiKeyAuthPlugin, NoopAuthPlugin, OAuth2ClientCredAuthPlugin, RequestIdTransformPlugin,
    RequiredHeadersGuardPlugin,
};
use crate::domain::plugin::{AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry};

/// Assemble the plugin registries used by the data plane.
#[must_use]
pub fn build_registries(
    config: &OagwConfig,
    credstore: Option<&Arc<dyn CredStoreClientV1>>,
) -> (
    AuthPluginRegistry,
    GuardPluginRegistry,
    TransformPluginRegistry,
) {
    let mut auth = AuthPluginRegistry::new();
    let mut guard = GuardPluginRegistry::new();
    let mut transform = TransformPluginRegistry::new();

    // Auth
    auth.register(Arc::new(NoopAuthPlugin));
    auth.register(Arc::new(ApiKeyAuthPlugin::new(credstore.cloned())));
    auth.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
        ClientAuthMethod::Form,
        credstore.cloned(),
        config.token_cache_capacity,
        config.token_cache_ttl_secs,
    )));
    auth.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
        ClientAuthMethod::Basic,
        credstore.cloned(),
        config.token_cache_capacity,
        config.token_cache_ttl_secs,
    )));

    // Guards
    guard.register(Arc::new(RequiredHeadersGuardPlugin));

    // Transforms
    transform.register(Arc::new(RequestIdTransformPlugin));

    (auth, guard, transform)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::config::OagwConfig;

    #[test]
    fn builtins_registered_under_gts_ids() {
        let config = OagwConfig::default();
        let (auth, guard, transform) = build_registries(&config, None);

        assert!(auth.get(crate::domain::dto::AUTH_NOOP).is_some());
        assert!(auth.get(crate::domain::dto::AUTH_APIKEY).is_some());
        assert!(auth.get(crate::domain::dto::AUTH_OAUTH2_FORM).is_some());
        assert!(auth.get(crate::domain::dto::AUTH_OAUTH2_BASIC).is_some());
        // Catalog-only ids are NOT resolvable at runtime.
        assert!(auth.get(crate::domain::dto::AUTH_BASIC_RESERVED).is_none());

        assert!(
            guard
                .get(crate::domain::dto::GUARD_REQUIRED_HEADERS)
                .is_some()
        );
        assert!(
            transform
                .get(crate::domain::dto::TRANSFORM_REQUEST_ID)
                .is_some()
        );
    }
}
