//! Plugin resolution and built-in registration (T053, DESIGN §3.1).
//!
//! A plugin reference is classified before it is resolved: a UUID instance
//! part addresses a persisted, tenant-owned plugin the plugin store owns, and
//! any other instance resolves through the in-process registry. The built-in
//! plugins this gear ships are registered under their GTS identifiers; the
//! catalog-only identifiers are not.

use super::registry::{PluginRegistry, PluginResolution};
use crate::domain::gts_helpers::{
    BUILTIN_AUTH_APIKEY, BUILTIN_AUTH_NOOP, BUILTIN_AUTH_OAUTH2_CC, BUILTIN_AUTH_OAUTH2_CC_BASIC,
    BUILTIN_GUARD_REQUIRED_HEADERS, BUILTIN_TRANSFORM_REQUEST_ID, CATALOG_AUTH_BASIC,
    CATALOG_AUTH_BEARER, CATALOG_GUARD_CORS, CATALOG_GUARD_TIMEOUT, CATALOG_TRANSFORM_LOGGING,
    CATALOG_TRANSFORM_METRICS,
};

/// A persisted, tenant-owned plugin reference: `{type}~{uuid}`.
const PERSISTED: &str = "gts.cf.core.oagw.transform_plugin.v1~7c9e6679-7425-40de-944b-e07fc1f90ae7";
/// A named reference the registry has never heard of.
const UNKNOWN: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.something_else.v1";

#[test]
fn a_uuid_instance_part_addresses_the_persisted_store() {
    let registry = PluginRegistry::builtin();
    assert_eq!(
        registry.classify(PERSISTED),
        PluginResolution::Persisted {
            uuid: Some(
                uuid::Uuid::parse_str("7c9e6679-7425-40de-944b-e07fc1f90ae7").expect("a uuid")
            )
        },
        "the uuid instance part is extracted, not looked up in the registry"
    );
    assert_eq!(
        registry.classify("not a gts identifier at all"),
        PluginResolution::Named { resolvable: false },
        "a non-gts reference still resolves through the registry"
    );
}

#[test]
fn a_named_instance_resolves_through_the_registry() {
    let registry = PluginRegistry::builtin();
    assert_eq!(
        registry.classify(BUILTIN_AUTH_APIKEY),
        PluginResolution::Named { resolvable: true },
        "a built-in is a named reference"
    );
    assert_eq!(
        registry.classify(UNKNOWN),
        PluginResolution::Named { resolvable: false },
        "an unknown named reference is not resolvable"
    );
}

#[test]
fn every_builtin_auth_plugin_is_resolvable() {
    let registry = PluginRegistry::builtin();
    for id in [
        BUILTIN_AUTH_NOOP,
        BUILTIN_AUTH_APIKEY,
        BUILTIN_AUTH_OAUTH2_CC,
        BUILTIN_AUTH_OAUTH2_CC_BASIC,
    ] {
        assert!(
            registry.auth(id).is_some(),
            "the auth built-in `{id}` is registered"
        );
    }
}

#[test]
fn required_headers_is_the_only_builtin_guard() {
    let registry = PluginRegistry::builtin();
    assert_eq!(
        registry.guard_ids(),
        vec![BUILTIN_GUARD_REQUIRED_HEADERS.to_string()],
        "one guard built-in ships with the gear"
    );
}

#[test]
fn request_id_is_the_only_builtin_transform() {
    let registry = PluginRegistry::builtin();
    assert_eq!(
        registry.transform_ids(),
        vec![BUILTIN_TRANSFORM_REQUEST_ID.to_string()],
        "one transform built-in ships with the gear"
    );
}

#[test]
fn the_catalog_only_identifiers_are_not_registry_resolvable() {
    let registry = PluginRegistry::builtin();
    for id in [
        CATALOG_AUTH_BASIC,
        CATALOG_AUTH_BEARER,
        CATALOG_GUARD_TIMEOUT,
        CATALOG_GUARD_CORS,
        CATALOG_TRANSFORM_LOGGING,
        CATALOG_TRANSFORM_METRICS,
    ] {
        assert!(
            registry.auth(id).is_none()
                && registry.guard(id).is_none()
                && registry.transform(id).is_none(),
            "`{id}` exists for types-registry cataloging only"
        );
    }
}

#[test]
fn a_registry_lookup_that_misses_is_a_plugin_not_found() {
    use crate::domain::error::DomainError;

    // The chain resolves a bound plugin the registry does not carry into the
    // 503 row the error contract documents.
    let error = DomainError::PluginNotFound {
        plugin_id: UNKNOWN.to_string(),
    };
    assert_eq!(
        error.error_type_suffix(),
        "plugin.not_found.v1"
    );
    assert_eq!(crate::api::rest::error::http_status_of(&error), 503);
}
