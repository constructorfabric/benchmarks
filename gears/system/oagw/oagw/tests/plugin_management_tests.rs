//! Custom plugin management tests.
//!
//! Covers the three flows of DECOMPOSITION §2.4 against the store: the create
//! that stores the verbatim source and never parses or executes it, the one
//! validation error that names every failing property, the duplicate-name
//! 400 that is a validation failure and not a conflict row, the 404 that
//! never distinguishes a foreign identifier from a named plugin, the
//! tenant-scoped list with its closed OData surface, the source path that
//! returns the source alone, and the deletion that answers nothing.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::plugin_def;
use oagw::control_plane::service::ManagementService;
use oagw::control_plane::service::ServiceError;
use oagw::config::OagwConfig;
use oagw::domain::error::ErrorKind;
use oagw::domain::plugin_contract::PluginFamily;
use oagw::store::OagwStore;

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// A management service over its own empty store and cache.
fn service() -> ManagementService {
    let cache = Arc::new(ControlPlaneCache::new());
    ManagementService::new(
        Arc::new(OagwStore::new()),
        &OagwConfig::default(),
        Arc::clone(&cache),
    )
    .expect("the validators compile")
}

/// A valid transform plugin body.
fn transform_body(name: &str) -> Value {
    json!({
        "plugin_type": "transform",
        "name": name,
        "description": "redacts response headers",
        "config_schema": { "type": "object" },
        "phases": ["on_response"],
        "source_code": "def on_response(ctx):\n    return ctx\n"
    })
}

/// Removes one root property from a body.
fn without(body: &Value, key: &str) -> Value {
    let mut body = body.clone();
    body.as_object_mut()
        .expect("the body is an object")
        .remove(key);
    body
}

/// The catalogue row of a refused operation, asserting it is a domain failure.
fn domain_of(error: &ServiceError) -> &oagw::DomainError {
    assert!(!error.is_storage(), "the operation is a domain failure");
    error.domain()
}

#[test]
fn a_created_plugin_is_stored_verbatim_and_unreferenced() {
    let service = service();
    let tenant = tenant(0x10);
    let row = service
        .create_plugin(tenant, &transform_body("redact-headers"))
        .expect("the plugin is created");

    assert_eq!(row.plugin.name, "redact-headers");
    assert_eq!(row.plugin.plugin_type, "transform");
    assert_eq!(
        row.plugin.source_code,
        "def on_response(ctx):\n    return ctx\n",
        "the source is stored verbatim"
    );
    assert_eq!(row.plugin.description.as_deref(), Some("redacts response headers"));
    assert!(row.plugin.config_schema.is_some());
    assert_eq!(row.plugin.phases, ["on_response"]);
    assert!(row.plugin.last_used_at.is_none(), "no use has been recorded");
    assert!(
        row.plugin.gc_eligible_at.is_none(),
        "the create sets no gc_eligible_at"
    );
    assert_eq!(
        plugin_def::plugin_instance(PluginFamily::Transform, row.plugin.id),
        format!("gts.cf.core.oagw.transform_plugin.v1~{}", row.plugin.id),
        "the created id answers as the family's anonymous GTS instance"
    );
}

#[test]
fn a_plugin_without_the_optional_members_is_stored_without_them() {
    let service = service();
    let body = json!({
        "plugin_type": "auth",
        "name": "inject-key",
        "source_code": "def on_request(ctx): pass"
    });
    let row = service
        .create_plugin(tenant(0x10), &body)
        .expect("the plugin is created");
    assert_eq!(row.plugin.description, None);
    assert_eq!(row.plugin.config_schema, None);
    assert_eq!(row.plugin.phases, Vec::<String>::new());
}

#[test]
fn the_source_is_never_parsed_or_executed_at_create_time() {
    let service = service();
    let body = json!({
        "plugin_type": "transform",
        "name": "not-starlark",
        "source_code": "this is not starlark {{{",
        "phases": ["on_request"]
    });
    let row = service
        .create_plugin(tenant(0x10), &body)
        .expect("create-time validation covers the declared fields alone");
    assert_eq!(row.plugin.source_code, "this is not starlark {{{");
}

