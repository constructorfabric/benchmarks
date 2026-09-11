//! The plugin row lifecycle: in-use protection, garbage-collection
//! eligibility, and the periodic job.
//!
//! Covers `cpt-cf-oagw-algo-plugin-inuse-gc` and
//! `cpt-cf-oagw-state-plugin-lifecycle` end to end through the management
//! service and the store: the scalar-column reference scan that answers 409,
//! the marking a binding write that removes a reference performs in its own
//! transaction, the clearing a rebinding performs, and the job that marks a
//! row that never gained a reference and deletes only the rows whose TTL has
//! elapsed and whose reference set is still empty when it runs.
//!
//! Realizes `cpt-cf-oagw-dod-plugin-inuse-gc`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::plugin_def;
use oagw::control_plane::service::{ManagementService, ServiceError};
use oagw::domain::error::ErrorKind;
use oagw::domain::plugin_contract::PluginFamily;
use oagw::gts::plugin_catalog;
use oagw::store::{OagwStore, PLUGIN_GC_TTL_SECS};
use serde_json::{Value, json};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const CATALOG_ONLY_TRANSFORM: &str = plugin_catalog::CATALOG_ONLY_TRANSFORM_LOGGING;

/// The next distinct upstream host, so two upstreams of one tenant never
/// collide on the alias the endpoints derive.
fn next_host() -> String {
    static HOST: AtomicUsize = AtomicUsize::new(0);
    format!("host{}.example.com", HOST.fetch_add(1, Ordering::SeqCst))
}

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
        "server": { "endpoints": [{ "scheme": "https", "host": next_host() }] },
        "protocol": HTTP_PROTOCOL,
        "tags": []
    })
}

/// A custom transform plugin the calling tenant owns, as its anonymous
/// identifier, with its row identifier alongside.
fn custom_plugin(service: &ManagementService, tenant: Uuid, name: &str) -> (Uuid, String) {
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
    let id = row.plugin.id;
    (id, plugin_def::plugin_instance(PluginFamily::Transform, id))
}

/// Creates the upstream and answers the row.
fn create_upstream(service: &ManagementService, tenant: Uuid, body: &Value) -> oagw::UpstreamRow {
    service
        .create_upstream(tenant, body)
        .expect("the upstream is created")
}

/// Binds one custom plugin to a fresh upstream and answers both identifiers.
fn bind_custom_plugin(service: &ManagementService, tenant: Uuid, reference: &str, uuid: Uuid) -> Uuid {
    bind_with_body(service, tenant, reference, uuid).0
}

/// Binds one custom plugin to a fresh upstream and answers its identifier and
/// the body the upstream was created with, so a replacement can restate the
/// endpoints the alias was derived from.
fn bind_with_body(
    service: &ManagementService,
    tenant: Uuid,
    reference: &str,
    uuid: Uuid,
) -> (Uuid, Value) {
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": next_host() }] },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": [ { "plugin_ref": reference, "plugin_uuid": uuid } ] },
        "tags": []
    });
    let id = create_upstream(service, tenant, &body).upstream.id;
    (id, body)
}

/// The same upstream body with the `plugins` member removed, which is the full
/// replacement that unlinks every binding the upstream carried.
fn without_plugins(body: &Value) -> Value {
    let mut replacement = body.clone();
    replacement
        .as_object_mut()
        .expect("the body is an object")
        .remove("plugins");
    replacement
}

/// The `gc_eligible_at` the row carries, or `None` when the row is gone.
fn eligibility_of(service: &ManagementService, tenant: Uuid, id: Uuid) -> Option<Option<u64>> {
    store_of(service)
        .get_plugin(tenant, id)
        .map(|row| row.plugin.gc_eligible_at)
}

/// The domain failure the service answered, for the status assertions.
fn domain_of(error: &ServiceError) -> &oagw::domain::error::DomainError {
    let ServiceError::Domain(error) = error else {
        panic!("the refusal is a domain failure, not {error:?}");
    };
    error
}

/// The 409 the in-use protection answers with.
fn in_use_error(error: &ServiceError) -> String {
    let refused = domain_of(error);
    assert_eq!(refused.kind, ErrorKind::PluginInUse);
    refused.detail.clone()
}

// ---------------------------------------------------------------------------
// In-use protection: the scalar-column reference scan and the 409 it answers.
// ---------------------------------------------------------------------------

