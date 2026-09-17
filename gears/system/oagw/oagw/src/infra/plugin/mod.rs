//! Plugin infrastructure — built-in implementations and registry assembly
//! (DoD `cpt-cf-oagw-dod-plugin-system-builtins`, component
//! `cpt-cf-oagw-component-plugin-system`, infra layer).
//!
//! Ships the built-in plugins in the `oagw` crate:
//!
//! - **auth**: `noop` ([`NoopAuthPlugin`]), `apikey` ([`ApiKeyAuthPlugin`]),
//!   `oauth2_client_cred` / `oauth2_client_cred_basic`
//!   ([`OAuth2ClientCredAuthPlugin`], Form/Basic variants);
//! - **guard**: `required_headers` ([`RequiredHeadersGuardPlugin`]);
//! - **transform**: `request_id` ([`RequestIdTransformPlugin`]).
//!
//! All built-ins are registered under their canonical GTS identifiers (see
//! [`crate::domain::plugin::ids`]) via [`builtin_registries`], which the gear
//! assembles during initialization.  Catalog-only identifiers (`basic`,
//! `bearer`, `timeout`, `cors`, `logging`, `metrics`) are *not* registered —
//! they are visible to the types-registry catalog but never resolve through a
//! plugin registry (DoD `cpt-cf-oagw-dod-plugin-system-catalog-only`).

pub mod auth;
pub mod guard;
pub mod oauth2;
pub mod transform;

use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::CredStoreClientV1;

use crate::domain::plugin::PluginRegistries;
use crate::domain::plugin::ids::{
    APIKEY_AUTH, NOOP_AUTH, OAUTH2_CLIENT_CRED, OAUTH2_CLIENT_CRED_BASIC,
};

pub use auth::{ApiKeyAuthPlugin, NoopAuthPlugin};
pub use guard::RequiredHeadersGuardPlugin;
pub use oauth2::OAuth2ClientCredAuthPlugin;
pub use transform::RequestIdTransformPlugin;

/// Assembles the plugin registries with every built-in registered under its
/// GTS identifier, using the OAuth2 token-cache defaults.
///
/// `credstore` supplies CredStore credential resolution for the `apikey` and
/// OAuth2 auth plugins (algorithm `cpt-cf-oagw-algo-plugin-system-resolve-secret`);
/// when `None`, those plugins are still registered but their credential lookups
/// fail with `secret.not_found` (the gear always passes its client).
#[must_use]
pub fn builtin_registries(credstore: Option<Arc<dyn CredStoreClientV1>>) -> PluginRegistries {
    let registries = PluginRegistries::new();
    register_builtins(&registries, credstore);
    registries
}

/// Assembles the registries threading the gear's OAuth2 token-cache sizing
/// (`OagwConfig::token_cache_ttl_secs` / `token_cache_capacity`) into the
/// OAuth2 plugins (algorithm `cpt-cf-oagw-algo-plugin-system-oauth2-cache`,
/// step `inst-ps-oauth-ttl`).
#[must_use]
pub fn builtin_registries_with_cache(
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    cache_ttl: Duration,
    cache_capacity: usize,
) -> PluginRegistries {
    let registries = PluginRegistries::new();
    register_builtins_with_cache(&registries, credstore, cache_ttl, cache_capacity);
    registries
}

/// Registers the built-in plugins into the given registries (OAuth2 token
/// cache defaults).
pub fn register_builtins(
    registries: &PluginRegistries,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
) {
    register_builtins_with_cache(
        registries,
        credstore,
        oauth2::DEFAULT_CACHE_TTL,
        oauth2::DEFAULT_CACHE_CAPACITY,
    );
}

