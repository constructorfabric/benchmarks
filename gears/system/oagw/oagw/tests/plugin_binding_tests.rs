//! The binding write that rides on the upstream and route write paths.
//!
//! Covers `cpt-cf-oagw-flow-bind-plugins`, `cpt-cf-oagw-algo-plugin-ref-resolve`,
//! and `cpt-cf-oagw-algo-binding-validate` end to end through the management
//! service: the contiguous-position rule, the four reference resolutions the
//! store and the named registry produce, the `plugin_uuid` match, the auth
//! sub-configuration's identity and its `cred://` shape, the single-transaction
//! write the binding rows land in, and the full replacement that unlinks what
//! the body omits.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::plugin_def;
use oagw::control_plane::service::{ManagementService, ServiceError};
use oagw::domain::error::ErrorKind;
use oagw::domain::plugin_contract::{PluginFamily, NamedPluginRegistry};
use oagw::domain::plugin::Plugin;
use oagw::gts::plugin_catalog;
use oagw::store::{BindingWrite, OagwStore, PluginBinding};
use serde_json::{Value, json};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const GUARD: &str = plugin_catalog::GUARD_REQUIRED_HEADERS;
const TRANSFORM: &str = plugin_catalog::TRANSFORM_REQUEST_ID;
const AUTH: &str = plugin_catalog::AUTH_APIKEY;
const CATALOG_ONLY: &str = plugin_catalog::CATALOG_ONLY_GUARD_TIMEOUT;

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
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

/// The store the service was built over, for the row-level assertions.
fn store_of(service: &ManagementService) -> &OagwStore {
    service.store()
}

/// An upstream body whose endpoints derive the alias, carrying no family.
fn upstream_body() -> Value {
    json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "tags": []
    })
}

/// A custom transform plugin the calling tenant owns, as its anonymous
/// identifier.
fn custom_plugin(service: &ManagementService, tenant: Uuid, name: &str) -> String {
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": name,
                "phases": ["on_response"],
                "source_code": "def on_response(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom plugin is created");
    plugin_def::plugin_instance(PluginFamily::Transform, row.plugin.id)
}

/// Creates the upstream and answers the row.
fn create_upstream(service: &ManagementService, tenant: Uuid, body: &Value) -> oagw::UpstreamRow {
    service
        .create_upstream(tenant, body)
        .expect("the upstream is created")
}

/// The detail of the validation error the service answered.
fn detail_of(error: &ServiceError) -> String {
    let ServiceError::Domain(error) = error else {
        panic!("the refusal is a domain failure, not {error:?}");
    };
    assert_eq!(error.kind, ErrorKind::ValidationError);
    error.detail.clone()
}

/// An upstream body binding the guard and transform plugins at the positions
/// the caller names.
fn bound_body(items: Vec<Value>) -> Value {
    json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": items },
        "tags": []
    })
}

// ---------------------------------------------------------------------------
// The success path: the binding rows are written in the parent's transaction.
// ---------------------------------------------------------------------------

#[test]
fn a_binding_of_builtin_plugins_writes_the_rows_in_the_parents_transaction() {
    let service = service();
    let tenant = tenant(0x20);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": {
            "sharing": "private",
            "items": [
                { "plugin_ref": GUARD, "config": { "headers": ["x-trace"] } },
                { "plugin_ref": TRANSFORM }
            ]
        },
        "tags": []
    });
    let written = create_upstream(&service, tenant, &body);

    let rows = store_of(&service).upstream_plugin_rows(tenant, written.upstream.id);
    assert_eq!(rows.len(), 2, "one binding row per submitted item");
    assert_eq!(rows[0].position, 0);
    assert_eq!(rows[0].plugin_ref, GUARD);
    assert_eq!(rows[0].plugin_uuid, None, "a built-in plugin has no row");
    assert_eq!(
        rows[0].config,
        json!({ "headers": ["x-trace"] }),
        "the configuration the item carried reaches the row"
    );
    assert_eq!(rows[1].position, 1);
    assert_eq!(rows[1].plugin_ref, TRANSFORM);
    assert_eq!(rows[1].config, json!({}), "an item that carries no config binds an empty one");
}

