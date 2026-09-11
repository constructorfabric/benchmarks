//! Integration tests of the three plugin registries and of the
//! registry-reference-only posture of a custom plugin
//! (`cpt-cf-oagw-dod-plugin-system-registries`,
//! `cpt-cf-oagw-dod-plugin-system-registry-reference-only`).
//!
//! The validation artefact of graded deviations 6 and 10 of the FEATURE:
//! deviation 10 — the six catalog-only identifiers carry no handler, so no
//! registry resolves them and the core timeout, CORS, logging and metrics
//! behavior is unreachable through a plugin binding; deviation 6 — the plugin
//! trait boundary is the sandboxing surface, so the `source_code` a custom
//! plugin registers is an opaque reference artifact with no execution path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use oagw::config::TokenCacheConfig;
use oagw::domain::plugin::composition::ComposedChain;
use oagw::domain::repo::{PluginBinding, RouteRecord, UpstreamRecord};
use oagw::infra::plugin::executor::PluginRuntime;
use oagw::infra::plugin::resolution::{resolve_reference, PluginRegistries, TenantChain};
use oagw::infra::storage::Storage;
use oagw::test_support::{FakeCredStore, permissive_surface};
use serde_json::{Value, json};
use uuid::Uuid;

const AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~";
const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const TRANSFORM: &str = "gts.cf.core.oagw.transform_plugin.v1~";

const AUTH_BUILTINS: [&str; 4] = [
    "noop",
    "apikey",
    "oauth2_client_cred",
    "oauth2_client_cred_basic",
];
const GUARD_BUILTINS: [&str; 1] = ["required_headers"];
const TRANSFORM_BUILTINS: [&str; 1] = ["request_id"];

/// Every catalog-only identifier of graded deviation 10, spelled against the
/// base type the catalog registers it under.
const CATALOG_ONLY: [&str; 6] = [
    "cf.core.oagw.basic.v1",
    "cf.core.oagw.bearer.v1",
    "cf.core.oagw.timeout.v1",
    "cf.core.oagw.cors.v1",
    "cf.core.oagw.logging.v1",
    "cf.core.oagw.metrics.v1",
];

const SOURCE: &str = "const never = 'executed';";

fn registries() -> PluginRegistries {
    let storage = Storage::new();
    let (_upstreams, _routes, plugins) = storage.repositories();
    PluginRegistries::with_builtins(Arc::new(FakeCredStore), TokenCacheConfig::default(), plugins)
}

/// The three registries are constructible through `with_builtins()` and each
/// resolves its own built-in identifiers and nothing else.
#[test]
fn the_three_registries_resolve_their_builtins_only() {
    let registries = registries();

    for label in AUTH_BUILTINS {
        assert!(
            registries.auth.contains(label),
            "`{label}` is a built-in auth plugin"
        );
        assert!(registries.auth.get(label).is_resolved(), "`{label}` resolves");
    }
    for label in GUARD_BUILTINS {
        assert!(registries.guard.get(label).is_resolved(), "`{label}` resolves");
    }
    for label in TRANSFORM_BUILTINS {
        assert!(
            registries.transform.get(label).is_resolved(),
            "`{label}` resolves"
        );
    }

    // No registry crosses a base-type boundary: a built-in of one type is not
    // resolvable through the registry of another.
    for label in [AUTH_BUILTINS.as_slice(), &GUARD_BUILTINS, &TRANSFORM_BUILTINS].concat() {
        assert!(
            !registries.guard.contains(label) || GUARD_BUILTINS.contains(&label),
            "`{label}` never resolves through the guard registry twice"
        );
    }
    assert!(!registries.guard.contains("apikey"), "the guard registry holds no auth plugin");
    assert!(
        !registries.transform.contains("required_headers"),
        "the transform registry holds no guard"
    );
    assert!(!registries.auth.contains("request_id"), "the auth registry holds no transform");

    // The three full GTS identifiers resolve as well as the labels, because a
    // binding carries the full identifier.
    assert!(
        registries.guard.contains("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
        "the full guard identifier resolves"
    );
    assert!(
        registries.transform.contains("gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"),
        "the full transform identifier resolves"
    );
    assert!(
        registries.auth.contains("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
        "the full auth identifier resolves"
    );
}

/// No registry resolves a catalog-only identifier, under its short label or
/// under the full GTS identifier the catalog registers.
#[test]
fn no_registry_resolves_a_catalog_only_identifier() {
    let registries = registries();
    for instance in CATALOG_ONLY {
        let full = |base: &str| format!("{base}{instance}");
        assert!(
            !registries.auth.contains(instance)
                && !registries.auth.contains(full(AUTH).as_str()),
            "`{instance}` never resolves through the auth registry"
        );
        assert!(
            !registries.guard.contains(instance)
                && !registries.guard.contains(full(GUARD).as_str()),
            "`{instance}` never resolves through the guard registry"
        );
        assert!(
            !registries.transform.contains(instance)
                && !registries.transform.contains(full(TRANSFORM).as_str()),
            "`{instance}` never resolves through the transform registry"
        );
    }
}

/// A binding of a catalog-only identifier is rejected at binding time, and the
/// core timeout, CORS, logging and metrics behavior stays unreachable.
#[tokio::test]
async fn a_catalog_only_binding_is_rejected_and_triggers_no_core_behavior() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    for (base, instance) in [
        (AUTH, "cf.core.oagw.basic.v1"),
        (AUTH, "cf.core.oagw.bearer.v1"),
        (GUARD, "cf.core.oagw.timeout.v1"),
        (GUARD, "cf.core.oagw.cors.v1"),
        (TRANSFORM, "cf.core.oagw.logging.v1"),
        (TRANSFORM, "cf.core.oagw.metrics.v1"),
    ] {
        let payload = json!({
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "plugins": { "sharing": "private", "items": [format!("{base}{instance}")] }
        });
        let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST, "{instance}: {bytes:?}");
    }
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream_plugin"], 0, "no rejected binding is stored");
}

