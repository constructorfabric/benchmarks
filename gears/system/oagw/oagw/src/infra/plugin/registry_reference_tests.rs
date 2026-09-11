//! Registry-reference-only posture of a custom plugin
//! (`cpt-cf-oagw-dod-plugin-system-registry-reference-only`).
//!
//! A plugin registered through the management API is addressable, resolvable
//! and bindable, and its stored source content is an opaque reference artifact
//! no code path interprets or executes (graded deviation 6).
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-registry-reference-only:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{GUARD_PLUGIN_BASE_TYPE, TRANSFORM_PLUGIN_BASE_TYPE};
use crate::domain::plugin::Principal;
use crate::domain::plugin::composition::ComposedChain;
use crate::domain::plugin::identifier::parse_instance;
use crate::infra::plugin::resolution::{PluginRegistries, TenantChain};
use crate::infra::plugin::executor::PluginRuntime;
use crate::infra::storage::Storage;
use crate::test_support::FakeCredStore;

const SOURCE: &str = "const reference = 'opaque'; never executed";

struct Fixture {
    registries: PluginRegistries,
    record: crate::domain::dto::Plugin,
    storage: Arc<Storage>,
}

fn fixture(tenant: Uuid, name: &str, source: Option<&str>) -> Fixture {
    let storage = Arc::new(Storage::new());
    let (_upstreams, _routes, plugins) = Arc::clone(&storage).repositories();
    let registries = PluginRegistries::with_builtins(
        Arc::new(FakeCredStore),
        crate::config::TokenCacheConfig::default(),
        Arc::clone(&plugins),
    );
    let mut record = crate::domain::dto::Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        plugin_type: GUARD_PLUGIN_BASE_TYPE.to_owned(),
        name: name.to_owned(),
        config_schema: Some(serde_json::json!({ "type": "object" })),
        source_code: source.map(str::to_owned),
        last_used_at: None,
        gc_eligible_at: None,
    };
    plugins.create(tenant, record.clone()).expect("stored");
    record.tenant_id = tenant;
    Fixture { registries, record, storage }
}

/// A registered custom plugin resolves by its UUID instance form, which is
/// what makes it bindable and addressable.
#[test]
fn a_registered_custom_plugin_resolves_as_a_reference() {
    let tenant = Uuid::new_v4();
    let Fixture { registries, record, .. } = fixture(tenant, "reference-guard", Some(SOURCE));
    let reference = format!("{GUARD_PLUGIN_BASE_TYPE}{}", record.id);
    let resolved = crate::infra::plugin::resolution::resolve_reference(
        &registries,
        &TenantChain::new(vec![tenant]),
        &reference,
    )
    .expect("the record resolves");
    assert_eq!(parse_instance(&reference), parse_instance(&format!("{GUARD_PLUGIN_BASE_TYPE}{}", record.id)));
    assert_eq!(resolved.plugin_type, GUARD_PLUGIN_BASE_TYPE);
    assert!(resolved.config_schema.is_some(), "the registered schema is carried");
    assert!(resolved.binding.is_none(), "a custom plugin carries no executable binding");
}

/// A registered custom plugin binds: the binding-time resolver accepts the
/// reference and stores a row whose `plugin_uuid` agrees with it.
#[test]
fn a_registered_custom_plugin_is_bindable() {
    let tenant = Uuid::new_v4();
    let Fixture { record, storage, .. } = fixture(tenant, "bindable-guard", Some(SOURCE));
    let reference = format!("{GUARD_PLUGIN_BASE_TYPE}{}", record.id);
    let (_upstreams, _routes, plugins) = Arc::clone(&storage).repositories();
    let resolver = crate::infra::plugin::CatalogBindingResolver::new(Arc::clone(&plugins));
    let bindings =
        crate::domain::services::route_management::PluginBindingResolver::resolve(
            &resolver,
            tenant,
            &[reference.clone()],
        )
        .expect("bound");
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].plugin_ref, reference);
    assert_eq!(bindings[0].plugin_uuid, Some(record.id));
    assert_eq!(bindings[0].position, 0);
}

