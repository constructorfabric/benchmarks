//! Integration tests for the preserved DESIGN §3.6 schema contract
//! (`cpt-cf-oagw-dod-gear-foundation-schema-contract`).
//!
//! Persistence is in-memory (graded deviation 5), so what the substitution must
//! preserve is the documented table shape — one child table per documented
//! table, the `oagw_upstream` unique key `(tenant_id, alias)`, and the ordered
//! plugin binding positions contiguous from zero.
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-schema-contract:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use toolkit::Gear;

use oagw::domain::repo::TableShapes;
use oagw::domain::services::ControlPlaneService;
use oagw::test_support::{test_context, upstream};
use oagw::{DomainError, OagwGear, PluginsConfig, SharingMode};

/// The store the gear publishes, plus the service over it.
async fn gear() -> (std::sync::Arc<dyn ControlPlaneService>, oagw::infra::storage::Storage) {
    let oagw_gear = OagwGear::default();
    oagw_gear.init(&test_context(None)).await.expect("init succeeds");
    let storage = oagw_gear.storage().expect("storage published");
    (oagw_gear.service().expect("service published"), (*storage).clone())
}

/// Register one custom plugin in `tenant`'s catalog and return its identifier,
/// so a UUID-backed binding reference names a row the caller holds.
fn catalogue(
    service: &std::sync::Arc<dyn ControlPlaneService>,
    tenant: Uuid,
    name: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    service
        .create_plugin(
            tenant,
            oagw::domain::dto::Plugin {
                id,
                tenant_id: tenant,
                plugin_type: oagw::domain::gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID.to_owned(),
                name: name.to_owned(),
                config_schema: None,
                source_code: Some("const opaque = true;".to_owned()),
                last_used_at: None,
                gc_eligible_at: None,
            },
        )
        .expect("the catalog row is created");
    id
}

/// Every documented DESIGN §3.6 table exists and starts empty.
#[tokio::test]
async fn every_documented_table_is_preserved() {
    let (_, storage) = gear().await;
    let shapes = oagw::infra::storage::Storage::table_shapes();
    assert_eq!(shapes, TableShapes::ALL, "every documented shape is preserved");

    let counts = storage.row_counts();
    for name in oagw::domain::repo::TableShapes::tables().keys() {
        assert_eq!(counts.get(name), Some(&0), "{name} exists and starts empty");
    }
    assert_eq!(counts.len(), 10, "one table per documented DESIGN §3.6 table");
}

/// The `oagw_upstream` unique key `(tenant_id, alias)` is enforced, and the
/// rejected second write leaves the table unchanged.
#[tokio::test]
async fn the_upstream_unique_key_is_preserved() {
    let (service, storage) = gear().await;
    let tenant = Uuid::new_v4();

    let first = service.create_upstream(tenant, upstream(tenant, "vendor.io")).expect("first");
    let error = service
        .create_upstream(tenant, upstream(tenant, "vendor.io"))
        .expect_err("conflict");
    assert!(error.is_conflict(), "{error} is a conflict");
    assert_eq!(storage.row_counts()["oagw_upstream"], 1, "the store is unchanged");
    assert_eq!(
        service.get_upstream_by_alias(tenant, "vendor.io").expect("resolves").id,
        first.id
    );

    // A different tenant holds the same alias: the key is the pair.
    let other = Uuid::new_v4();
    service
        .create_upstream(other, upstream(other, "vendor.io"))
        .expect("the same alias is free in another tenant");
    assert_eq!(storage.row_counts()["oagw_upstream"], 2);
}