#[test]
fn an_upstream_binding_row_refuses_the_deletion_with_409() {
    let service = service();
    let tenant = tenant(0x30);
    let (id, reference) = custom_plugin(&service, tenant, "bound");
    bind_custom_plugin(&service, tenant, &reference, id);

    let refused = service.delete_plugin(tenant, id).expect_err("in use");
    let detail = in_use_error(&refused);
    assert_eq!(domain_of(&refused).http_status(), 409);
    assert!(
        !detail.contains("upstream") && !detail.contains("route"),
        "the answer names no referencing resource: {detail}"
    );
    assert!(
        eligibility_of(&service, tenant, id).is_some(),
        "the row is left in place"
    );
}

#[test]
fn a_route_binding_row_refuses_the_deletion_with_409() {
    let service = service();
    let tenant = tenant(0x31);
    let (id, reference) = custom_plugin(&service, tenant, "routed");
    let upstream = create_upstream(&service, tenant, &upstream_body());
    let body = json!({
        "upstream_id": upstream.upstream.id,
        "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
        "priority": 1,
        "enabled": true,
        "plugins": { "items": [ { "plugin_ref": reference, "plugin_uuid": id } ] }
    });
    service
        .create_route(tenant, &body)
        .expect("the route is created");

    let refused = service.delete_plugin(tenant, id).expect_err("in use");
    assert_eq!(domain_of(&refused).http_status(), 409);
    assert!(
        eligibility_of(&service, tenant, id).is_some(),
        "the row is left in place"
    );
}

#[test]
fn an_upstream_auth_column_refuses_the_deletion_with_409() {
    let service = service();
    let tenant = tenant(0x32);
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "auth",
                "name": "apikey",
                "phases": ["on_request"],
                "source_code": "def authenticate(ctx):\n    return ctx\n"
            }),
        )
        .expect("the auth plugin is created");
    let id = row.plugin.id;
    let reference = plugin_def::plugin_instance(PluginFamily::Auth, id);
    let body = json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com" }] },
        "protocol": HTTP_PROTOCOL,
        "auth": { "type": reference, "config": { "secret_ref": "cred://api-key" } },
        "tags": []
    });
    create_upstream(&service, tenant, &body);

    let refused = service.delete_plugin(tenant, id).expect_err("in use");
    assert_eq!(domain_of(&refused).http_status(), 409);
}

#[test]
fn an_unlinked_plugin_is_deleted_and_its_binding_rows_are_untouched() {
    let service = service();
    let tenant = tenant(0x33);
    let (id, _) = custom_plugin(&service, tenant, "loose");

    assert!(
        service.delete_plugin(tenant, id).expect("not in use"),
        "the deletion reports the row it removed"
    );
    assert!(eligibility_of(&service, tenant, id).is_none(), "the row is gone");
}

#[test]
fn a_named_plugin_has_no_row_and_no_lifecycle() {
    let service = service();
    let tenant = tenant(0x34);

    let refused = service
        .delete_plugin(tenant, Uuid::max())
        .expect_err("no row to delete");
    assert_eq!(domain_of(&refused).http_status(), 404, "a named plugin has no row");

    let report = service.run_plugin_garbage_collection().expect("the job runs");
    assert!(
        report.marked.is_empty() && report.collected.is_empty(),
        "a store with no custom row is left untouched: {report:?}"
    );
    assert!(
        plugin_catalog::is_catalog_only(CATALOG_ONLY_TRANSFORM),
        "the catalog-only identifier stays reserved"
    );
}

// ---------------------------------------------------------------------------
// Garbage-collection eligibility: the marking and clearing a binding write does.
// ---------------------------------------------------------------------------

#[test]
fn losing_the_last_reference_marks_the_row_and_rebinding_clears_it() {
    let service = service();
    let tenant = tenant(0x35);
    let (id, reference) = custom_plugin(&service, tenant, "rebind");
    let (parent, body) = bind_with_body(&service, tenant, &reference, id);

    // The full replacement that omits the item unlinks the only reference.
    service
        .replace_upstream(tenant, parent, &without_plugins(&body))
        .expect("the replacement is accepted");

    let marked = eligibility_of(&service, tenant, id).expect("the row survives");
    assert!(
        marked.is_some(),
        "a plugin that lost its last reference is marked"
    );

    bind_custom_plugin(&service, tenant, &reference, id);
    assert_eq!(
        eligibility_of(&service, tenant, id),
        Some(None),
        "a plugin that gained a reference loses the marking"
    );
}

