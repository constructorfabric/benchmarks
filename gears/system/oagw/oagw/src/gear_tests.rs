//! Gear-level tests: the `Gear::init` sequence
//! (`cpt-cf-oagw-flow-gear-foundation-gear-init`) and the REST declaration
//! (`cpt-cf-oagw-flow-gear-foundation-rest-registration`).
//!
//! Every test builds a real `GearCtx` over the client fakes of
//! [`crate::test_support`], so the wiring under test is the wiring the runtime
//! drives: configuration through the toolkit lookup, dependencies through the
//! client hub, types through the `types-registry` handle.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use toolkit::Gear;
use toolkit_gts::GTS_ID_URI_PREFIX;
use uuid::Uuid;

use crate::api::rest;
use crate::config::{DEFAULT_MAX_BODY_SIZE_BYTES, DEFAULT_TOKEN_CACHE_CAPACITY, MAX_BODY_SIZE_CEILING};
use crate::domain::dto::EndpointScheme;
use crate::domain::gts_helpers::BASE_TYPES;
use crate::infra::type_provisioning;
use crate::test_support::{
    FakeAuthZResolver, FakeCredStore, FakeTenantResolver, FakeTypesRegistry, context, context_without_types_registry,
    failing_registry, hub_of, test_context, test_context_with_registry, unimplemented, upstream,
};
use crate::{OagwConfig, OagwGear};

/// An absent `oagw` block yields the documented defaults.
#[tokio::test]
async fn init_applies_the_documented_defaults() {
    let gear = OagwGear::default();
    let ctx = test_context(None);
    gear.init(&ctx).await.expect("init succeeds");

    let config = gear.config().expect("config published");
    assert!(!config.allow_http_upstream);
    assert_eq!(config.proxy_timeout_secs, 2);
    assert_eq!(config.token_cache_ttl_secs, 300);
    assert_eq!(config.token_cache_capacity, DEFAULT_TOKEN_CACHE_CAPACITY);
    assert_eq!(config.max_body_size_bytes, DEFAULT_MAX_BODY_SIZE_BYTES);
    assert_eq!(config.max_body_size_bytes, MAX_BODY_SIZE_CEILING);
}

/// `inst-gf-init-2`: the declared block overrides the defaults.
#[tokio::test]
async fn init_applies_the_declared_block() {
    let gear = OagwGear::default();
    let ctx = test_context(Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "token_cache_ttl_secs": 60,
        "token_cache_capacity": 100,
        "max_body_size_bytes": 1024
    })));
    gear.init(&ctx).await.expect("init succeeds");

    let config = gear.config().expect("config published");
    assert!(config.allow_http_upstream);
    assert_eq!(config.proxy_timeout_secs, 5);
    assert_eq!(config.token_cache_ttl_secs, 60);
    assert_eq!(config.token_cache_capacity, 100);
    assert_eq!(config.max_body_size_bytes, 1024);
    assert_eq!(OagwConfig::default().ssrf_policy, config.ssrf_policy);
}

/// `inst-gf-init-3`/`-4`: an invalid block aborts initialization before any
/// repository, service or handle exists.
#[tokio::test]
async fn init_fails_fast_on_an_invalid_block() {
    let gear = OagwGear::default();
    let ctx = test_context(Some(serde_json::json!({
        "max_body_size_bytes": MAX_BODY_SIZE_CEILING + 1
    })));
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("oagw config invalid"), "{error}");

    assert!(gear.config().is_none(), "no configuration is published");
    assert!(gear.storage().is_none(), "no storage is published");
    assert!(gear.service().is_none(), "no service is published");
}

/// An unknown configuration key is rejected (`deny_unknown_fields`).
#[tokio::test]
async fn init_rejects_an_unknown_config_key() {
    let gear = OagwGear::default();
    let ctx = test_context(Some(serde_json::json!({ "unknown_key": true })));
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("unknown"), "{error}");
    assert!(gear.config().is_none());
}

/// `inst-gf-init-5`: the repositories, the service and the four dependency
/// handles resolve.
#[tokio::test]
async fn init_publishes_the_storage_and_the_service() {
    let gear = OagwGear::default();
    let ctx = test_context(None);
    gear.init(&ctx).await.expect("init succeeds");

    let storage = gear.storage().expect("storage published");
    let counts = storage.row_counts();
    assert_eq!(counts.len(), 10, "every documented table exists");
    assert!(counts.values().all(|count| *count == 0), "the store starts empty");

    let service = gear.service().expect("service published");
    let tenant = Uuid::new_v4();
    let created = service.create_upstream(tenant, upstream(tenant, "backend")).expect("create");
    assert_eq!(created.alias, "backend");
    assert_eq!(service.list_upstreams(tenant).expect("list").len(), 1);
}