/// A custom plugin registered through the management API is addressable,
/// resolvable and bindable, and carries no executable binding.
#[tokio::test]
async fn a_registered_custom_plugin_is_addressable_but_never_executable() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            Some(json!({
                "plugin_type": GUARD,
                "name": "registry-reference-only",
                "config_schema": { "type": "object" },
                "source_code": SOURCE
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    // The record is addressable as `gts.cf.core.oagw.{type}_plugin.v1~{id}`,
    // and its source content is exposed only through the source endpoint.
    let id: Uuid = serde_json::from_value(record["id"].clone()).expect("identifier");
    let reference = format!("{GUARD}{id}");
    assert_eq!(record["plugin_ref"], reference, "the record is addressable by its GTS identifier");
    let (status, source) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{id}/source"),
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{source:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&source).expect("the source body")["source_code"],
        SOURCE,
        "the source reference is carried verbatim"
    );
    let storage = surface.gear.storage().expect("storage");
    let (_upstreams, _routes, plugins) = storage.repositories();
    let registries = PluginRegistries::with_builtins(
        Arc::new(FakeCredStore),
        TokenCacheConfig::default(),
        plugins,
    );
    let resolved = resolve_reference(&registries, &TenantChain::new(vec![tenant]), &reference)
        .expect("the registered plugin resolves at proxy time");
    assert_eq!(resolved.plugin_type, GUARD);
    assert!(
        resolved.binding.is_none(),
        "a registered plugin carries no executable binding: {resolved:?}"
    );

    // A chain that binds it runs no phase for it.
    let runtime = PluginRuntime::new(Arc::new(registries), 2);
    let chain = ComposedChain {
        bindings: vec![PluginBinding { position: 0, plugin_ref: reference, plugin_uuid: Some(id) }],
        auth_ref: None,
    };
    let mut resolved_chain = runtime
        .resolve(&chain, &TenantChain::new(vec![tenant]), None)
        .expect("the chain resolves");
    let mut headers = Vec::new();
    let transformed = runtime
        .run_request(
            &mut resolved_chain,
            "GET",
            "/v1",
            &[],
            &mut headers,
            bytes::Bytes::new(),
            oagw::domain::plugin::Principal::default(),
            "trace",
        )
        .await
        .expect("the chain proceeds");
    assert!(
        resolved_chain.phases.is_empty(),
        "no phase ran for a registry-reference-only plugin: {:?}",
        resolved_chain.phases
    );
    assert!(transformed.headers.is_empty(), "nothing was injected");
}

/// A registered custom plugin is bound by an upstream write and the binding row
/// agrees with the reference, which is what makes it bindable.
#[tokio::test]
async fn a_registered_custom_plugin_is_bindable_by_reference() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            Some(json!({ "plugin_type": GUARD, "name": "bindable", "source_code": SOURCE })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let id: Uuid = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("identifier")
        .parse()
        .expect("uuid");

    let payload = json!({
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
        "plugins": { "sharing": "private", "items": [id.to_string()] }
    });
    let (status, bytes) = surface.create(tenant, Uuid::new_v4(), payload).await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_upstream_plugin"], 1, "the binding row is stored");
}

/// No Starlark or other script interpreter sits behind the plugin trait
/// boundary: a registered plugin bound on a real upstream and route leaves the
/// proxied request and the stub's response untouched, and the source content is
/// retrievable only through the source endpoint.
#[tokio::test]
async fn no_interpreter_sits_behind_the_trait_boundary() {
    use oagw::domain::dto::{EndpointScheme, HttpMethod};

    let surface = permissive_surface(Some(json!({ "allow_http_upstream": true, "proxy_timeout_secs": 5 }))).await;
    let tenant = Uuid::new_v4();
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(oagw::test_support::security_context(tenant, Uuid::new_v4())),
            Some(json!({ "plugin_type": GUARD, "name": "no-interpreter", "source_code": SOURCE })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let id: Uuid = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("identifier")
        .parse()
        .expect("uuid");

    let stub = oagw::test_support::stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let record = oagw::test_support::upstream_at(
        tenant,
        "registered",
        EndpointScheme::Http,
        &host,
        port,
    );
    let upstream_id = record.id;
    {
        let storage = surface.gear.storage().expect("the store is initialized");
        let (upstreams, routes, _) = storage.repositories();
        upstreams
            .create(
                tenant,
                UpstreamRecord {
                    upstream: record,
                    plugin_bindings: vec![PluginBinding {
                        position: 0,
                        plugin_ref: id.to_string(),
                        plugin_uuid: Some(id),
                    }],
                },
            )
            .expect("the upstream is seeded");
        routes
            .create(
                tenant,
                RouteRecord {
                    route: oagw::test_support::route_for(
                        tenant,
                        upstream_id,
                        "/v1",
                        &[HttpMethod::Get],
                    ),
                    plugin_bindings: Vec::new(),
                },
            )
            .expect("the route is seeded");
    }

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/registered/v1/orders", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{:?}", exchange.body);
    let received = stub.requests.lock().expect("the stub log");
    assert_eq!(received.len(), 1, "the request reached the stub");
    assert!(
        received[0]
            .headers
            .iter()
            .all(|(name, _)| name != "x-oagw-registered"),
        "the registered source injected nothing: {:?}",
        received[0].headers
    );
}
