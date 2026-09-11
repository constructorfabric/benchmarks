//! Unit tests for the plugin registries and chain ordering.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    AuthPluginRegistry, GuardPluginRegistry, TransformPluginRegistry, auth_plugin_id,
    custom_plugin_id, is_catalogue_only,
};
use crate::domain::error::OagwError;
use crate::domain::merge::merge_plugins;
use crate::domain::model::{AuthConfig, PluginRef, PluginsConfig};
use crate::domain::plugin::oauth2_client_cred::{self, TokenFetcher};
use crate::gts_helpers;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// The status an id lookup fails with; registries hold non-`Debug` plugins.
fn status_of<T>(result: Result<T, OagwError>) -> u16 {
    match result {
        Err(error) => error.status(),
        Ok(_) => panic!("the identifier must not resolve"),
    }
}

fn auth_config(plugin_type: &str) -> AuthConfig {
    AuthConfig {
        plugin_type: plugin_type.to_owned(),
        sharing: crate::domain::model::SharingMode::default(),
        config: BTreeMap::new(),
    }
}

fn token_fetcher() -> Arc<dyn TokenFetcher> {
    Arc::new(RefusingFetcher)
}

struct RefusingFetcher;

#[async_trait::async_trait]
impl oauth2_client_cred::TokenFetcher for RefusingFetcher {
    async fn fetch(
        &self,
        _endpoint: &str,
        _method: oauth2_client_cred::ClientAuthMethod,
        _client_id: &crate::domain::plugin::Credential,
        _client_secret: &crate::domain::plugin::Credential,
        _scopes: Option<&str>,
    ) -> Result<oauth2_client_cred::FetchedToken, OagwError> {
        Err(OagwError::AuthenticationFailed("refused".to_owned()))
    }
}

#[test]
fn the_auth_registry_carries_every_built_in() {
    let registry = AuthPluginRegistry::with_builtins(&token_fetcher());

    assert_eq!(
        registry.get(gts_helpers::AUTH_NOOP).unwrap().id(),
        gts_helpers::AUTH_NOOP
    );
    assert_eq!(
        registry.get(gts_helpers::AUTH_APIKEY).unwrap().id(),
        gts_helpers::AUTH_APIKEY
    );
    assert_eq!(
        registry
            .get(gts_helpers::AUTH_OAUTH2_CLIENT_CRED)
            .unwrap()
            .id(),
        gts_helpers::AUTH_OAUTH2_CLIENT_CRED
    );
    assert_eq!(
        registry
            .get(gts_helpers::AUTH_OAUTH2_CLIENT_CRED_BASIC)
            .unwrap()
            .id(),
        gts_helpers::AUTH_OAUTH2_CLIENT_CRED_BASIC
    );
}

#[test]
fn a_catalogue_only_identifier_has_no_implementation() {
    let registry = AuthPluginRegistry::with_builtins(&token_fetcher());

    for id in [
        gts_helpers::AUTH_BASIC,
        gts_helpers::AUTH_BEARER,
        gts_helpers::GUARD_TIMEOUT,
        gts_helpers::GUARD_CORS,
        gts_helpers::TRANSFORM_LOGGING,
        gts_helpers::TRANSFORM_METRICS,
    ] {
        assert!(is_catalogue_only(id), "{id} is catalogue only");
        let error = registry.get(id).err().expect("catalogue ids are unusable");
        assert_eq!(error.status(), 503, "{id} must be a 503");
        assert_eq!(
            error.type_id(),
            gts_helpers::error_type_id("plugin.not_found")
        );
    }
}

#[test]
fn an_unknown_identifier_is_also_a_503() {
    let registry = AuthPluginRegistry::with_builtins(&token_fetcher());

    assert_eq!(
        status_of(registry.get("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.made-up.v1")),
        503
    );
}

#[test]
fn the_guard_registry_carries_the_required_headers_guard() {
    let registry = GuardPluginRegistry::with_builtins();

    assert_eq!(
        registry
            .get(gts_helpers::GUARD_REQUIRED_HEADERS)
            .unwrap()
            .id(),
        gts_helpers::GUARD_REQUIRED_HEADERS
    );
    assert_eq!(status_of(registry.get("no-such-guard")), 503);
}

#[test]
fn the_transform_registry_carries_the_request_id_transform() {
    let registry = TransformPluginRegistry::with_builtins();

    assert_eq!(
        registry
            .get(gts_helpers::TRANSFORM_REQUEST_ID)
            .unwrap()
            .id(),
        gts_helpers::TRANSFORM_REQUEST_ID
    );
    assert_eq!(status_of(registry.get("no-such-transform")), 503);
}

#[test]
fn an_inserted_plugin_resolves_by_its_own_identifier() {
    let mut registry = GuardPluginRegistry::new();
    registry.insert(Arc::new(
        crate::domain::plugin::required_headers_guard::RequiredHeadersGuardPlugin,
    ));

    assert!(registry.get(gts_helpers::GUARD_REQUIRED_HEADERS).is_ok());
}

#[test]
fn the_selected_auth_plugin_is_the_configured_one() {
    assert_eq!(
        auth_plugin_id(&auth_config(gts_helpers::AUTH_APIKEY)),
        gts_helpers::AUTH_APIKEY
    );
}

#[test]
fn a_custom_plugin_identifier_is_tenant_scoped() {
    let tenant = Uuid::from_u128(42);
    assert_eq!(
        custom_plugin_id("gts.cf.core.oagw.auth_plugin.v1~acme.v1", tenant),
        format!("gts.cf.core.oagw.auth_plugin.v1~acme.v1:{tenant}")
    );
}

#[test]
fn upstream_plugins_run_before_route_plugins() {
    let upstream = PluginsConfig {
        sharing: crate::domain::model::SharingMode::default(),
        items: vec![
            PluginRef::GtsId("upstream-1".to_owned()),
            PluginRef::GtsId("upstream-2".to_owned()),
        ],
    };
    let route = PluginsConfig {
        sharing: crate::domain::model::SharingMode::default(),
        items: vec![
            PluginRef::GtsId("route-1".to_owned()),
            PluginRef::GtsId("route-2".to_owned()),
        ],
    };

    let ids = |items: Vec<crate::domain::merge::PluginBinding>| -> Vec<String> {
        items.into_iter().map(|binding| binding.id).collect()
    };
    assert_eq!(
        ids(merge_plugins(&upstream, Some(&route))),
        vec!["upstream-1", "upstream-2", "route-1", "route-2"]
    );
    assert_eq!(
        ids(merge_plugins(&upstream, None)),
        vec!["upstream-1", "upstream-2"]
    );
}