/// `inst-gf-init-5`: a missing dependency handle aborts initialization.
#[tokio::test]
async fn init_fails_when_a_dependency_handle_is_missing() {
    let gear = OagwGear::default();
    let ctx = context_without_types_registry(None);
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("TypesRegistryClient"), "{error}");
    assert!(gear.storage().is_none());
}

/// `inst-gf-init-6`: the six base types register through `types-registry`.
#[tokio::test]
async fn init_registers_the_base_types() {
    let registry = Arc::new(FakeTypesRegistry::new());
    let gear = OagwGear::default();
    let ctx = test_context_with_registry(None, Arc::clone(&registry));
    gear.init(&ctx).await.expect("init succeeds");

    let mut registered = registry.registered_ids();
    registered.sort_unstable();
    // The submitted schema `$id` is the `gts://` URI form of the canonical
    // identifier, which is what the GTS model keys a Type Schema by.
    let mut expected: Vec<String> = BASE_TYPES
        .iter()
        .map(|id| format!("{GTS_ID_URI_PREFIX}{id}"))
        .collect();
    expected.sort_unstable();
    assert_eq!(registered, expected);
}

/// `inst-gf-init-7`/`-8`: a registry that answers an error aborts
/// initialization and leaves nothing published.
#[tokio::test]
async fn init_aborts_when_the_registry_is_unreachable() {
    let gear = OagwGear::default();
    let ctx = test_context_with_registry(None, failing_registry(unimplemented()));
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(
        error.to_string().contains("type provisioning"),
        "the provisioning error is surfaced: {error}"
    );
    assert!(gear.storage().is_none());
    assert!(gear.service().is_none());
}

/// `inst-gf-init-9`: a second `init` on the same gear instance is a rejected
/// no-op, and the first initialization's state stands.
#[tokio::test]
async fn init_twice_is_rejected_without_disturbing_the_first() {
    let registry = Arc::new(FakeTypesRegistry::new());
    let gear = OagwGear::default();
    let ctx = test_context_with_registry(None, Arc::clone(&registry));
    gear.init(&ctx).await.expect("first init succeeds");

    let error = gear.init(&ctx).await.expect_err("second init fails");
    assert!(error.to_string().contains("already initialized"), "{error}");

    // The registry saw exactly one batch: the repeat registration is a no-op.
    assert_eq!(registry.registered_ids().len(), BASE_TYPES.len());
    assert_eq!(gear.config().expect("config still published").proxy_timeout_secs, 2);
    assert_eq!(gear.storage().expect("storage still published").row_counts().len(), 10);
}

/// The service handle `RestApiCapability::register_rest` publishes is
/// reachable from the client hub under the `ControlPlaneService` contract, so
/// a later entry can resolve it without a direct gear reference.
#[tokio::test]
async fn register_rest_publishes_the_service_to_the_client_hub() {
    let gear = OagwGear::default();
    let ctx = test_context(None);
    gear.init(&ctx).await.expect("init succeeds");
    let hub = hub_of(&ctx);
    assert!(
        hub.get::<dyn crate::domain::services::ControlPlaneService>().is_err(),
        "init resolves the handles, `register_rest` publishes the service"
    );

    let router = toolkit::contracts::RestApiCapability::register_rest(
        &gear,
        &ctx,
        axum::Router::new(),
        &toolkit::api::OpenApiRegistryImpl::new(),
    )
    .expect("register_rest succeeds");
    // Entry 2.2 filled the upstream prefix in: the five management operations
    // are live handlers now, so the router is no longer empty. The other three
    // prefixes stay declared and unserviced (`inst-gf-rest-5`).
    assert!(router.has_routes(), "entry 2.2 registers the upstream handlers");

    let resolved = hub
        .get::<dyn crate::domain::services::ControlPlaneService>()
        .expect("the service is published");
    let published = gear.service().expect("service published");
    assert!(Arc::ptr_eq(&resolved, &published));
}

