//! Integration tests for the gear wiring and the REST registration
//! (`cpt-cf-oagw-dod-gear-foundation-gear-wiring`,
//! `cpt-cf-oagw-dod-gear-foundation-rest-registration`).
//!
//! Every test drives a real `OagwGear` through `Gear::init` over the client
//! fakes of [`oagw::test_support`], so the wiring under test is the wiring the
//! runtime drives.
//!
//! @cpt-dod:cpt-cf-oagw-dod-gear-foundation-gear-wiring:p1
// @cpt-state:cpt-cf-oagw-state-gear-foundation-config-record:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-integration-tests:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-layer-boundaries:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-repository-boundary:p1
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-rest-registration:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use toolkit::Gear;

// @cpt-begin:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-1
// `inst-gf-st-gear-1`: the wiring starts in the `initializing` state.
// @cpt-end:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-1
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-2
// `inst-gf-st-gear-2`: a successful `Gear::init` moves it to `ready`.
// @cpt-end:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-2
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-3
// `inst-gf-st-gear-3`: a rejected dependency or configuration leaves it
// non-ready and reportable.
// @cpt-end:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-3
use oagw::test_support::{context, hub_of, test_context, test_context_with_registry, upstream};

use oagw::{ControlPlaneService, OagwConfig, OagwGear};

// @cpt-begin:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-1
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-2
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-3
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-4
// @cpt-begin:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-4
/// `Gear::init` completes against a valid `oagw.config` block and a second
/// initialization on the same instance is rejected instead of overwriting the
/// state the first one published.
#[tokio::test]
async fn init_completes_against_a_real_config_block_and_is_idempotent() {
    let gear = OagwGear::default();
    let ctx = test_context(Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 7,
        "token_cache_ttl_secs": 60,
        "token_cache_capacity": 512,
        "max_body_size_bytes": 4096
    })));

    gear.init(&ctx).await.expect("the first init succeeds");

    let config = gear.config().expect("the configuration is published");
    assert!(config.allow_http_upstream);
    assert_eq!(config.proxy_timeout_secs, 7);
    assert_eq!(config.token_cache_ttl_secs, 60);
    assert_eq!(config.token_cache_capacity, 512);
    assert_eq!(config.max_body_size_bytes, 4096);
    assert!(gear.storage().is_some(), "the storage is published");
    assert!(gear.service().is_some(), "the service is published");

    let error = gear.init(&ctx).await.expect_err("a second init is rejected");
    assert!(error.to_string().contains("already initialized"), "{error}");

    // The first initialization's state stands: the second attempt replaced
    // nothing.
    assert_eq!(gear.config().expect("config retained").proxy_timeout_secs, 7);
    assert_eq!(gear.storage().expect("storage retained").row_counts().len(), 10);
}
//
// @cpt-end:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-4
// @cpt-end:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-3
// @cpt-end:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-2
// @cpt-end:cpt-cf-oagw-state-gear-foundation-config-record:p1:inst-gf-st-rec-1
// @cpt-end:cpt-cf-oagw-state-gear-foundation-gear-wiring:p1:inst-gf-st-gear-4
//

/// An invalid `oagw.config` block fails initialization with a validation error
/// naming the offending key, and nothing is published.
#[tokio::test]
async fn init_fails_fast_and_publishes_nothing_on_an_invalid_block() {
    for (block, key) in [
        (serde_json::json!({ "proxy_timeout_secs": 0 }), "proxy_timeout_secs"),
        (serde_json::json!({ "token_cache_capacity": 0 }), "token_cache_capacity"),
        (serde_json::json!({ "token_cache_ttl_secs": 0 }), "token_cache_ttl_secs"),
        (serde_json::json!({ "max_body_size_bytes": 100 * 1024 * 1024 + 1 }), "max_body_size_bytes"),
        (serde_json::json!({ "no_such_key": 1 }), "unknown field"),
    ] {
        let gear = OagwGear::default();
        let ctx = test_context(Some(block));
        let error = gear.init(&ctx).await.expect_err("init fails");
        let rendered = error.to_string();
        // An unknown key is rejected by the toolkit lookup itself, an
        // out-of-range value by the gear's own validation; both name the key.
        assert!(
            (rendered.contains("oagw config invalid") || rendered.contains("invalid config"))
                && rendered.contains(key),
            "`{key}` is named by {rendered}"
        );
        assert!(gear.config().is_none(), "no configuration is published");
        assert!(gear.storage().is_none(), "no storage is published");
        assert!(gear.service().is_none(), "no service handle is published");
        assert!(
            hub_of(&ctx).get::<dyn ControlPlaneService>().is_err(),
            "no service is published to the client hub"
        );
    }
}

