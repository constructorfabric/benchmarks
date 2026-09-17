use std::sync::Arc;

use credstore_sdk::test_util::MockCredStoreClient;
use toolkit_auth::oauth2::ClientAuthMethod;

use super::{
    AuthPluginRegistry, GuardPluginRegistry, NoopAuthPlugin, OAuth2ClientCredAuthPlugin,
    PluginRegistries, RequestIdTransformPlugin, RequiredHeadersGuardPlugin, TokenCacheConfig,
    TransformPluginRegistry,
};
use crate::domain::error::ErrorKind;
use crate::infra::credentials::SecretResolver;

/// Resolver over an empty store; the registry never touches secrets.
fn resolver() -> SecretResolver {
    SecretResolver::new(Arc::new(MockCredStoreClient::empty()))
}

/// Kind of the resolution failure, so assertions stay on the catalog surface.
fn auth_kind(reference: &str) -> ErrorKind {
    kind_of(registries().auth.resolve(reference))
}

/// Kind of a resolution outcome, ignoring the successful case.
fn kind_of<T>(outcome: Result<T, crate::domain::error::DomainError>) -> ErrorKind {
    match outcome {
        Ok(_) => panic!("the plugin must not resolve"),
        Err(error) => error.kind,
    }
}

fn registries() -> PluginRegistries {
    PluginRegistries::with_builtins(&resolver(), &TokenCacheConfig::default())
}

#[test]
fn registries_hold_every_builtin() {
    let registries = registries();
    for id in [
        "noop",
        "apikey",
        "oauth2_client_cred",
        "oauth2_client_cred_basic",
    ] {
        let plugin = registries
            .auth
            .resolve(id)
            .unwrap_or_else(|error| panic!("`{id}` must resolve: {error}"));
        assert_eq!(plugin.id(), id);
    }
    assert!(registries.guard.resolve("required_headers").is_ok());
    assert!(registries.transform.resolve("request_id").is_ok());
}

#[test]
fn plugins_resolve_under_their_gts_identifier_too() {
    let registries = registries();
    let reference = format!(
        "{}{}",
        crate::ids::AUTH_PLUGIN_TYPE,
        crate::ids::AUTH_APIKEY
    );
    let plugin = registries
        .auth
        .resolve(&reference)
        .unwrap_or_else(|error| panic!("`{reference}` must resolve: {error}"));
    assert_eq!(plugin.id(), "apikey");
    assert!(
        registries
            .guard
            .resolve(crate::ids::GUARD_REQUIRED_HEADERS)
            .is_ok()
    );
    assert!(
        registries
            .transform
            .resolve(crate::ids::TRANSFORM_REQUEST_ID)
            .is_ok()
    );
}

#[test]
fn catalog_only_plugins_are_never_resolvable() {
    let registries = registries();
    for reference in crate::ids::CATALOG_ONLY_AUTH {
        assert_eq!(auth_kind(reference), ErrorKind::PluginNotFound);
    }
    for reference in crate::ids::CATALOG_ONLY_GUARD {
        let outcome = registries.guard.resolve(reference);
        assert_eq!(kind_of(outcome), ErrorKind::PluginNotFound);
    }
    for reference in crate::ids::CATALOG_ONLY_TRANSFORM {
        let outcome = registries.transform.resolve(reference);
        assert_eq!(kind_of(outcome), ErrorKind::PluginNotFound);
    }
}

#[test]
fn unregistered_names_and_uuids_are_rejected() {
    assert_eq!(auth_kind("totally-unknown"), ErrorKind::PluginNotFound);
    let unknown = uuid::Uuid::new_v4().to_string();
    assert_eq!(auth_kind(&unknown), ErrorKind::PluginNotFound);
}

#[test]
fn registered_plugins_are_reachable_under_both_keys() {
    let mut auth = AuthPluginRegistry::default();
    auth.register(Arc::new(NoopAuthPlugin));
    assert!(auth.resolve("noop").is_ok());
    assert!(auth.resolve(crate::ids::AUTH_NOOP).is_ok());

    let mut guards = GuardPluginRegistry::default();
    guards.register(Arc::new(RequiredHeadersGuardPlugin));
    assert!(guards.resolve(crate::ids::GUARD_REQUIRED_HEADERS).is_ok());

    let mut transforms = TransformPluginRegistry::default();
    transforms.register(Arc::new(RequestIdTransformPlugin));
    assert!(transforms.resolve(crate::ids::TRANSFORM_REQUEST_ID).is_ok());
}

#[test]
fn oauth2_variants_are_distinct_plugins() {
    let secrets = resolver();
    let mut registry = AuthPluginRegistry::default();
    registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
        secrets.clone(),
        ClientAuthMethod::Form,
        TokenCacheConfig::default(),
    )));
    registry.register(Arc::new(OAuth2ClientCredAuthPlugin::new(
        secrets,
        ClientAuthMethod::Basic,
        TokenCacheConfig::default(),
    )));
    let form = registry
        .resolve("oauth2_client_cred")
        .unwrap_or_else(|error| panic!("form variant: {error}"));
    let basic = registry
        .resolve("oauth2_client_cred_basic")
        .unwrap_or_else(|error| panic!("basic variant: {error}"));
    assert_ne!(form.id(), basic.id());
    assert_ne!(form.plugin_type(), basic.plugin_type());
}

#[test]
fn apikey_plugin_is_built_on_a_secret_resolver() {
    let registry = AuthPluginRegistry::with_builtins(&resolver(), &TokenCacheConfig::default());
    let plugin = registry
        .resolve("apikey")
        .unwrap_or_else(|error| panic!("apikey: {error}"));
    assert_eq!(plugin.id(), "apikey");
}
