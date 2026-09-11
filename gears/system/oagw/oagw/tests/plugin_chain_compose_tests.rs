//! Plugin chain composition and execution order.
//!
//! Covers `cpt-cf-oagw-algo-chain-compose` end to end through the management
//! service's store and the built-in registries: the auth phase resolved from
//! the upstream's scalar identity columns and absent on a route, the
//! upstream-before-route order within a phase, the stored-position order
//! within a layer, the per-phase sub-chains the declared phases select, and
//! the 503 a binding that no longer resolves is answered with.
//!
//! Realizes `cpt-cf-oagw-algo-chain-compose`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::plugin_def;
use oagw::control_plane::service::ManagementService;
use oagw::domain::plugin_contract::PluginFamily;
use oagw::gts::plugin_catalog;
use oagw::plugins::chain::{self, ComposedAuth, ComposedStep};
use oagw::plugins::PluginRegistries;
use oagw::store::{OagwStore, PluginBinding};
use serde_json::{Value, json};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const GUARD: &str = plugin_catalog::GUARD_REQUIRED_HEADERS;
const TRANSFORM: &str = plugin_catalog::TRANSFORM_REQUEST_ID;
const AUTH_APIKEY: &str = plugin_catalog::AUTH_APIKEY;

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// The next distinct upstream host, so two upstreams of one tenant never
/// collide on the alias the endpoints derive.
fn next_host() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static HOST: AtomicUsize = AtomicUsize::new(0);
    format!("host{}.example.com", HOST.fetch_add(1, Ordering::SeqCst))
}

/// A management service over its own empty store and cache.
fn service() -> ManagementService {
    ManagementService::new(
        Arc::new(OagwStore::new()),
        &oagw::OagwConfig::default(),
        Arc::new(ControlPlaneCache::new()),
    )
    .expect("the validators compile")
}

/// The store the service was built over, for the composition's inputs.
fn store_of(service: &ManagementService) -> &OagwStore {
    service.store()
}

/// The built-in registries the composition resolves named plugins through.
fn registries() -> PluginRegistries {
    PluginRegistries::with_builtins(
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        oagw::plugins::token_cache::TokenCacheConfig::new(
            std::time::Duration::from_secs(300),
            10_000,
        ),
    )
}

/// The named identities the reference resolution reads.
fn named_of(_service: &ManagementService) -> oagw::domain::plugin_contract::NamedPluginRegistry {
    oagw::domain::plugin_contract::NamedPluginRegistry::with_builtins()
}

/// An upstream body whose endpoints derive the alias, carrying the plugins the
/// bindings name.
fn upstream_body(bindings: Vec<Value>) -> Value {
    let mut body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": next_host() }] },
        "protocol": HTTP_PROTOCOL,
        "tags": []
    });
    if !bindings.is_empty() {
        body.as_object_mut()
            .expect("the body is an object")
            .insert(String::from("plugins"), json!({ "items": bindings }));
    }
    body
}

/// One `plugins` item naming a built-in plugin at one position.
fn builtin_item(position: u32, reference: &str) -> Value {
    json!({ "plugin_ref": reference, "position": position })
}

/// Creates the upstream and answers the row.
fn create_upstream(service: &ManagementService, tenant: Uuid, body: &Value) -> oagw::UpstreamRow {
    service
        .create_upstream(tenant, body)
        .expect("the upstream is created")
}

/// A custom transform plugin the calling tenant owns, as its anonymous
/// identifier and its row identifier alongside.
fn custom_plugin(service: &ManagementService, tenant: Uuid, name: &str) -> (Uuid, String) {
    custom_plugin_declaring(service, tenant, name, &["on_response"])
}

/// A custom transform plugin declaring exactly the phases the caller names.
fn custom_plugin_declaring(
    service: &ManagementService,
    tenant: Uuid,
    name: &str,
    phases: &[&str],
) -> (Uuid, String) {
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": name,
                "phases": phases,
                "source_code": "def on_response(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom plugin is created");
    let id = row.plugin.id;
    (id, plugin_def::plugin_instance(PluginFamily::Transform, id))
}

/// The stored binding rows of one upstream, in position order.
fn upstream_bindings(service: &ManagementService, tenant: Uuid, id: Uuid) -> Vec<PluginBinding> {
    store_of(service).upstream_plugin_rows(tenant, id)
}

/// The stored binding rows of one route, in position order.
fn route_bindings(service: &ManagementService, tenant: Uuid, id: Uuid) -> Vec<PluginBinding> {
    store_of(service).route_plugin_rows(tenant, id)
}