/// The registered route tree exposes the four documented prefixes gear-relative
/// (`/oagw/v1/...`) with no `/api` segment, and registers no handler before its
/// owning entry delivers one.
#[test]
fn the_registered_route_tree_is_gear_relative_and_yet_unserviced() {
    let tree = oagw::api::rest::route_tree();
    assert!(!tree.is_empty(), "the route tree is declared");

    for prefix in oagw::api::rest::REGISTRABLE_PREFIXES {
        assert!(
            ["upstreams", "routes", "plugins", "proxy"]
                .iter()
                .any(|segment| prefix.contains(segment)),
            "{prefix} is one of the four documented prefixes"
        );
        assert!(!oagw::api::rest::has_api_prefix(prefix), "{prefix} leaks the api prefix");
    }

    let paths: Vec<&str> = tree.iter().map(|route| route.path).collect();
    for expected in [
        oagw::api::rest::UPSTREAMS_PATH,
        oagw::api::rest::ROUTES_PATH,
        oagw::api::rest::PLUGINS_PATH,
        oagw::api::rest::PROXY_PATH,
        oagw::api::rest::PROXY_SUFFIX_PATH,
    ] {
        assert!(paths.contains(&expected), "{expected} is registered");
        assert!(!oagw::api::rest::has_api_prefix(expected), "{expected} leaks the api prefix");
    }

    // Every management path is served under the `/oagw/v1` prefix and none of
    // them is absolute outside it.
    for path in &paths {
        assert!(path.starts_with(oagw::api::rest::PREFIX), "{path} is not gear-relative");
    }
}

/// `register_rest` publishes the service handle so a later entry can resolve it
/// from the client hub, does not error, and - since entry 2.2 - registers the
/// five upstream management routes rather than leaving the tree empty.
#[tokio::test]
async fn register_rest_publishes_the_service_handle_and_the_upstream_routes() {
    let gear = OagwGear::default();
    let ctx = test_context(None);
    gear.init(&ctx).await.expect("init succeeds");

    let hub = hub_of(&ctx);
    assert!(hub.get::<dyn ControlPlaneService>().is_err(), "init resolves handles only");

    let router = toolkit::contracts::RestApiCapability::register_rest(
        &gear,
        &ctx,
        axum::Router::new(),
        &toolkit::api::OpenApiRegistryImpl::new(),
    )
    .expect("register_rest succeeds");
    assert!(router.has_routes(), "entry 2.2 registers the upstream handlers");

    let published = gear.service().expect("the service is published");
    let resolved = hub.get::<dyn ControlPlaneService>().expect("the handle is resolvable");
    assert!(
        std::sync::Arc::ptr_eq(&resolved, &published),
        "the published handle is the one the gear built"
    );
}

/// The gear declares the four dependency handles and the `rest` capability, and
/// a hub missing one of them aborts initialization by naming it.
#[tokio::test]
async fn init_names_a_missing_dependency_handle() {
    let gear = OagwGear::default();
    let ctx = context(
        None,
        None,
        Some(std::sync::Arc::new(oagw::test_support::FakeAuthZResolver)),
        Some(std::sync::Arc::new(oagw::test_support::FakeTenantResolver)),
        Some(std::sync::Arc::new(oagw::test_support::FakeCredStore)),
    );
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("TypesRegistryClient"), "{error}");
    assert_eq!(OagwGear::MODULE_NAME, "oagw");
}

/// The configuration the gear parsed flows through to the service it built, so
/// `allow_http_upstream` gates the `http` scheme at the write boundary.
#[tokio::test]
async fn the_parsed_configuration_gates_the_service() {
    let tenant = uuid::Uuid::new_v4();

    let strict = OagwGear::default();
    strict.init(&test_context(None)).await.expect("init succeeds");
    let mut record = upstream(tenant, "plain");
    record.server.endpoints[0].scheme = oagw::EndpointScheme::Http;
    let error = strict
        .service()
        .expect("service published")
        .create_upstream(tenant, record.clone())
        .expect_err("http is rejected");
    assert!(matches!(error, oagw::DomainError::ValidationError { .. }), "{error}");

    let lifted = OagwGear::default();
    lifted
        .init(&test_context(Some(serde_json::json!({ "allow_http_upstream": true }))))
        .await
        .expect("init succeeds");
    let created = lifted
        .service()
        .expect("service published")
        .create_upstream(tenant, record)
        .expect("http is admitted under the lift");
    assert_eq!(created.alias, "plain");
}

/// `OagwConfig` is the published type the runtime hands over, so the defaults
/// the FEATURE names are the ones the published type carries.
#[test]
fn the_published_config_type_carries_the_documented_defaults() {
    let config = OagwConfig::default();
    assert!(!config.allow_http_upstream);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, 10_000);
    assert_eq!(config.proxy_timeout_secs, 2);
    assert_eq!(config.max_body_size_bytes, 100 * 1024 * 1024);
    assert!(matches!(config.ssrf_policy, oagw::SsrfPolicy::Disabled));
}

/// A fresh gear instance starts from a clean state and initializes.
#[tokio::test]
async fn a_fresh_gear_instance_starts_from_a_clean_state() {
    let registry = std::sync::Arc::new(oagw::test_support::FakeTypesRegistry::new());
    let gear = OagwGear::default();
    gear.init(&test_context_with_registry(None, Arc::clone(&registry)))
        .await
        .expect("a fresh instance initializes");
    assert_eq!(registry.registered_ids().len(), oagw::gts::BASE_TYPES.len());
}