/// The `auth_plugin_ref` / `auth_plugin_uuid` scalar columns survive: a
/// built-in reference stores the reference only, and a UUID-backed reference
/// stores both columns in agreement.
#[tokio::test]
async fn the_upstream_auth_columns_are_preserved() {
    let (service, _) = gear().await;
    let tenant = Uuid::new_v4();

    let mut built_in = upstream(tenant, "builtin");
    built_in.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: Some("gts.cf.core.oagw.auth_plugin.bearer.v1~".to_owned()),
        config: Some(serde_json::json!({ "scheme": "Bearer" })),
        ..oagw::AuthConfig::default()
    });
    let created = service.create_upstream(tenant, built_in).expect("created");
    assert_eq!(
        created.auth.as_ref().and_then(|auth| auth.auth_type.as_deref()),
        Some("gts.cf.core.oagw.auth_plugin.bearer.v1~"),
        "the auth plugin reference is stored as a scalar column"
    );
}

/// Ordered plugin binding positions are the list indices, contiguous from zero,
/// with `plugin_uuid` set only for a UUID-backed reference.
#[tokio::test]
async fn plugin_binding_positions_are_contiguous_from_zero() {
    let (service, storage) = gear().await;
    let tenant = Uuid::new_v4();

    // A UUID-backed reference resolves through the caller's plugin catalog, so
    // the catalog row is created before the binding that points at it.
    let uuid_backed = catalogue(&service, tenant, "catalogued").to_string();
    let mut record = upstream(tenant, "vendor.io");
    record.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![
            "gts.cf.core.oagw.guard_plugin.required_headers.v1~".to_owned(),
            uuid_backed.clone(),
            "gts.cf.core.oagw.transform_plugin.header_injection.v1~".to_owned(),
        ],
    });

    let created = service.create_upstream(tenant, record).expect("created");
    let items = created.plugins.as_ref().expect("plugins stored").items.clone();
    assert_eq!(items.len(), 3, "the ordered chain is stored in full");

    // Three binding rows, one per position, contiguous from zero.
    assert_eq!(storage.row_counts()["oagw_upstream_plugin"], 3);
    assert_eq!(storage.row_counts()["oagw_upstream"], 1);
    assert_eq!(storage.row_counts()["oagw_route_plugin"], 0);

    // Replacing the record with a shorter chain rewrites the child rows, so no
    // stale binding row survives a replacement.
    let mut replacement = created;
    replacement.plugins.as_mut().expect("plugins present").items =
        vec!["gts.cf.core.oagw.guard_plugin.required_headers.v1~".to_owned()];
    service.replace_upstream(tenant, replacement).expect("replaced");
    assert_eq!(storage.row_counts()["oagw_upstream_plugin"], 1, "no stale binding row");
}

