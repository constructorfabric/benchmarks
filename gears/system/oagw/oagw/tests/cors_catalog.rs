//! Integration tests of the catalog-only `cors` guard identifier
//! (`cpt-cf-oagw-flow-cors-catalog-identifier`,
//! `cpt-cf-oagw-dod-cors-catalog-identifier`): the identifier is cataloged and
//! resolves through no plugin registry, and a binding naming it is rejected at
//! binding time and stored nowhere.
// @cpt-dod:cpt-cf-oagw-dod-cors-catalog-identifier:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use oagw::config::TokenCacheConfig;
use oagw::domain::DomainError;
use oagw::infra::plugin::resolution::{PluginRegistries, TenantChain, resolve_reference};
use oagw::infra::storage::Storage;
use oagw::test_support::{FakeCredStore, permissive_surface};
use serde_json::{Value, json};
use uuid::Uuid;

const CORS_GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";
const CORS_INSTANCE: &str = "cf.core.oagw.cors.v1";

fn registries() -> PluginRegistries {
    let storage = Storage::new();
    let (_upstreams, _routes, plugins) = storage.repositories();
    PluginRegistries::with_builtins(Arc::new(FakeCredStore), TokenCacheConfig::default(), plugins)
}

/// The `cors` guard identifier is registered in the types catalog and in no
/// plugin registry: no registry resolves it and no reference resolves it.
#[tokio::test]
async fn the_cors_guard_identifier_resolves_through_no_registry() {
    let registries = registries();
    assert!(
        !registries.guard.contains(CORS_INSTANCE) && !registries.guard.contains(CORS_GUARD),
        "no guard plugin named `cors` is registered"
    );
    for reference in [CORS_INSTANCE, CORS_GUARD] {
        let resolved =
            resolve_reference(&registries, &TenantChain::new(vec![Uuid::new_v4()]), reference);
        assert!(
            matches!(&resolved, Err(DomainError::PluginNotFound { plugin_ref }) if plugin_ref == reference),
            "`{reference}` resolves through no registry: {resolved:?}"
        );
    }
}

/// A `plugins.items[].plugin_ref` binding naming the `cors` guard identifier is
/// rejected at binding time and nothing is stored.
#[tokio::test]
async fn a_cors_guard_binding_is_rejected_and_nothing_is_stored() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let (status, created) = surface
        .create(
            tenant,
            Uuid::new_v4(),
            json!({
                "alias": "api.vendor.com",
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
                "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
                "plugins": { "items": [CORS_GUARD] }
            }),
        )
        .await;
    assert_eq!(status, http::StatusCode::BAD_REQUEST, "{created:?}");
    let body: Value = serde_json::from_slice(&created).expect("the rejection body");
    assert_eq!(
        body["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1",
        "{body}"
    );
    assert!(body["detail"].as_str().expect("detail").contains("catalog only"), "{body}");

    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("the list body");
    assert_eq!(listed["count"], json!(0), "no binding and no record was stored: {listed}");
}