#[test]
fn one_validation_error_names_every_failing_property() {
    let service = service();
    let body = json!({
        "plugin_type": "transform",
        "name": "broken",
        "config_schema": "not an object",
        "phases": ["on_error", "on_nonexistent"],
        "source_code": "def on_request(ctx): pass",
        "unexpected": true
    });
    let refusal = service
        .create_plugin(tenant(0x10), &body)
        .expect_err("the body fails four properties");
    let error = domain_of(&refusal);
    assert_eq!(error.kind, ErrorKind::ValidationError);
    for property in [
        "unknown property 'unexpected' at root",
        "config_schema",
        "phases[1]",
    ] {
        assert!(
            error.detail.contains(property),
            "'{property}' is named by: {}",
            error.detail
        );
    }
}

#[test]
fn a_phase_outside_the_family_set_is_refused() {
    let service = service();
    let body = json!({
        "plugin_type": "guard",
        "name": "guard-with-error-phase",
        "phases": ["on_error"],
        "source_code": "def on_request(ctx): pass"
    });
    let refusal = service
        .create_plugin(tenant(0x10), &body)
        .expect_err("the guard family admits no error phase");
    let error = domain_of(&refusal);
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(error.detail.contains("phases[0]"), "{}", error.detail);
}

#[test]
fn a_plugin_type_that_names_no_family_is_refused() {
    let service = service();
    let body = json!({
        "plugin_type": "throttle",
        "name": "unknown-family",
        "source_code": "def on_request(ctx): pass"
    });
    let refusal = service
        .create_plugin(tenant(0x10), &body)
        .expect_err("no family answers 'throttle'");
    let error = domain_of(&refusal);
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(error.detail.contains("plugin_type"), "{}", error.detail);
}

#[test]
fn an_empty_source_and_an_empty_name_are_refused() {
    let service = service();
    let body = json!({
        "plugin_type": "transform",
        "name": "",
        "source_code": ""
    });
    let refusal = service
        .create_plugin(tenant(0x10), &body)
        .expect_err("both properties fail");
    let error = domain_of(&refusal);
    assert!(error.detail.contains("name"), "{}", error.detail);
    assert!(error.detail.contains("source_code"), "{}", error.detail);

    for key in ["name", "source_code"] {
        let refusal = service
            .create_plugin(tenant(0x10), &without(&transform_body("absent"), key))
            .expect_err("the property is required");
        assert!(
            domain_of(&refusal).detail.contains(key),
            "'{key}' is named by: {}",
            domain_of(&refusal).detail
        );
    }
}

#[test]
fn a_name_held_by_the_tenant_is_a_validation_failure_and_writes_nothing() {
    let service = service();
    let owner = tenant(0x10);
    service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the first create succeeds");

    let refusal = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect_err("the name is already held");
    let error = domain_of(&refusal);
    assert_eq!(error.kind, ErrorKind::ValidationError, "no 409 row exists");
    assert!(
        error.detail.contains("name"),
        "'name' is named by: {}",
        error.detail
    );
    assert_eq!(
        service.list_plugins(owner, "").expect("the list reads").items.len(),
        1,
        "the refused create wrote no row"
    );
}

#[test]
fn a_name_is_held_within_one_tenant_only() {
    let service = service();
    let body = transform_body("shared-name");
    service
        .create_plugin(tenant(0x10), &body)
        .expect("the first tenant holds the name");
    service
        .create_plugin(tenant(0x20), &body)
        .expect("another tenant holds the same name freely");
}

#[test]
fn a_read_never_distinguishes_a_foreign_row_from_a_missing_one() {
    let service = service();
    let owner = tenant(0x10);
    let row = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let read = service.read_plugin(owner, row.plugin.id).expect("read");
    assert_eq!(read.plugin.id, row.plugin.id);

    for other in [
        // A plugin that does not exist at all.
        Uuid::from_u128(0x99),
        // The same plugin as another tenant addresses it: a foreign row.
        row.plugin.id,
    ] {
        let refusal = service
            .read_plugin(tenant(0x20), other)
            .expect_err("no row of the calling tenant matches");
        let error = domain_of(&refusal);
        assert_eq!(error.kind, ErrorKind::RouteNotFound, "{}", error.detail);
    }
}

#[test]
fn the_source_path_returns_the_source_alone() {
    let service = service();
    let owner = tenant(0x10);
    let row = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let source = service
        .read_plugin_source(owner, row.plugin.id)
        .expect("the source reads");
    assert_eq!(source, row.plugin.source_code);

    let refusal = service
        .read_plugin_source(tenant(0x20), row.plugin.id)
        .expect_err("a foreign row reads nothing");
    assert_eq!(domain_of(&refusal).kind, ErrorKind::RouteNotFound);
}