/// A registered custom plugin is readable through the source endpoint, and its
/// stored content is an opaque reference artifact.
#[test]
fn the_registered_source_is_an_opaque_reference_artifact() {
    let tenant = Uuid::new_v4();
    let Fixture { record, .. } = fixture(tenant, "readable-guard", Some(SOURCE));
    let response = crate::api::rest::dto::PluginSourceResponse::from_record(&record);
    assert_eq!(response.id, record.id);
    assert_eq!(response.plugin_type, GUARD_PLUGIN_BASE_TYPE);
    assert_eq!(response.name, "readable-guard");
    assert_eq!(response.source_code, SOURCE);
}

/// No execution path exists for the source content: a chain that binds the
/// registered plugin runs no phase for it, and its source is never handed to
/// any plugin trait.
#[tokio::test]
async fn the_registered_source_never_executes() {
    let tenant = Uuid::new_v4();
    let Fixture { registries, record, .. } = fixture(tenant, "silent-guard", Some(SOURCE));
    let reference = format!("{GUARD_PLUGIN_BASE_TYPE}{}", record.id);
    let runtime = PluginRuntime::new(Arc::new(registries), 2);
    let chain = ComposedChain {
        bindings: vec![crate::domain::repo::PluginBinding {
            position: 0,
            plugin_ref: reference,
            plugin_uuid: Some(record.id),
        }],
        auth_ref: None,
    };
    let mut resolved = runtime
        .resolve(&chain, &TenantChain::new(vec![tenant]), None)
        .expect("the registered plugin resolves");
    assert!(resolved.is_empty(), "the chain runs no phase for it: {:?}", resolved);
    assert!(resolved.phases.is_empty());
    let mut headers = Vec::new();
    let transformed = runtime
        .run_request(&mut resolved, "GET", "/p", &[], &mut headers, bytes::Bytes::new(), Principal::default(), "trace")
        .await
        .expect("the chain proceeds without it");
    assert!(transformed.headers.is_empty());
    assert!(resolved.phases.is_empty(), "no trace entry is recorded");
}

/// The source content of a registered plugin is carried verbatim through the
/// record: nothing in the resolution path inspects, compiles or rewrites it.
#[test]
fn the_source_content_is_carried_verbatim() {
    let tenant = Uuid::new_v4();
    let Fixture { registries, record, .. } = fixture(tenant, "verbatim-guard", Some(SOURCE));
    let reference = format!("{GUARD_PLUGIN_BASE_TYPE}{}", record.id);
    let resolved = crate::infra::plugin::resolution::resolve_reference(
        &registries,
        &TenantChain::new(vec![tenant]),
        &reference,
    )
    .expect("resolves");
    // The resolution surface names the plugin type and the schema, and carries
    // no rendering of the source at all.
    let rendered = format!("{resolved:?}");
    assert!(!rendered.contains("opaque"), "the debug surface carries no source: {rendered}");
    assert_eq!(resolved.plugin_type, GUARD_PLUGIN_BASE_TYPE);
}

/// A registered plugin of another base type is typed by its own record, not by
/// the base type the reference spells.
#[test]
fn the_registered_type_comes_from_the_record() {
    let tenant = Uuid::new_v4();
    let Fixture { registries, record, .. } = fixture(tenant, "typed", Some(SOURCE));
    let reference = format!("{TRANSFORM_PLUGIN_BASE_TYPE}{}", record.id);
    let resolved = crate::infra::plugin::resolution::resolve_reference(
        &registries,
        &TenantChain::new(vec![tenant]),
        &reference,
    )
    .expect_err("the types disagree");
    assert!(matches!(resolved, DomainError::ValidationError { .. }), "{resolved}");
}
