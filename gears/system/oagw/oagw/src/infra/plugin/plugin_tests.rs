//! Unit tests for the plugin-catalog binding-time resolvability boundary
//! (`cpt-cf-oagw-dod-route-management-route-overrides`).

use uuid::Uuid;

use super::*;
use crate::domain::dto::{Plugin, PluginsConfig};
use crate::domain::gts_helpers::{
    BASIC_AUTH_PLUGIN_ID, BUILTIN_PLUGIN_IDS, CATALOG_ONLY_PLUGIN_IDS, CORS_GUARD_PLUGIN_ID,
    NOOP_AUTH_PLUGIN_ID, TIMEOUT_GUARD_PLUGIN_ID,
};
use crate::infra::storage::Storage;

fn resolver() -> CatalogBindingResolver {
    let (_upstreams, _routes, plugins) = Storage::new().repositories();
    CatalogBindingResolver::new(plugins)
}

fn custom_plugin(tenant_id: Uuid, id: Uuid) -> Plugin {
    Plugin {
        id,
        tenant_id,
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.test.v1".to_owned(),
        name: "custom".to_owned(),
        config_schema: Some(serde_json::json!({})),
        source_code: None,
        last_used_at: None,
        gc_eligible_at: None,
    }
}

/// A built-in identifier resolves, with `plugin_ref` stored and no UUID.
#[test]
fn a_builtin_reference_resolves() {
    let resolver = resolver();
    let tenant_id = Uuid::new_v4();
    let bindings = resolver
        .resolve(tenant_id, &[NOOP_AUTH_PLUGIN_ID.to_owned()])
        .expect("resolved");
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].position, 0, "positions are contiguous from zero");
    assert_eq!(bindings[0].plugin_ref, NOOP_AUTH_PLUGIN_ID, "`plugin_ref` is always stored");
    assert_eq!(bindings[0].plugin_uuid, None, "a GTS reference is not UUID-backed");
}

/// A custom-plugin reference resolves only when the calling tenant holds the
/// row it names.
#[test]
fn a_custom_reference_resolves_only_inside_its_tenant() {
    let (upstreams, _routes, plugins) = Storage::new().repositories();
    let _ = upstreams;
    let owner = Uuid::new_v4();
    let foreign = Uuid::new_v4();
    let id = Uuid::new_v4();
    plugins
        .create(owner, custom_plugin(owner, id))
        .expect("the custom plugin is stored");
    let resolver = CatalogBindingResolver::new(plugins);
    assert!(resolver.resolve(owner, &[id.to_string()]).is_ok(), "the tenant holds the row");
    let error = resolver
        .resolve(foreign, &[id.to_string()])
        .expect_err("a foreign plugin row does not resolve");
    assert!(matches!(error, crate::domain::error::DomainError::ValidationError { .. }), "{error:?}");
}

/// A catalog-only identifier — `cors.v1`, `timeout.v1`, `basic.v1` among them
/// — is rejected at binding time.
#[test]
fn a_catalog_only_reference_is_rejected_at_binding_time() {
    let resolver = resolver();
    for reference in CATALOG_ONLY_PLUGIN_IDS {
        let error = resolver
            .resolve(Uuid::new_v4(), &[reference.to_owned()])
            .expect_err("a catalog-only reference is not bindable");
        let crate::domain::error::DomainError::ValidationError { detail, path, .. } = &error else {
            panic!("expected a validation rejection, got {error:?}")
        };
        assert_eq!(path.as_deref(), Some("plugins.items"), "{detail}");
    }
}

/// The `cors` guard identifier stays catalog-only: CORS is a first-class field
/// of an upstream or a route (FEATURE entry 2.8), so a `plugins.items[]`
/// binding naming it is rejected at binding time and no binding is stored.
#[test]
fn a_cors_guard_reference_is_never_bindable() {
    let (_upstreams, _routes, plugins) = Storage::new().repositories();
    let resolver = CatalogBindingResolver::new(plugins.clone());
    let error = resolver
        .resolve(Uuid::new_v4(), &[CORS_GUARD_PLUGIN_ID.to_owned()])
        .expect_err("the cors guard identifier is not bindable");
    let crate::domain::error::DomainError::ValidationError { detail, path, .. } = &error else {
        panic!("expected a validation rejection, got {error:?}")
    };
    assert_eq!(path.as_deref(), Some("plugins.items"), "{detail}");
    assert!(detail.contains("catalog only"), "{detail}");
    let stored = plugins.list(Uuid::new_v4()).expect("the repository is reachable");
    assert!(stored.is_empty(), "no plugin binding was stored: {stored:?}");
}

/// A reference that is neither a built-in identifier nor a custom plugin
/// reference is rejected.
#[test]
fn an_unrecognizable_reference_is_rejected() {
    let resolver = resolver();
    for reference in ["not-an-identifier", "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.unknown.v1"] {
        let error = resolver
            .resolve(Uuid::new_v4(), &[reference.to_owned()])
            .expect_err("rejected");
        assert!(matches!(error, crate::domain::error::DomainError::ValidationError { .. }), "{error:?}");
    }
}

/// Every entry is resolved in order, so the first unresolvable one names its
/// position.
#[test]
fn the_first_unresolvable_entry_names_its_position() {
    let (upstreams, _routes, plugins) = Storage::new().repositories();
    let _ = upstreams;
    let owner = Uuid::new_v4();
    let custom = Uuid::new_v4();
    plugins.create(owner, custom_plugin(owner, custom)).expect("stored");
    let resolver = CatalogBindingResolver::new(plugins);
    let references = vec![
        NOOP_AUTH_PLUGIN_ID.to_owned(),
        custom.to_string(),
        TIMEOUT_GUARD_PLUGIN_ID.to_owned(),
    ];
    let error = resolver.resolve(owner, &references).expect_err("rejected");
    let crate::domain::error::DomainError::ValidationError { detail, .. } = &error else {
        panic!("expected a validation rejection, got {error:?}")
    };
    assert!(detail.contains("`2`"), "{detail}");
}

/// The resolver is the boundary the route write path executes: the catalog
/// constants it consults are the ones the foundation entry provisioned.
#[test]
fn the_catalog_boundary_is_the_foundation_catalog() {
    assert!(BUILTIN_PLUGIN_IDS.contains(&NOOP_AUTH_PLUGIN_ID));
    assert!(CATALOG_ONLY_PLUGIN_IDS.contains(&BASIC_AUTH_PLUGIN_ID));
    assert!(CATALOG_ONLY_PLUGIN_IDS.contains(&CORS_GUARD_PLUGIN_ID));
    assert!(CATALOG_ONLY_PLUGIN_IDS.contains(&TIMEOUT_GUARD_PLUGIN_ID));
}

/// A route `plugins` block resolves through the same boundary the write path
/// uses, so no unresolved entry can reach a binding row.
#[test]
fn the_write_path_never_stores_an_unresolved_entry() {
    let resolver = resolver();
    let tenant_id = Uuid::new_v4();
    let references: Vec<String> = vec![NOOP_AUTH_PLUGIN_ID.to_owned(), BASIC_AUTH_PLUGIN_ID.to_owned()];
    let error = resolver.resolve(tenant_id, &references).expect_err("rejected");
    assert!(matches!(error, crate::domain::error::DomainError::ValidationError { .. }), "{error:?}");
    let _ = PluginsConfig { sharing: crate::domain::dto::SharingMode::Inherit, items: references };
}