#[test]
fn the_list_is_tenant_scoped_and_bounded() {
    let service = service();
    let owner = tenant(0x10);
    for name in ["redact-headers", "inject-key", "strip-prefix"] {
        service
            .create_plugin(owner, &transform_body(name))
            .expect("the plugin is created");
    }
    let other = tenant(0x20);
    service
        .create_plugin(
            other,
            &json!({
                "plugin_type": "auth",
                "name": "another-tenants",
                "source_code": "def on_request(ctx): pass"
            }),
        )
        .expect("another tenant's plugin is created");

    let page = service.list_plugins(owner, "").expect("the list reads");
    assert_eq!(page.items.len(), 3, "only the calling tenant's rows");
    assert_eq!(page.projection, Vec::<String>::new());
    assert_eq!(page.top, 50, "the declared default page size");

    let bounded = service
        .list_plugins(owner, "$top=2&$skip=1")
        .expect("the parameters are admitted");
    assert_eq!(bounded.items.len(), 2);
    assert_eq!(bounded.top, 2);

    let filtered = service
        .list_plugins(owner, "$filter=type eq 'transform'&$top=100")
        .expect("the filter is admitted");
    assert_eq!(filtered.items.len(), 3);
    assert_eq!(filtered.top, 100, "the ceiling is admitted");

    let by_name = service
        .list_plugins(owner, "$filter=name eq 'inject-key'")
        .expect("the name filter is admitted");
    assert_eq!(by_name.items.len(), 1);
    assert_eq!(by_name.items[0].plugin.name, "inject-key");

    let foreign = tenant(0x20);
    let unknown = service
        .list_plugins(foreign, "")
        .expect("the list reads");
    assert_eq!(unknown.items.len(), 1);
}

#[test]
fn the_list_surface_admits_no_ordering() {
    let service = service();
    let owner = tenant(0x10);
    service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let refusal = service
        .list_plugins(owner, "$orderby=name")
        .expect_err("the plugin table declares no ordering");
    let error = domain_of(&refusal);
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(
        error.detail.contains("$orderby"),
        "'$orderby' is named by: {}",
        error.detail
    );
}

#[test]
fn the_list_surface_refuses_an_unexposed_projection() {
    let service = service();
    let owner = tenant(0x10);
    service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let refusal = service
        .list_plugins(owner, "$select=id,tenant_id")
        .expect_err("tenant_id is not a plugin list property");
    assert!(domain_of(&refusal).detail.contains("$select"));

    let projected = service
        .list_plugins(owner, "$select=name,source_code")
        .expect("the projection is admitted");
    assert_eq!(projected.projection, ["name", "source_code"]);
}

#[test]
fn a_deleted_plugin_answers_nothing_afterwards() {
    let service = service();
    let owner = tenant(0x10);
    let row = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let deleted = service
        .delete_plugin(owner, row.plugin.id)
        .expect("the deletion is applied");
    assert!(deleted);

    let refusal = service
        .read_plugin(owner, row.plugin.id)
        .expect_err("the row is gone");
    assert_eq!(domain_of(&refusal).kind, ErrorKind::RouteNotFound);

    let refusal = service
        .delete_plugin(owner, row.plugin.id)
        .expect_err("the second deletion addresses nothing");
    assert_eq!(domain_of(&refusal).kind, ErrorKind::RouteNotFound);
}

#[test]
fn a_deletion_never_reaches_another_tenants_row() {
    let service = service();
    let owner = tenant(0x10);
    let row = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");

    let refusal = service
        .delete_plugin(tenant(0x20), row.plugin.id)
        .expect_err("no row of the calling tenant matches");
    assert_eq!(domain_of(&refusal).kind, ErrorKind::RouteNotFound);
    assert!(
        service.read_plugin(tenant(0x10), row.plugin.id).is_ok(),
        "the foreign deletion left the row in place"
    );
}

#[test]
fn an_unbound_plugin_deletion_answers_no_reference_is_held() {
    let service = service();
    let owner = tenant(0x10);
    let row = service
        .create_plugin(owner, &transform_body("redact-headers"))
        .expect("the plugin is created");
    assert!(
        !OagwStore::new().plugin_in_use(owner, row.plugin.id),
        "no reference set holds a freshly created plugin"
    );
}