/// `allow_http_upstream: false` (the default) rejects an `http` endpoint.
#[tokio::test]
async fn default_config_rejects_the_http_scheme() {
    let gear = OagwGear::default();
    let ctx = test_context(None);
    gear.init(&ctx).await.expect("init succeeds");
    let service = gear.service().expect("service published");

    let mut candidate = upstream(Uuid::new_v4(), "plain");
    candidate.server.endpoints[0].scheme = EndpointScheme::Http;
    let error = service.create_upstream(candidate.tenant_id, candidate).expect_err("rejected");
    assert!(matches!(error, crate::DomainError::ValidationError { .. }), "{error}");
}

/// `allow_http_upstream: true` admits an `http` endpoint (graded deviation 2).
#[tokio::test]
async fn allow_http_upstream_flows_through_to_the_service() {
    let gear = OagwGear::default();
    let ctx = test_context(Some(serde_json::json!({ "allow_http_upstream": true })));
    gear.init(&ctx).await.expect("init succeeds");
    let service = gear.service().expect("service published");

    let tenant = Uuid::new_v4();
    let mut record = upstream(tenant, "plain");
    record.server.endpoints[0].scheme = EndpointScheme::Http;
    record.server.endpoints[0].port = 80;
    let created = service.create_upstream(tenant, record).expect("accepted");
    assert_eq!(created.alias, "plain");
}

/// The declared route tree keeps every path gear-relative: `/oagw/v1/...`
/// with no leading `/api` segment (`cpt-cf-oagw-constraint-gear-relative-paths`).
///
/// Entry 2.6 adds the fifth plugin operation, `GET /oagw/v1/plugins/{id}/source`,
/// and adds no `PUT` or `PATCH` on the plugin path, because a custom plugin is
/// immutable after creation.
#[test]
fn the_declared_route_tree_is_gear_relative() {
    let tree = rest::route_tree();
    assert_eq!(tree.len(), 15 + 2 * 5, "15 management pairs plus the proxy pairs");
    let plugin_pairs: Vec<(&str, &str)> = tree
        .iter()
        .filter(|route| route.path.starts_with(rest::PLUGINS_PATH))
        .map(|route| (route.method, route.path))
        .collect();
    assert_eq!(
        plugin_pairs,
        [
            ("POST", rest::PLUGINS_PATH),
            ("GET", rest::PLUGINS_PATH),
            ("GET", rest::PLUGIN_BY_ID_PATH),
            ("GET", rest::PLUGIN_SOURCE_PATH),
            ("DELETE", rest::PLUGIN_BY_ID_PATH),
        ],
        "the five plugin operations, with no PUT and no PATCH"
    );
    for route in &tree {
        assert!(!rest::has_api_prefix(route.path), "{} leaks the api prefix", route.path);
        assert!(route.path.starts_with("/oagw/v1/"), "{} is not gear-relative", route.path);
    }
    assert_eq!(rest::REGISTRABLE_PREFIXES.len(), 4);
    for prefix in rest::REGISTRABLE_PREFIXES {
        assert!(prefix.starts_with("/oagw/"), "{prefix} is not gear-relative");
    }
    let proxy_methods: Vec<&str> =
        tree.iter().filter(|r| r.path == rest::PROXY_PATH).map(|r| r.method).collect();
    assert_eq!(proxy_methods, ["GET", "POST", "PUT", "DELETE", "PATCH"]);
}

/// A hub without the `types-registry` handle names the missing client.
#[tokio::test]
async fn init_names_the_missing_client() {
    let ctx = context(
        None,
        None,
        Some(Arc::new(FakeAuthZResolver)),
        Some(Arc::new(FakeTenantResolver)),
        Some(Arc::new(FakeCredStore)),
    );
    let gear = OagwGear::default();
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("TypesRegistryClient"), "{error}");
    assert_eq!(OagwGear::MODULE_NAME, "oagw");
}

/// The base-type schema documents are `$id`-shaped and carry the type
/// identifier (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`).
#[test]
fn the_base_type_schemas_carry_their_identifiers() {
    for id in BASE_TYPES {
        let schema = type_provisioning::base_type_schema(id);
        assert_eq!(schema["$id"].as_str(), Some(format!("{GTS_ID_URI_PREFIX}{id}").as_str()));
        assert_eq!(schema["type"].as_str(), Some("object"));
        assert!(schema["properties"]["id"].is_object());
    }
    assert_eq!(type_provisioning::base_type_schemas().len(), BASE_TYPES.len());
}