/// The identifiers the composed order of one sub-chain runs in, as
/// `layer:position:reference` triples.
fn order_of(steps: &[ComposedStep]) -> Vec<String> {
    steps
        .iter()
        .map(|step| {
            format!(
                "{}:{}:{}",
                if step.upstream_layer() { "u" } else { "r" },
                step.position(),
                step.plugin_ref()
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The auth phase.
// ---------------------------------------------------------------------------

#[test]
fn an_upstream_with_no_auth_plugin_composes_the_noop_behaviour() {
    let service = service();
    let tenant = tenant(0x40);
    let row = create_upstream(&service, tenant, &upstream_body(Vec::new()));

    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &upstream_bindings(&service, tenant, row.upstream.id),
        &[],
    )
    .expect("the chain composes");
    assert!(matches!(chain.auth, ComposedAuth::Noop));
    assert!(chain.guard_request.is_empty() && chain.transform_response.is_empty());
}

#[test]
fn an_upstream_auth_plugin_is_resolved_from_the_scalar_columns() {
    let service = service();
    let tenant = tenant(0x41);
    let mut body = upstream_body(Vec::new());
    body.as_object_mut().expect("the body is an object").insert(
        String::from("auth"),
        json!({ "type": AUTH_APIKEY, "config": { "credential_ref": "cred://api-key" } }),
    );
    let row = create_upstream(&service, tenant, &body);
    let stored = store_of(&service).get_upstream(tenant, row.upstream.id).expect("stored");

    let identity = stored.auth_plugin_ref.expect("the scalar column is written");
    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        Some((
            identity.as_str(),
            stored.auth_plugin_uuid,
            &json!({ "credential_ref": "cred://api-key" }),
        )),
        &upstream_bindings(&service, tenant, row.upstream.id),
        &[],
    )
    .expect("the chain composes");
    assert!(
        matches!(chain.auth, ComposedAuth::Builtin { .. }),
        "a built-in auth plugin resolves to its implementation"
    );
}

#[test]
fn a_custom_auth_plugin_composes_as_its_persisted_row() {
    let service = service();
    let tenant = tenant(0x42);
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "auth",
                "name": "custom-auth",
                "phases": ["on_request"],
                "source_code": "def on_request(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom auth plugin is created");
    let reference = plugin_def::plugin_instance(PluginFamily::Auth, row.plugin.id);
    let mut body = upstream_body(Vec::new());
    body.as_object_mut().expect("the body is an object").insert(
        String::from("auth"),
        json!({ "type": reference, "config": {} }),
    );
    let upstream = create_upstream(&service, tenant, &body);
    let stored = store_of(&service).get_upstream(tenant, upstream.upstream.id).expect("stored");

    let identity = stored.auth_plugin_ref.expect("the scalar column is written");
    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        Some((identity.as_str(), stored.auth_plugin_uuid, &json!({}))),
        &upstream_bindings(&service, tenant, upstream.upstream.id),
        &[],
    )
    .expect("the chain composes");
    match chain.auth {
        ComposedAuth::Custom { row: composed, .. } => {
            assert_eq!(composed.id, row.plugin.id, "the row the chain carries is the bound one");
        }
        other => panic!("a custom auth plugin composes as its row, not {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The composed order within a phase.
// ---------------------------------------------------------------------------

#[test]
fn upstream_positions_run_before_route_positions() {
    let service = service();
    let tenant = tenant(0x43);
    let upstream = create_upstream(
        &service,
        tenant,
        &upstream_body(vec![
            builtin_item(0, GUARD),
            builtin_item(1, TRANSFORM),
        ]),
    );
    let route = service
        .create_route(
            tenant,
            &json!({
                "upstream_id": upstream.upstream.id,
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1,
                "plugins": { "items": [
                    builtin_item(0, TRANSFORM),
                    builtin_item(1, GUARD)
                ] }
            }),
        )
        .expect("the route is created");

    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &upstream_bindings(&service, tenant, upstream.upstream.id),
        &route_bindings(&service, tenant, route.route.id),
    )
    .expect("the chain composes");

    // `[U1, U2] + [R1, R2]` composes to `[U1, U2, R1, R2]` within a phase: the
    // upstream layer runs first, and the stored position decides inside it.
    assert_eq!(
        order_of(&chain.guard_request),
        vec![
            format!("u:0:{GUARD}"),
            format!("r:1:{GUARD}"),
        ]
    );
    assert_eq!(
        order_of(&chain.transform_request),
        vec![format!("u:1:{TRANSFORM}"), format!("r:0:{TRANSFORM}")]
    );
}

#[test]
fn the_stored_position_decides_within_one_layer() {
    let service = service();
    let tenant = tenant(0x44);
    let upstream = create_upstream(
        &service,
        tenant,
        &upstream_body(vec![
            builtin_item(0, TRANSFORM),
            builtin_item(1, GUARD),
            builtin_item(2, TRANSFORM),
        ]),
    );

    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &upstream_bindings(&service, tenant, upstream.upstream.id),
        &[],
    )
    .expect("the chain composes");

    assert_eq!(
        order_of(&chain.transform_request),
        vec![format!("u:0:{TRANSFORM}"), format!("u:2:{TRANSFORM}")]
    );
    assert_eq!(order_of(&chain.guard_request), vec![format!("u:1:{GUARD}")]);
}

// ---------------------------------------------------------------------------
// The per-phase sub-chains.
// ---------------------------------------------------------------------------

#[test]
fn a_custom_row_declares_the_phases_its_row_stored() {
    let service = service();
    let tenant = tenant(0x45);
    let (id, reference) = custom_plugin(&service, tenant, "response-only");
    let upstream = create_upstream(
        &service,
        tenant,
        &upstream_body(vec![json!({
            "plugin_ref": reference,
            "plugin_uuid": id
        })]),
    );

    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &upstream_bindings(&service, tenant, upstream.upstream.id),
        &[],
    )
    .expect("the chain composes");

    assert!(
        chain.transform_request.is_empty(),
        "a row that declares on_response only is absent from the request phase"
    );
    assert_eq!(order_of(&chain.transform_response), vec![format!("u:0:{reference}")]);
}