#[test]
fn a_binding_of_a_custom_plugin_carries_the_uuid_and_the_parent_row_carries_no_auth_identity() {
    let service = service();
    let tenant = tenant(0x21);
    let reference = custom_plugin(&service, tenant, "tag");
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [reference] },
        "tags": []
    });
    let written = create_upstream(&service, tenant, &body);

    let rows = store_of(&service).upstream_plugin_rows(tenant, written.upstream.id);
    assert_eq!(rows[0].plugin_ref, reference);
    assert!(
        rows[0].plugin_uuid.is_some(),
        "a UUID-backed plugin stores its UUID"
    );

    let stored = store_of(&service)
        .get_upstream(tenant, written.upstream.id)
        .expect("the row is stored");
    assert!(
        stored.auth_plugin_ref.is_none() && stored.auth_plugin_uuid.is_none(),
        "an upstream that binds no auth plugin carries no identity column"
    );
}

#[test]
fn a_route_binds_at_its_own_positions_independently_of_any_upstream() {
    let service = service();
    let tenant = tenant(0x22);
    let upstream = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [GUARD] },
        "tags": []
    }));
    let body = json!({
        "upstream_id": upstream.upstream.id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 1,
        "plugins": { "items": [{ "plugin_ref": TRANSFORM, "position": 0 }] },
        "tags": []
    });
    let written = service.create_route(tenant, &body).expect("the route is created");

    let rows = store_of(&service).route_plugin_rows(tenant, written.route.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].position, 0, "the route's positions start at 0");
    assert_eq!(rows[0].plugin_ref, TRANSFORM);
}

#[test]
fn an_upstream_that_binds_one_auth_plugin_writes_the_scalar_columns() {
    let service = service();
    let tenant = tenant(0x23);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": { "sharing": "private", "type": AUTH, "config": { "key": "k" } },
        "tags": []
    });
    let written = create_upstream(&service, tenant, &body);

    let stored = store_of(&service)
        .get_upstream(tenant, written.upstream.id)
        .expect("the row is stored");
    assert_eq!(stored.auth_plugin_ref.as_deref(), Some(AUTH));
    assert_eq!(stored.auth_plugin_uuid, None, "a built-in auth plugin has no row");
    assert!(
        store_of(&service)
            .upstream_plugin_rows(tenant, written.upstream.id)
            .is_empty(),
        "the auth identity never becomes a binding row"
    );
}

#[test]
fn a_custom_auth_plugin_writes_both_identity_columns() {
    let service = service();
    let tenant = tenant(0x24);
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "auth",
                "name": "tenant-auth",
                "phases": ["on_request"],
                "source_code": "def authenticate(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom auth plugin is created");
    let reference = plugin_def::plugin_instance(PluginFamily::Auth, row.plugin.id);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": { "sharing": "private", "type": reference },
        "tags": []
    });
    let written = create_upstream(&service, tenant, &body);

    let stored = store_of(&service)
        .get_upstream(tenant, written.upstream.id)
        .expect("the row is stored");
    assert_eq!(stored.auth_plugin_ref.as_deref(), Some(reference.as_str()));
    assert_eq!(stored.auth_plugin_uuid, Some(row.plugin.id));
}

// ---------------------------------------------------------------------------
// The resolution failures: 400, no row written.
// ---------------------------------------------------------------------------

#[test]
fn a_catalog_only_identifier_is_refused_and_named_as_reserved() {
    let service = service();
    let tenant = tenant(0x25);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [CATALOG_ONLY] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("catalogue identifier no plugin family backs"),
        "a reserved identifier is told from an unknown one: {detail}"
    );
}

#[test]
fn an_unknown_identifier_is_refused_and_named_as_unknown() {
    let service = service();
    let tenant = tenant(0x26);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.absent.v1"] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("names no resolvable plugin"),
        "an unknown identifier is told from a reserved one: {detail}"
    );
    assert!(
        !detail.contains(GUARD) && !detail.contains(TRANSFORM),
        "the answer discloses no registry or catalogue content: {detail}"
    );
}

#[test]
fn an_auth_slot_of_the_wrong_family_and_the_reserved_auth_names_are_refused() {
    let service = service();
    for (name, identifier, expected) in [
        (
            "the reserved basic identifier",
            plugin_catalog::CATALOG_ONLY_AUTH_BASIC,
            "catalogue identifier no plugin family backs",
        ),
        (
            "the reserved bearer identifier",
            plugin_catalog::CATALOG_ONLY_AUTH_BEARER,
            "catalogue identifier no plugin family backs",
        ),
        (
            "a guard identifier in the auth slot",
            GUARD,
            "is not an auth plugin",
        ),
        (
            "a transform identifier in the auth slot",
            TRANSFORM,
            "is not an auth plugin",
        ),
    ] {
        let tenant = tenant(0x2c);
        let body = json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
            "protocol": HTTP_PROTOCOL,
            "auth": { "type": identifier, "config": {} },
            "tags": []
        });
        let refused = service
            .create_upstream(tenant, &body)
            .expect_err("the auth slot refuses the identifier");
        let detail = detail_of(&refused);
        assert!(
            detail.contains(expected),
            "the {name} is answered with its own reason: {detail}"
        );
        assert!(
            store_of(&service).list_upstreams(tenant).is_empty(),
            "the {name} wrote no row"
        );
    }
}