/// Registers the built-in plugins with an explicit OAuth2 token-cache sizing.
pub fn register_builtins_with_cache(
    registries: &PluginRegistries,
    credstore: Option<Arc<dyn CredStoreClientV1>>,
    cache_ttl: Duration,
    cache_capacity: usize,
) {
    registries
        .auth
        .register(Arc::new(NoopAuthPlugin { gts_id: NOOP_AUTH }));

    if let Some(cred) = credstore {
        registries.auth.register(Arc::new(ApiKeyAuthPlugin::new(
            Arc::clone(&cred),
            APIKEY_AUTH,
        )));

        // OAuth2 client-credentials — Form and Basic auth-method variants
        // (ADR 0008: registered twice).
        registries
            .auth
            .register(Arc::new(OAuth2ClientCredAuthPlugin::new_with_cache(
                Arc::clone(&cred),
                OAUTH2_CLIENT_CRED,
                oauth2::client_auth_form(),
                None,
                cache_ttl,
                cache_capacity,
            )));
        registries
            .auth
            .register(Arc::new(OAuth2ClientCredAuthPlugin::new_with_cache(
                cred,
                OAUTH2_CLIENT_CRED_BASIC,
                oauth2::client_auth_basic(),
                None,
                cache_ttl,
                cache_capacity,
            )));
    }

    registries
        .guards
        .register(Arc::new(RequiredHeadersGuardPlugin));

    registries
        .transforms
        .register(Arc::new(RequestIdTransformPlugin));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plugin::ids::{
        BASIC_AUTH, BEARER_AUTH, CORS_GUARD, REQUEST_ID_TRANSFORM, REQUIRED_HEADERS_GUARD,
    };

    fn regs() -> PluginRegistries {
        let cred: Arc<dyn CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        builtin_registries(Some(cred))
    }

    #[test]
    fn builtin_registries_register_every_builtin_under_its_gts_id() {
        let regs = regs();
        for id in [
            NOOP_AUTH,
            APIKEY_AUTH,
            OAUTH2_CLIENT_CRED,
            OAUTH2_CLIENT_CRED_BASIC,
            REQUIRED_HEADERS_GUARD,
            REQUEST_ID_TRANSFORM,
        ] {
            match id {
                APIKEY_AUTH | NOOP_AUTH | OAUTH2_CLIENT_CRED | OAUTH2_CLIENT_CRED_BASIC => {
                    assert!(
                        regs.auth.contains(id),
                        "{id} must resolve in the auth registry"
                    );
                }
                REQUIRED_HEADERS_GUARD => {
                    assert!(
                        regs.guards.contains(id),
                        "{id} must resolve in the guard registry"
                    );
                }
                REQUEST_ID_TRANSFORM => {
                    assert!(
                        regs.transforms.contains(id),
                        "{id} must resolve in the transform registry"
                    );
                }
                _ => unreachable!(),
            }
        }
        // Naming a known id must produce a resolvable instance.
        assert!(regs.auth.resolve(APIKEY_AUTH).is_some());
        assert!(regs.guards.resolve(REQUIRED_HEADERS_GUARD).is_some());
        assert!(regs.transforms.resolve(REQUEST_ID_TRANSFORM).is_some());
    }

    #[test]
    fn catalog_only_identifiers_have_no_backing_implementation() {
        // REGISTERED in the types-registry catalog by platform bootstrap, but
        // never resolvable via a plugin registry (DoD
        // `cpt-cf-oagw-dod-plugin-system-catalog-only`).
        let regs = regs();
        for id in [BASIC_AUTH, BEARER_AUTH, CORS_GUARD] {
            assert!(
                !regs.auth.contains(id),
                "{id} must not be in the auth registry"
            );
            assert!(
                !regs.guards.contains(id),
                "{id} must not be in the guard registry"
            );
            assert!(
                !regs.transforms.contains(id),
                "{id} must not be in the transform registry"
            );
        }
    }

    #[test]
    fn full_builtin_set_is_registered_when_credstore_is_available() {
        let with = regs();
        assert_eq!(with.auth.len(), 4);
        assert_eq!(with.guards.len(), 1);
        assert_eq!(with.transforms.len(), 1);

        // Without a CredStore only the noop auth plugin is registered; the
        // credential-using built-ins are skipped.
        let without = builtin_registries(None);
        assert_eq!(without.auth.len(), 1);
        assert!(without.auth.contains(NOOP_AUTH));
    }
}