#[test]
fn every_declared_phase_selects_the_same_composed_order() {
    let service = service();
    let tenant = tenant(0x46);
    let (id, reference) = custom_plugin_declaring(&service, tenant, "error-phase", &["on_error"]);
    let upstream = create_upstream(
        &service,
        tenant,
        &upstream_body(vec![
            builtin_item(0, GUARD),
            json!({ "plugin_ref": reference, "plugin_uuid": id, "position": 1 }),
        ]),
    );

    let chain = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &upstream_bindings(&service, tenant, upstream.upstream.id),
        &[],
    )
    .expect("the chain composes");

    assert_eq!(order_of(&chain.guard_request), vec![format!("u:0:{GUARD}")]);
    assert_eq!(order_of(&chain.guard_response), vec![format!("u:0:{GUARD}")]);
    assert_eq!(order_of(&chain.transform_error), vec![format!("u:1:{reference}")]);
    assert!(chain.transform_request.is_empty());
}

// ---------------------------------------------------------------------------
// The unresolved reference.
// ---------------------------------------------------------------------------

#[test]
fn a_binding_that_no_longer_resolves_is_reported_not_dropped() {
    let service = service();
    let tenant = tenant(0x47);
    // A binding row whose plugin row is gone: the row was deleted after the
    // binding was written, which is the stale set the composition is handed.
    let missing = Uuid::from_u128(0x4747);
    let reference = plugin_def::plugin_instance(PluginFamily::Transform, missing);

    let refused = chain::compose(
        store_of(&service),
        tenant,
        &named_of(&service),
        &registries(),
        None,
        &[PluginBinding {
            position: 0,
            plugin_ref: reference.clone(),
            plugin_uuid: Some(missing),
            config: json!({}),
        }],
        &[],
    )
    .expect_err("the binding no longer resolves");
    assert_eq!(refused.kind, oagw::domain::error::ErrorKind::PluginNotFound);
    assert_eq!(refused.http_status(), 503);
}

#[test]
fn a_binding_of_another_tenant_resolves_to_nothing() {
    let service = service();
    let caller = tenant(0x48);
    let owner = tenant(0x49);
    let (id, reference) = custom_plugin(&service, owner, "foreign");

    let refused = chain::compose(
        store_of(&service),
        caller,
        &named_of(&service),
        &registries(),
        None,
        &[PluginBinding {
            position: 0,
            plugin_ref: reference,
            plugin_uuid: Some(id),
            config: json!({}),
        }],
        &[],
    )
    .expect_err("the reference is not the calling tenant's");
    assert_eq!(refused.kind, oagw::domain::error::ErrorKind::PluginNotFound);
}
