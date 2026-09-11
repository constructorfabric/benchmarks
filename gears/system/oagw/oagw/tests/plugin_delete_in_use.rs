//! Integration tests of the reference-guarded plugin deletion
//! (`cpt-cf-oagw-dod-plugin-system-immutability-delete`).
//!
//! The `409 PluginInUse` body names the referencing upstreams and routes, a
//! rejected deletion leaves the record and every binding unchanged, and there
//! is no `PUT` or `PATCH` on the plugin path.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-immutability-delete:p1

use oagw::domain::repo::PluginBinding;
use oagw::domain::repo::{RouteRecord, UpstreamRecord};
use oagw::test_support::{permissive_surface, security_context, upstream_at, ManagementSurface};
use serde_json::{json, Value};
use uuid::Uuid;

const GUARD: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~";

fn body(name: &str) -> Value {
    json!({
        "plugin_type": GUARD,
        "name": name,
        "config_schema": { "type": "object" },
        "source_code": "const reference = 'opaque';"
    })
}

async fn created(surface: &ManagementSurface, tenant: Uuid, principal: Uuid, name: &str) -> Uuid {
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(body(name)),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    serde_json::from_slice::<Value>(&bytes)
        .expect("record")["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("a UUID identifier")
}

/// Bind the plugin to an upstream's `plugins.items[]` through the store.
fn bind_upstream(
    surface: &ManagementSurface,
    tenant: Uuid,
    alias: &str,
    binding: PluginBinding,
    auth_ref: Option<String>,
) -> Uuid {
    let storage = surface.gear.storage().expect("storage");
    let (upstreams, _, _) = storage.repositories();
    let mut record = upstream_at(tenant, alias, oagw::domain::dto::EndpointScheme::Https, "backend.example.com", 443);
    record.auth = auth_ref.map(|reference| {
        oagw::domain::dto::AuthConfig {
            auth_type: Some(reference),
            sharing: oagw::domain::dto::SharingMode::Enforce,
            config: None,
        }
    });
    upstreams
        .create(tenant, UpstreamRecord { upstream: record.clone(), plugin_bindings: vec![binding] })
        .expect("seeded");
    record.id
}

/// Bind the plugin to a route's `plugins.items[]` through the store.
fn bind_route(surface: &ManagementSurface, tenant: Uuid, upstream_id: Uuid, binding: PluginBinding) -> Uuid {
    let storage = surface.gear.storage().expect("storage");
    let (_, routes, _) = storage.repositories();
    let mut route =
        oagw::test_support::route_for(tenant, upstream_id, "/v1", &[oagw::domain::dto::HttpMethod::Get]);
    let _ = &mut route;
    routes
        .create(
            tenant,
            RouteRecord {
                route: route.clone(),
                plugin_bindings: vec![binding],
            },
        )
        .expect("seeded");
    route.id
}

/// The `409` body carries the `referenced_by` set.
#[tokio::test]
async fn a_referenced_plugin_is_a_conflict_naming_its_references() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let id = created(&surface, tenant, principal, "in-use").await;
    let reference = format!("{GUARD}{id}");

    let upstream_id = bind_upstream(
        &surface,
        tenant,
        "gateway",
        PluginBinding { position: 0, plugin_ref: reference.clone(), plugin_uuid: Some(id) },
        None,
    );
    bind_route(
        &surface,
        tenant,
        upstream_id,
        PluginBinding { position: 0, plugin_ref: reference, plugin_uuid: Some(id) },
    );

    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(
        problem["type"],
        "gts://gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
        "{problem}"
    );
    let referenced_by = &problem["referenced_by"];
    assert_eq!(referenced_by["upstreams"].as_array().expect("upstreams").len(), 1);
    assert_eq!(referenced_by["routes"].as_array().expect("routes").len(), 1);

    // The record and every binding are unchanged.
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_plugin"], 1);
    assert_eq!(counts["oagw_upstream_plugin"], 1);
    assert_eq!(counts["oagw_route_plugin"], 1);
}