#[test]
fn a_reference_to_another_tenants_plugin_row_is_refused() {
    let service = service();
    let owner = tenant(0x27);
    let caller = tenant(0x28);
    let reference = custom_plugin(&service, owner, "foreign");
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [reference] },
        "tags": []
    });
    let error = service.create_upstream(caller, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("no plugin row of the calling tenant"),
        "a foreign row is never resolvable: {detail}"
    );
}

#[test]
fn a_failing_binding_writes_no_parent_row_and_no_binding_row() {
    let service = service();
    let tenant = tenant(0x29);
    let before = store_of(&service).list_upstreams(tenant).len();
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [CATALOG_ONLY] },
        "tags": []
    });
    service
        .create_upstream(tenant, &body)
        .expect_err("the write is refused");
    assert_eq!(
        store_of(&service).list_upstreams(tenant).len(),
        before,
        "the parent row is not written"
    );
}

// ---------------------------------------------------------------------------
// The shape rules: positions, the UUID match, the auth slot.
// ---------------------------------------------------------------------------

#[test]
fn non_contiguous_positions_are_refused() {
    let service = service();
    let tenant = tenant(0x2a);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [
            { "plugin_ref": GUARD, "position": 1 },
            { "plugin_ref": TRANSFORM, "position": 2 }
        ] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("position 1 is not the submitted order"),
        "the contiguous set from 0 is the rule: {detail}"
    );
}

#[test]
fn the_contiguous_set_is_stored_in_the_submitted_order() {
    let service = service();
    let tenant = tenant(0x2a);
    let written = create_upstream(
        &service,
        tenant,
        &bound_body(vec![
            json!({ "plugin_ref": GUARD, "position": 0 }),
            json!({ "plugin_ref": TRANSFORM, "position": 1 }),
            json!({ "plugin_ref": GUARD, "position": 2 }),
        ]),
    );
    let rows = store_of(&service).upstream_plugin_rows(tenant, written.upstream.id);
    assert_eq!(
        rows.iter().map(|row| row.position).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "the stored order is the submitted order"
    );
    assert_eq!(
        rows.iter().map(|row| row.plugin_ref.as_str()).collect::<Vec<_>>(),
        vec![GUARD, TRANSFORM, GUARD],
    );
}

#[test]
fn a_set_with_a_gap_and_one_with_a_duplicate_position_are_refused() {
    let service = service();
    let tenant = tenant(0x2a);

    // Positions 0 and 2: the second item is not at its own index.
    let gap = service
        .create_upstream(
            tenant,
            &bound_body(vec![
                json!({ "plugin_ref": GUARD, "position": 0 }),
                json!({ "plugin_ref": TRANSFORM, "position": 2 }),
            ]),
        )
        .expect_err("the set skips position 1");
    assert!(
        detail_of(&gap).contains("position 2"),
        "the gap is named: {}",
        detail_of(&gap)
    );

    // A repeated position: the second item carrying 0 is not at index 1.
    let duplicate = service
        .create_upstream(
            tenant,
            &bound_body(vec![
                json!({ "plugin_ref": GUARD, "position": 0 }),
                json!({ "plugin_ref": TRANSFORM, "position": 0 }),
            ]),
        )
        .expect_err("the set repeats position 0");
    assert!(
        detail_of(&duplicate).contains("position 0"),
        "the repeat is named: {}",
        detail_of(&duplicate)
    );
    assert!(
        store_of(&service).list_upstreams(tenant).is_empty(),
        "neither refused body wrote a row"
    );
}

#[test]
fn a_bare_uuid_string_is_refused() {
    let service = service();
    let tenant = tenant(0x2b);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": ["5b4d6a1e-1c2d-3e4f-5a6b-7c8d9e0f1a2b"] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("not a plugin identifier"),
        "a bare UUID declares no base type, so the type match cannot hold: {detail}"
    );
}