/// The tag tables and the route child tables are populated as their documented
/// shapes require, and an upstream delete cascades to every child row.
#[tokio::test]
async fn the_child_tables_cascade_with_their_parent() {
    let (service, storage) = gear().await;
    let tenant = Uuid::new_v4();

    let mut record = upstream(tenant, "vendor.io");
    record.tags = vec!["team-a".to_owned(), "tier-1".to_owned()];
    record.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec!["gts.cf.core.oagw.guard_plugin.required_headers.v1~".to_owned()],
    });
    let created = service.create_upstream(tenant, record).expect("created");

    let route = oagw::Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: created.id,
        match_type: oagw::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: oagw::MatchConfig {
            http: Some(oagw::HttpMatch {
                methods: vec![oagw::HttpMethod::Get, oagw::HttpMethod::Post],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: oagw::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: Some(PluginsConfig {
            sharing: SharingMode::Private,
            items: vec![catalogue(&service, tenant, "route-plugin").to_string()],
        }),
        tags: vec!["route-tag".to_owned()],
    };
    service.create_route(tenant, route).expect("route created");

    let counts = storage.row_counts();
    assert_eq!(counts["oagw_upstream_tag"], 2, "one tag row per tag");
    assert_eq!(counts["oagw_route_tag"], 1);
    assert_eq!(counts["oagw_route_method"], 2, "one method row per allowed method");
    assert_eq!(counts["oagw_route_http_match"], 1);
    assert_eq!(counts["oagw_route_grpc_match"], 0, "no grpc row for an http route");
    assert_eq!(counts["oagw_route_plugin"], 1);

    service.delete_upstream(tenant, created.id).expect("deleted");
    let counts = storage.row_counts();
    assert_eq!(counts["oagw_upstream"], 0);
    assert_eq!(counts["oagw_route"], 0, "the route cascaded");
    assert_eq!(counts["oagw_route_method"], 0, "the method rows cascaded");
    assert_eq!(counts["oagw_route_http_match"], 0);
    assert_eq!(counts["oagw_route_plugin"], 0, "the route bindings cascaded");
    assert_eq!(counts["oagw_upstream_tag"], 0, "the tag rows cascaded");
    assert_eq!(counts["oagw_upstream_plugin"], 0, "the upstream bindings cascaded");
}

/// A rejected write leaves every table unchanged: the atomicity the schema
/// contract requires of a record plus its child rows.
#[tokio::test]
async fn a_rejected_write_leaves_no_partial_record() {
    let (service, storage) = gear().await;
    let tenant = Uuid::new_v4();
    service.create_upstream(tenant, upstream(tenant, "vendor.io")).expect("created");
    let before = storage.row_counts();

    // A route under a non-existent upstream is a not-found, and writes nothing.
    let mut orphan = oagw::Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: Uuid::new_v4(),
        match_type: oagw::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: oagw::MatchConfig {
            http: Some(oagw::HttpMatch {
                methods: vec![oagw::HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: oagw::PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    };
    let error = service.create_route(tenant, orphan.clone()).expect_err("orphan rejected");
    assert!(matches!(error, DomainError::NotFound { .. }), "{error}");
    assert_eq!(storage.row_counts(), before, "nothing was written");

    // A secret in the payload is rejected before the store sees the record.
    orphan.tags = vec!["tag".to_owned()];
    orphan.upstream_id = service.get_upstream_by_alias(tenant, "vendor.io").expect("found").id;
    orphan.match_.http.as_mut().expect("http match").path = "/v2".to_owned();
    let mut smuggled = upstream(tenant, "other.io");
    smuggled.auth = Some(oagw::AuthConfig {
        sharing: oagw::SharingMode::Private,
        auth_type: None,
        config: Some(serde_json::json!({ "api_key_ref": "sk-raw-secret" })),
        ..oagw::AuthConfig::default()
    });
    assert!(service.create_upstream(tenant, smuggled).is_err(), "the secret is rejected");
    assert_eq!(storage.row_counts(), before, "the rejected record was not stored");
}

/// A UUID-backed plugin reference is resolved through the caller's catalog
/// before any binding row is stored, so a tenant can never persist a binding to
/// a plugin record it does not hold (`inst-ps-bind-2` .. `-8`).
#[tokio::test]
async fn a_binding_to_a_plugin_outside_the_callers_tenant_is_rejected() {
    let (service, storage) = gear().await;
    let tenant = Uuid::new_v4();
    let foreign = Uuid::new_v4();

    let mut record = upstream(tenant, "vendor.io");
    record.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![
            oagw::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            foreign.to_string(),
        ],
    });

    let before = storage.row_counts();
    let error = service.create_upstream(tenant, record).expect_err("rejected");
    assert!(
        matches!(error, DomainError::NotFound { resource_type: "plugin" }),
        "the unresolved reference is a not-found: {error:?}"
    );
    assert_eq!(storage.row_counts(), before, "no record and no binding row was written");

    // The same reference, once the caller holds the catalog row, is accepted.
    let held = catalogue(&service, tenant, "held");
    let mut record = upstream(tenant, "vendor.io");
    record.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![
            oagw::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID.to_owned(),
            held.to_string(),
        ],
    });
    let created = service.create_upstream(tenant, record).expect("created");
    assert_eq!(created.plugins.expect("plugins").items.len(), 2);
}