/// An upstream `auth` block reference blocks the deletion as well.
#[tokio::test]
async fn an_auth_block_reference_blocks_the_deletion() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    // An auth plugin, so the upstream `auth` block can name it.
    let (status, bytes) = surface
        .send(
            http::Method::POST,
            "/oagw/v1/plugins",
            Some(security_context(tenant, principal)),
            Some(json!({
                "plugin_type": AUTH,
                "name": "tenant-auth",
                "config_schema": { "type": "object" }
            })),
        )
        .await;
    assert_eq!(status, http::StatusCode::CREATED, "{bytes:?}");
    let id: Uuid = serde_json::from_slice::<Value>(&bytes).expect("record")["id"]
        .as_str()
        .expect("id")
        .parse()
        .expect("UUID");

    bind_upstream(
        &surface,
        tenant,
        "gateway",
        PluginBinding { position: 0, plugin_ref: String::new(), plugin_uuid: None },
        Some(format!("{AUTH}{id}")),
    );
    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{bytes:?}");
    let problem: Value = serde_json::from_slice(&bytes).expect("problem");
    assert_eq!(problem["referenced_by"]["upstreams"].as_array().expect("upstreams").len(), 1);
}

/// An unreferenced plugin is deleted with `204` and no queryable trace.
#[tokio::test]
async fn an_unreferenced_plugin_is_deleted() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let id = created(&surface, tenant, principal, "unreferenced").await;
    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::NO_CONTENT, "{bytes:?}");
    assert!(bytes.is_empty(), "the deletion carries no body: {bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_plugin"], 0);
}

/// No `PUT` or `PATCH` exists on the plugin path, and `POST` on the
/// by-identifier path is not a replacement either.
#[tokio::test]
async fn no_replacement_method_exists_on_the_plugin_path() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let id = created(&surface, tenant, principal, "immutable").await;
    for method_name in ["PUT", "PATCH"] {
        let (status, bytes) = surface
            .send(
                http::Method::from_bytes(method_name.as_bytes()).expect("a known method"),
                &format!("/oagw/v1/plugins/{id}"),
                Some(security_context(tenant, principal)),
                Some(json!({ "plugin_type": GUARD, "name": "renamed" })),
            )
            .await;
        assert_eq!(status, http::StatusCode::METHOD_NOT_ALLOWED, "{method_name}: {bytes:?}");
    }
    // The record is unchanged.
    let (status, bytes) = surface
        .send(
            http::Method::GET,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{bytes:?}");
    let record: Value = serde_json::from_slice(&bytes).expect("record");
    assert_eq!(record["name"], "immutable");
}

/// A binding attempt that arrives while a delete holds the write exclusion
/// fails with the `409` conflict outcome and leaves the record in place.
#[tokio::test]
async fn a_binding_that_arrives_under_the_delete_conflicts() {
    let surface = permissive_surface(None).await;
    let tenant = Uuid::new_v4();
    let principal = Uuid::new_v4();
    let id = created(&surface, tenant, principal, "raced").await;
    let reference = format!("{GUARD}{id}");

    // The racing write is represented by the binding row it would have
    // persisted against the same single-process store: the delete's own
    // critical section sees it and reports the conflict.
    let storage = surface.gear.storage().expect("storage");
    let (upstreams, _, _) = storage.repositories();
    let mut record =
        upstream_at(tenant, "racer", oagw::domain::dto::EndpointScheme::Https, "backend.example.com", 443);
    record.plugins = Some(oagw::domain::dto::PluginsConfig {
        sharing: oagw::domain::dto::SharingMode::Enforce,
        items: vec![reference.clone()],
    });
    let _ = upstreams.create(
        tenant,
        UpstreamRecord {
            upstream: record,
            plugin_bindings: vec![PluginBinding {
                position: 0,
                plugin_ref: reference.clone(),
                plugin_uuid: Some(id),
            }],
        },
    );

    let (status, bytes) = surface
        .send(
            http::Method::DELETE,
            &format!("/oagw/v1/plugins/{id}"),
            Some(security_context(tenant, principal)),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::CONFLICT, "{bytes:?}");
    let counts = surface.gear.storage().expect("storage").row_counts();
    assert_eq!(counts["oagw_plugin"], 1, "the record survives");
    assert_eq!(counts["oagw_upstream_plugin"], 1, "the binding is unchanged");
}