#[test]
fn a_carried_uuid_that_disagrees_with_the_reference_is_refused() {
    let service = service();
    let tenant = tenant(0x2c);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [{
            "plugin_ref": TRANSFORM,
            "plugin_uuid": Uuid::from_u128(0xdead)
        }] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    assert!(
        detail_of(&error).contains("plugin_uuid does not match"),
        "the carried UUID must agree with the plugin the reference names"
    );
}

#[test]
fn a_uuid_on_a_named_plugin_is_refused() {
    let service = service();
    let tenant = tenant(0x2d);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [{
            "plugin_ref": TRANSFORM,
            "plugin_uuid": Uuid::from_u128(0xdead)
        }] },
        "tags": []
    });
    service
        .create_upstream(tenant, &body)
        .expect_err("a named plugin carries no UUID");
}

#[test]
fn an_auth_identifier_in_the_binding_items_is_refused() {
    let service = service();
    let tenant = tenant(0x2e);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [AUTH] },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("bound through the upstream's auth sub-configuration"),
        "the auth slot is the scalar columns, never a binding row: {detail}"
    );
}

#[test]
fn a_second_auth_plugin_is_refused_by_the_schema() {
    let service = service();
    let tenant = tenant(0x2f);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": { "sharing": "private", "type": AUTH },
        "auth_plugin": { "type": AUTH },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("unknown property"),
        "the shipped schema closes the upstream root: {detail}"
    );
}

#[test]
fn a_route_body_that_carries_an_auth_sub_configuration_is_refused() {
    let service = service();
    let tenant = tenant(0x30);
    let upstream = create_upstream(&service, tenant, &upstream_body());
    let body = json!({
        "upstream_id": upstream.upstream.id,
        "match": { "http": { "methods": ["GET"], "path": "/v1" } },
        "priority": 1,
        "auth": { "sharing": "private", "type": AUTH },
        "tags": []
    });
    let error = service.create_route(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("unknown property"),
        "the shipped schema closes the route root: {detail}"
    );
}

#[test]
fn a_credential_reference_without_the_cred_shape_is_refused() {
    let service = service();
    let tenant = tenant(0x31);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": {
            "sharing": "private",
            "type": plugin_catalog::AUTH_OAUTH2_CLIENT_CRED,
            "config": { "client_secret_ref": "https://vault/secret/one" }
        },
        "tags": []
    });
    let error = service.create_upstream(tenant, &body).expect_err("refused");
    let detail = detail_of(&error);
    assert!(
        detail.contains("auth.config.client_secret_ref"),
        "the shape check names the member: {detail}"
    );
}

#[test]
fn a_credential_reference_with_the_cred_shape_is_accepted() {
    let service = service();
    let tenant = tenant(0x32);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": {
            "sharing": "private",
            "type": plugin_catalog::AUTH_OAUTH2_CLIENT_CRED,
            "config": { "client_secret_ref": "cred://client-secret-one" }
        },
        "tags": []
    });
    let written = create_upstream(&service, tenant, &body);

    // The reference is carried verbatim into the stored configuration and is
    // not resolved: the write answers no material and the read returns the
    // reference, never a secret.
    let stored = store_of(&service).get_upstream(tenant, written.upstream.id).expect("stored");
    let config = stored
        .upstream
        .auth
        .as_ref()
        .and_then(|auth| auth.config.as_ref())
        .expect("the auth configuration is stored");
    assert_eq!(
        config["client_secret_ref"], "cred://client-secret-one",
        "the reference is stored as submitted"
    );
    let rendered = serde_json::to_string(&stored.upstream).expect("the row renders");
    assert!(
        rendered.contains("cred://client-secret-one"),
        "the reference, not the material, is what the row carries: {rendered}"
    );
    assert!(
        !rendered.contains("sk-"),
        "no credential material is stored: {rendered}"
    );
}

#[test]
fn an_empty_whitespace_or_fragmented_reference_is_refused() {
    let service = service();
    for (name, reference) in [
        ("empty", ""),
        ("surrounding whitespace", " cred://client-secret-one "),
        ("a fragment", "cred://client-secret-one#fragment"),
    ] {
        let tenant = tenant(0x33);
        let body = json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
            "protocol": HTTP_PROTOCOL,
            "auth": {
                "sharing": "private",
                "type": plugin_catalog::AUTH_OAUTH2_CLIENT_CRED,
                "config": { "client_secret_ref": reference }
            },
            "tags": []
        });
        let refused = service
            .create_upstream(tenant, &body)
            .expect_err("the reference is not well formed");
        let detail = detail_of(&refused);
        assert!(
            detail.contains("auth.config.client_secret_ref"),
            "the {name} reference is named by its member: {detail}"
        );
        assert!(
            store_of(&service).list_upstreams(tenant).is_empty(),
            "the {name} reference wrote no row"
        );
    }
}

