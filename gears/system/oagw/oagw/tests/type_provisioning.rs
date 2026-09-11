//! Integration tests for the GTS base-type provisioning
//! (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`,
//! `cpt-cf-oagw-flow-gear-foundation-type-provisioning`).
//!
//! The gear registers its base types through the `types_registry` handle at
//! initialization (`inst-gf-gts-1`/`-2`): a repeat registration is a successful
//! no-op (`inst-gf-gts-3`), and any other rejection fails initialization
//! (`inst-gf-gts-4`/`-5`).
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-type-provisioning:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use toolkit::Gear;

use oagw::test_support::{
    FakeTypesRegistry, failing_registry, rejecting_registry, test_context_with_registry,
    unimplemented,
};
use oagw::OagwGear;

/// The six base types the gear provisions
/// (`cpt-cf-oagw-dod-gear-foundation-type-provisioning`).
const BASE_TYPES: [&str; 6] = [
    "gts.cf.core.oagw.upstream.v1~",
    "gts.cf.core.oagw.route.v1~",
    "gts.cf.core.oagw.auth_plugin.v1~",
    "gts.cf.core.oagw.guard_plugin.v1~",
    "gts.cf.core.oagw.transform_plugin.v1~",
    "gts.cf.core.oagw.proxy.v1~",
];

/// Initialization registers every base type through the `types-registry`
/// handle, and the `$id` of each submitted schema document is the type
/// identifier.
#[tokio::test]
async fn init_registers_every_base_type() {
    let registry = Arc::new(FakeTypesRegistry::new());
    let gear = OagwGear::default();

    gear.init(&test_context_with_registry(None, Arc::clone(&registry)))
        .await
        .expect("init succeeds");

    let mut registered = registry.registered_ids();
    registered.sort_unstable();
    // The submitted `$id` carries the `gts://` URI form the GTS model keys a
    // Type Schema by; the registry derives the canonical identifier from it.
    let mut expected: Vec<String> = BASE_TYPES
        .iter()
        .map(|id| format!("gts://{id}"))
        .collect();
    expected.sort_unstable();
    assert_eq!(registered, expected, "every base type is provisioned");

    for id in BASE_TYPES {
        let schema = oagw::infra::type_provisioning::base_type_schema(id);
        // The `$id` is the type identifier in the `gts://` URI form the GTS
        // spec requires of a Type Schema; the bare canonical form is not a
        // resolvable schema id.
        assert_eq!(schema["$id"].as_str(), Some(format!("gts://{id}").as_str()));
        assert_eq!(schema["type"].as_str(), Some("object"));
    }
}

/// A registration over an already-populated registry is a no-op, so a restart
/// of the gear — or a second instance in the same process — succeeds without
/// disturbing the registry's contents.
#[tokio::test]
async fn a_repeated_registration_is_a_no_op() {
    let warm = Arc::new(FakeTypesRegistry::new());
    let first = oagw::infra::type_provisioning::register_base_types(warm.as_ref())
        .await
        .expect("the first registration succeeds");
    assert_eq!(first.len(), BASE_TYPES.len());

    let second = oagw::infra::type_provisioning::register_base_types(warm.as_ref())
        .await
        .expect("the repeat registration is a no-op, not an error");
    assert_eq!(second, first, "the identifiers stand unchanged");
    assert_eq!(warm.registered_ids().len(), BASE_TYPES.len(), "nothing was duplicated");

    // A fresh gear instance over the warm registry initializes as well.
    let gear = OagwGear::default();
    gear.init(&test_context_with_registry(None, Arc::clone(&warm)))
        .await
        .expect("an already-populated registry is not an error");
    assert_eq!(warm.registered_ids().len(), BASE_TYPES.len());
}

/// An unreachable registry aborts initialization with the provisioning error,
/// and nothing is published.
#[tokio::test]
async fn an_unreachable_registry_fails_initialization() {
    let gear = OagwGear::default();
    let ctx = test_context_with_registry(None, failing_registry(unimplemented()));
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("type provisioning"), "{error}");
    assert!(gear.config().is_none(), "no configuration is published");
    assert!(gear.storage().is_none(), "no storage is published");
    assert!(gear.service().is_none(), "no service handle is published");
}

/// A registry that holds the connection but rejects a base type for any reason
/// other than `AlreadyExists` aborts initialization too.
#[tokio::test]
async fn a_rejected_base_type_fails_initialization() {
    let gear = OagwGear::default();
    let ctx = test_context_with_registry(None, rejecting_registry(unimplemented()));
    let error = gear.init(&ctx).await.expect_err("init fails");
    assert!(error.to_string().contains("type provisioning"), "{error}");
    assert!(gear.storage().is_none(), "nothing is published on a failed registration");
    assert!(gear.service().is_none());
}