#[test]
fn the_marking_lands_in_the_same_transaction_as_the_binding_write() {
    let service = service();
    let tenant = tenant(0x36);
    let (id, reference) = custom_plugin(&service, tenant, "atomic");
    let (parent, body) = bind_with_body(&service, tenant, &reference, id);

    // A replacement that fails validation writes nothing, so the marking the
    // unlinked row would have earned is written no more than the rows are.
    let mut refused = body.clone();
    refused.as_object_mut().expect("the body is an object").insert(
        String::from("plugins"),
        json!({ "items": [{ "plugin_ref": plugin_catalog::CATALOG_ONLY_GUARD_TIMEOUT }] }),
    );
    assert!(
        service.replace_upstream(tenant, parent, &refused).is_err(),
        "the reserved identifier is refused"
    );
    assert_eq!(
        eligibility_of(&service, tenant, id),
        Some(None),
        "no marking is written for a failed write"
    );
}

#[test]
fn last_used_at_is_never_written_by_the_lifecycle() {
    let service = service();
    let tenant = tenant(0x37);
    let (id, reference) = custom_plugin(&service, tenant, "unused");
    let (parent, body) = bind_with_body(&service, tenant, &reference, id);
    service
        .replace_upstream(tenant, parent, &without_plugins(&body))
        .expect("the replacement is accepted");
    service.run_plugin_garbage_collection().expect("the job runs");

    let row = store_of(&service).get_plugin(tenant, id).expect("the row is stored");
    assert!(
        row.plugin.last_used_at.is_none(),
        "no lifecycle operation writes last_used_at"
    );
}

// ---------------------------------------------------------------------------
// The periodic job: marking, collecting, and leaving everything else alone.
// ---------------------------------------------------------------------------

#[test]
fn the_job_marks_a_plugin_that_was_never_bound() {
    let service = service();
    let tenant = tenant(0x38);
    let (id, _) = custom_plugin(&service, tenant, "never-bound");

    let report = service.run_plugin_garbage_collection().expect("the job runs");
    assert_eq!(report.marked, vec![id], "the job's own scan marks the row");
    let marked = eligibility_of(&service, tenant, id).expect("the row survives");
    assert!(
        marked > Some(0),
        "the marking stores the instant the TTL elapses"
    );
    assert_eq!(
        report.collected,
        Vec::<Uuid>::new(),
        "a freshly marked row is not collectable on the run that marks it"
    );
}

#[test]
fn the_job_marks_nothing_that_is_already_linked() {
    let service = service();
    let tenant = tenant(0x39);
    let (id, reference) = custom_plugin(&service, tenant, "linked");
    bind_custom_plugin(&service, tenant, &reference, id);

    let report = service.run_plugin_garbage_collection().expect("the job runs");
    assert!(
        report.marked.is_empty() && report.collected.is_empty(),
        "a referenced row is neither marked nor collected: {report:?}"
    );
    assert_eq!(
        eligibility_of(&service, tenant, id),
        Some(None),
        "the row's eligibility is left unset"
    );
}

#[test]
fn the_job_collects_only_the_rows_whose_ttl_has_elapsed() {
    let service = service();
    let tenant = tenant(0x3a);
    let (early, _) = custom_plugin(&service, tenant, "early");
    let (late, _) = custom_plugin(&service, tenant, "late");
    let (linked, reference) = custom_plugin(&service, tenant, "kept");
    bind_custom_plugin(&service, tenant, &reference, linked);

    // One row marked at the epoch, one marked when the job runs.
    store_of(&service).mark_plugin_eligible(tenant, early, 0);
    store_of(&service).mark_plugin_eligible(tenant, late, PLUGIN_GC_TTL_SECS);
    assert_ne!(
        eligibility_of(&service, tenant, early),
        eligibility_of(&service, tenant, late),
        "the two marked rows carry different instants"
    );

    let report = service
        .run_plugin_garbage_collection_at(PLUGIN_GC_TTL_SECS + 10)
        .expect("the job runs");
    assert_eq!(report.collected, vec![early], "only the expired row is collected");
    assert!(eligibility_of(&service, tenant, early).is_none(), "the row is gone");
    assert!(
        eligibility_of(&service, tenant, late).is_some(),
        "the row inside its TTL is left alone"
    );
    assert_eq!(
        eligibility_of(&service, tenant, linked),
        Some(None),
        "the linked row is left alone"
    );
}

#[test]
fn the_job_never_collects_a_row_that_gained_a_reference_before_it_ran() {
    let service = service();
    let tenant = tenant(0x3b);
    let (id, reference) = custom_plugin(&service, tenant, "reclaimed");
    store_of(&service).mark_plugin_eligible(tenant, id, 0);
    bind_custom_plugin(&service, tenant, &reference, id);

    let report = service
        .run_plugin_garbage_collection_at(PLUGIN_GC_TTL_SECS + 10)
        .expect("the job runs");
    assert!(
        report.collected.is_empty(),
        "a rebound row is never collected: {report:?}"
    );
    assert!(eligibility_of(&service, tenant, id).is_some(), "the row is left in place");
}