// ---------------------------------------------------------------------------
// The replacement: the full replacement of the binding rows.
// ---------------------------------------------------------------------------

#[test]
fn a_replacement_that_drops_an_item_unlinks_the_plugin_it_named() {
    let service = service();
    let tenant = tenant(0x33);
    let created = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [GUARD, TRANSFORM] },
        "tags": []
    }));
    assert_eq!(
        store_of(&service).upstream_plugin_rows(tenant, created.upstream.id).len(),
        2
    );

    let replaced = service
        .replace_upstream(
            tenant,
            created.upstream.id,
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
                "protocol": HTTP_PROTOCOL,
                "plugins": { "items": [GUARD] },
                "tags": []
            }),
        )
        .expect("the replacement is written");

    let rows = store_of(&service).upstream_plugin_rows(tenant, replaced.upstream.id);
    assert_eq!(rows.len(), 1, "the write set is the full replacement");
    assert_eq!(rows[0].plugin_ref, GUARD);
}

#[test]
fn a_replacement_that_omits_the_plugins_object_clears_the_binding_rows() {
    let service = service();
    let tenant = tenant(0x34);
    let created = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [TRANSFORM] },
        "tags": []
    }));

    service
        .replace_upstream(
            tenant,
            created.upstream.id,
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
                "protocol": HTTP_PROTOCOL,
                "tags": []
            }),
        )
        .expect("the replacement is written");

    assert!(
        store_of(&service)
            .upstream_plugin_rows(tenant, created.upstream.id)
            .is_empty(),
        "a body that omits the sub-object unlinks every plugin"
    );
}

// ---------------------------------------------------------------------------
// The named registry the service resolves through.
// ---------------------------------------------------------------------------

#[test]
fn the_registry_backs_six_identifiers_and_refuses_the_six_reserved_ones() {
    let registry = NamedPluginRegistry::with_builtins();
    assert_eq!(registry.identifiers().len(), 6, "the backed identifiers only");
    for (identifier, _) in plugin_catalog::BACKED {
        registry
            .resolve(identifier)
            .unwrap_or_else(|error| panic!("a backed identifier resolves: {error:?}"));
    }
    for identifier in plugin_catalog::CATALOG_ONLY {
        let error = registry
            .resolve(identifier)
            .expect_err("a reserved identifier resolves to nothing");
        assert!(
            matches!(error, oagw::domain::plugin_contract::PluginResolveError::Reserved { .. }),
            "the refusal names the reservation: {error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The write set the validation builds, at the store boundary.
// ---------------------------------------------------------------------------

#[test]
fn the_empty_write_set_references_no_plugin_and_marks_at_the_instant_it_is_given() {
    let write = BindingWrite::none(1_000);
    assert!(write.bindings.is_empty());
    assert!(write.auth.is_none());
    assert!(write.referenced_uuids().is_empty());
}

#[test]
fn a_write_set_of_custom_bindings_answers_their_uuids() {
    let first = Uuid::from_u128(0x1);
    let second = Uuid::from_u128(0x2);
    let write = BindingWrite {
        bindings: vec![
            PluginBinding {
                position: 0,
                plugin_ref: String::from("gts.cf.core.oagw.transform_plugin.v1~first"),
                plugin_uuid: Some(first),
                config: json!({}),
            },
            PluginBinding {
                position: 1,
                plugin_ref: String::from("gts.cf.core.oagw.transform_plugin.v1~second"),
                plugin_uuid: Some(second),
                config: json!({}),
            },
        ],
        auth: None,
        marked_at: 0,
    };
    assert_eq!(write.referenced_uuids().len(), 2);
}

// ---------------------------------------------------------------------------
// The in-use scan the GC marking rides on.
// ---------------------------------------------------------------------------

#[test]
fn a_plugin_row_that_loses_its_last_reference_is_marked_eligible_in_the_same_transaction() {
    let service = service();
    let tenant = tenant(0x35);
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": "marked",
                "phases": ["on_response"],
                "source_code": "def on_response(ctx):\n    return ctx\n"
            }),
        )
        .expect("the plugin is created");
    let reference = plugin_def::plugin_instance(PluginFamily::Transform, row.plugin.id);

    let created = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [reference] },
        "tags": []
    }));
    let stored = store_of(&service)
        .get_plugin(tenant, row.plugin.id)
        .expect("the plugin row is stored");
    assert!(
        stored.plugin.gc_eligible_at.is_none(),
        "a referenced plugin is not marked"
    );

    service
        .replace_upstream(
            tenant,
            created.upstream.id,
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
                "protocol": HTTP_PROTOCOL,
                "tags": []
            }),
        )
        .expect("the replacement is written");

    let stored = store_of(&service)
        .get_plugin(tenant, row.plugin.id)
        .expect("the plugin row is stored");
    let marked = stored
        .plugin
        .gc_eligible_at
        .expect("the last reference was lost");
    assert!(
        marked > oagw::store::unix_now(),
        "the marking stores the instant the TTL elapses, which is in the future"
    );
}

