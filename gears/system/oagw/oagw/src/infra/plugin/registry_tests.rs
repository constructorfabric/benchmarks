//! Registry construction and resolution
//! (`cpt-cf-oagw-dod-plugin-system-registries`).
//!
//! Every registry is constructed through `with_builtins()`, resolves the
//! built-in identifiers of its type by their GTS identifier, returns an
//! unresolvable outcome for any identifier it does not carry, and never
//! resolves a catalog-only identifier — the catalog-only set is rejected by
//! the binding-time check, not by a registry.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-registries:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::super::resolution::PluginRegistries;
use crate::domain::gts_helpers::{
    APIKEY_AUTH_PLUGIN_ID, BUILTIN_PLUGIN_IDS, CATALOG_ONLY_PLUGIN_IDS, NOOP_AUTH_PLUGIN_ID,
    OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID, OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID,
    REQUEST_ID_TRANSFORM_PLUGIN_ID, REQUIRED_HEADERS_GUARD_PLUGIN_ID,
};

fn registries() -> PluginRegistries {
    let (_upstreams, _routes, plugins) = crate::infra::storage::Storage::new().repositories();
    PluginRegistries::with_builtins(
        std::sync::Arc::new(crate::test_support::FakeCredStore),
        crate::config::TokenCacheConfig::default(),
        plugins,
    )
}

/// The three registries are constructible through `with_builtins()` and every
/// built-in identifier of the right type resolves.
#[test]
fn the_registries_resolve_every_builtin_identifier() {
    let registries = registries();
    // The catalog carries every built-in identifier, which is what the
    // resolution and the binding-time check consult.
    assert_eq!(BUILTIN_PLUGIN_IDS.len(), 6);
    assert!(registries.auth.get(NOOP_AUTH_PLUGIN_ID).is_resolved());
    assert!(registries.auth.get(APIKEY_AUTH_PLUGIN_ID).is_resolved());
    assert!(registries.auth.get(OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID).is_resolved());
    assert!(registries.auth.get(OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID).is_resolved());
    assert!(registries.guard.get(REQUIRED_HEADERS_GUARD_PLUGIN_ID).is_resolved());
    assert!(registries.transform.get(REQUEST_ID_TRANSFORM_PLUGIN_ID).is_resolved());
}

/// The built-in plugin types are the GTS identifiers the catalog carries, and
/// the `id()` keys are the leaf names the FEATURE names.
#[test]
fn the_builtin_registry_keys_are_the_leaf_names() {
    let registries = registries();
    assert_eq!(registries.auth.get(NOOP_AUTH_PLUGIN_ID).ok().expect("noop").id(), "noop");
    assert_eq!(registries.auth.get(APIKEY_AUTH_PLUGIN_ID).ok().expect("apikey").id(), "apikey");
    assert_eq!(
        registries.guard.get(REQUIRED_HEADERS_GUARD_PLUGIN_ID).ok().expect("guard").id(),
        "required_headers"
    );
    assert_eq!(
        registries.transform.get(REQUEST_ID_TRANSFORM_PLUGIN_ID).ok().expect("transform").id(),
        "request_id"
    );
}

/// No catalog-only identifier resolves through any registry.
#[test]
fn no_catalog_only_identifier_resolves() {
    let registries = registries();
    for id in CATALOG_ONLY_PLUGIN_IDS {
        assert!(!registries.auth.get(id).is_resolved(), "{id} resolves as auth");
        assert!(!registries.guard.get(id).is_resolved(), "{id} resolves as guard");
        assert!(!registries.transform.get(id).is_resolved(), "{id} resolves as transform");
    }
}

/// An identifier no registry carries is unresolvable, in every registry.
#[test]
fn an_unknown_identifier_is_unresolvable() {
    let registries = registries();
    let unknown = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.nonesuch.v1";
    assert!(!registries.auth.get(unknown).is_resolved());
    assert!(!registries.guard.get(unknown).is_resolved());
    assert!(!registries.transform.get(unknown).is_resolved());
}

/// A registry is typed: an identifier of one plugin base type never resolves
/// through the registry of another.
#[test]
fn a_registry_never_resolves_another_type() {
    let registries = registries();
    assert!(!registries.guard.get(NOOP_AUTH_PLUGIN_ID).is_resolved());
    assert!(!registries.auth.get(REQUIRED_HEADERS_GUARD_PLUGIN_ID).is_resolved());
    assert!(!registries.guard.get(REQUEST_ID_TRANSFORM_PLUGIN_ID).is_resolved());
}
