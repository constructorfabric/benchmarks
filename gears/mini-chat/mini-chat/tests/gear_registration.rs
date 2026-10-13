//! Gear registration (DESIGN 3.2 "mini-chat gear" / "Gear lifecycle"): the
//! `mini-chat` gear is discoverable through the toolkit registry with the
//! documented capabilities and dependencies, and the two bundled plugins are
//! registered as gears too, so the example server links them via
//! `registered_gears.rs`.
//!
//! The test binary links every gear `mini-chat` or `oagw` declares as a
//! dependency (`use ... as _`), because `GearRegistry::discover_and_build`
//! validates the dependency graph.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use authn_resolver as _;
use authz_resolver as _;
use credstore as _;
use mini_chat as _;
use oagw as _;
use tenant_resolver as _;
use toolkit::registry::GearRegistry;
use types_registry as _;

fn entry(registry: &GearRegistry, name: &str) -> (Vec<&'static str>, Vec<&'static str>) {
    let e = registry
        .gears()
        .iter()
        .find(|e| e.name() == name)
        .unwrap_or_else(|| {
            let known: Vec<&str> = registry
                .gears()
                .iter()
                .map(toolkit::registry::GearEntry::name)
                .collect();
            panic!("gear `{name}` is not registered; known gears: {known:?}")
        });
    (e.deps().to_vec(), e.caps().labels())
}

#[test]
fn mini_chat_gear_is_registered_with_db_rest_and_stateful_capabilities() {
    let registry = GearRegistry::discover_and_build().expect("gear registry builds");
    let (_deps, caps) = entry(&registry, "mini-chat");
    for cap in ["db", "rest", "stateful"] {
        assert!(
            caps.contains(&cap),
            "mini-chat must declare `{cap}`; has {caps:?}"
        );
    }
}

#[test]
fn mini_chat_gear_depends_on_registry_resolvers_and_oagw() {
    let registry = GearRegistry::discover_and_build().expect("gear registry builds");
    let (deps, _caps) = entry(&registry, "mini-chat");
    for dep in ["types-registry", "authn-resolver", "authz-resolver", "oagw"] {
        assert!(
            deps.contains(&dep),
            "mini-chat must depend on `{dep}`; deps: {deps:?}"
        );
    }
}

#[test]
fn mini_chat_initialises_after_its_dependencies() {
    let registry = GearRegistry::discover_and_build().expect("gear registry builds");
    let order: Vec<&str> = registry
        .gears()
        .iter()
        .map(toolkit::registry::GearEntry::name)
        .collect();
    let pos = |n: &str| {
        order
            .iter()
            .position(|g| *g == n)
            .unwrap_or_else(|| panic!("{n} missing"))
    };
    for dep in ["types-registry", "authn-resolver", "authz-resolver", "oagw"] {
        assert!(
            pos(dep) < pos("mini-chat"),
            "{dep} must be initialised before mini-chat: {order:?}"
        );
    }
}

#[test]
fn bundled_plugins_are_registered_as_gears_after_types_registry() {
    let registry = GearRegistry::discover_and_build().expect("gear registry builds");
    for name in [
        "static-mini-chat-model-policy-plugin",
        "static-mini-chat-audit-plugin",
    ] {
        let (deps, _caps) = entry(&registry, name);
        assert!(
            deps.contains(&"types-registry"),
            "{name} registers its GTS instance in types-registry; deps: {deps:?}"
        );
    }
}