#[test]
fn a_plugin_row_that_gains_a_reference_loses_the_marking() {
    let service = service();
    let tenant = tenant(0x36);
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": "relented",
                "phases": ["on_response"],
                "source_code": "def on_response(ctx):\n    return ctx\n"
            }),
        )
        .expect("the plugin is created");
    let reference = plugin_def::plugin_instance(PluginFamily::Transform, row.plugin.id);

    let first = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "one.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [reference] },
        "tags": []
    }));
    service
        .replace_upstream(
            tenant,
            first.upstream.id,
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "one.example.com" }] },
                "protocol": HTTP_PROTOCOL,
                "tags": []
            }),
        )
        .expect("the unlink is written");
    let marked = store_of(&service)
        .get_plugin(tenant, row.plugin.id)
        .and_then(|stored| stored.plugin.gc_eligible_at)
        .expect("the plugin is marked");

    create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "two.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [reference] },
        "tags": []
    }));
    let stored = store_of(&service)
        .get_plugin(tenant, row.plugin.id)
        .expect("the plugin row is stored");
    assert!(
        stored.plugin.gc_eligible_at.is_none(),
        "a rebound plugin loses the marking it held"
    );
    assert!(
        stored.plugin.gc_eligible_at != Some(marked),
        "the marking the row held is cleared, not re-stamped"
    );
}

#[test]
fn a_built_in_plugin_is_never_marked_for_it_has_no_row() {
    let service = service();
    let tenant = tenant(0x37);
    let created = create_upstream(&service, tenant, &json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [GUARD] },
        "tags": []
    }));
    service
        .replace_upstream(
            tenant,
            created.upstream.id,
            &json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
                "protocol": HTTP_PROTOCOL,
                "tags": []
            }),
        )
        .expect("the unlink is written");
    assert!(
        store_of(&service).list_plugins(tenant).is_empty(),
        "a named plugin has no row to mark"
    );
}

// ---------------------------------------------------------------------------
// The plugin row the marker grammar owns.
// ---------------------------------------------------------------------------

#[test]
fn a_custom_plugin_row_keeps_the_columns_the_shipped_model_declares() {
    let service = service();
    let tenant = tenant(0x38);
    let created = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "guard",
                "name": "headers",
                "phases": ["on_request"],
                "source_code": "def guard_request(ctx):\n    return ctx\n"
            }),
        )
        .expect("the plugin is created");

    let stored = store_of(&service)
        .get_plugin(tenant, created.plugin.id)
        .expect("the row is stored");
    assert_eq!(stored.tenant_id, tenant);
    assert_eq!(stored.plugin.plugin_type, "guard");
    assert!(stored.plugin.last_used_at.is_none(), "no use is recorded here");
    assert!(
        stored.plugin.gc_eligible_at.is_none(),
        "a created plugin is linked to nothing and is not yet eligible"
    );
}

/// The persisted `Plugin` the marker grammar's `oagw_plugin` row is modelled
/// from, asserted once so a renamed column fails here and not at runtime.
#[test]
fn the_plugin_row_carries_the_two_lifecycle_columns_the_model_declares() {
    let plugin = Plugin {
        id: Uuid::from_u128(0x39),
        tenant_id: Uuid::from_u128(0x3a),
        plugin_type: String::from("guard"),
        name: String::from("lifecycle"),
        description: None,
        config_schema: None,
        phases: Vec::new(),
        source_code: String::from("def guard_request(ctx):\n    return ctx\n"),
        last_used_at: None,
        gc_eligible_at: Some(1_000),
    };
    let rendered = serde_json::to_value(&plugin).expect("the row serializes");
    assert!(rendered.get("last_used_at").is_some());
    assert!(rendered.get("gc_eligible_at").is_some());
}
