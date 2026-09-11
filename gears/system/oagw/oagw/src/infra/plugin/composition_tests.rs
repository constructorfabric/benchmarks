//! Chain composition order
//! (`cpt-cf-oagw-algo-plugin-system-chain-order`): upstream-then-route, the
//! `private`/`enforce` sharing rules and the contiguous-position invariant.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-chain-ordering:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::domain::dto::{AuthConfig, SharingMode};
use crate::domain::error::DomainError;
use crate::domain::plugin::composition::{compose, ChainLayer, ComposedChain};
use crate::domain::repo::PluginBinding;

const GUARD_BASE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
const TRANSFORM_BASE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

fn binding(position: u32, leaf: &str, base: &str) -> PluginBinding {
    PluginBinding {
        position,
        plugin_ref: format!("{base}{leaf}"),
        plugin_uuid: None,
    }
}

/// `inst-ps-ord-2`: upstream-level bindings come before route-level ones, in
/// binding order within each level.
#[test]
fn the_levels_concatenate_upstream_then_route() {
    let composed = compose(
        &[],
        &[
            binding(0, "u1", GUARD_BASE),
            binding(1, "u2", TRANSFORM_BASE),
        ],
        &[
            binding(0, "r1", GUARD_BASE),
            binding(1, "r2", TRANSFORM_BASE),
        ],
        None,
    )
    .expect("composed");
    assert_eq!(
        composed
            .bindings
            .iter()
            .map(|binding| binding.plugin_ref.rsplit('~').next().expect("leaf"))
            .collect::<Vec<_>>(),
        ["u1", "u2", "r1", "r2"]
    );
    assert!(composed.auth_ref.is_none());
}

/// Ancestors precede the caller's own levels, in the order the caller handed
/// them over (base first).
#[test]
fn ancestor_levels_precede_the_callers_own() {
    let composed = compose(
        &[
            ChainLayer::inherited(vec![binding(0, "a1", GUARD_BASE)], SharingMode::Enforce),
            ChainLayer::inherited(vec![binding(0, "a2", GUARD_BASE)], SharingMode::Enforce),
        ],
        &[binding(0, "u1", TRANSFORM_BASE)],
        &[],
        None,
    )
    .expect("composed");
    assert_eq!(
        composed
            .bindings
            .iter()
            .map(|binding| binding.plugin_ref.rsplit('~').next().expect("leaf"))
            .collect::<Vec<_>>(),
        ["a1", "a2", "u1"]
    );
}

/// `inst-ps-ord-1`: an ancestor binding under `sharing: private` contributes
/// nothing.
#[test]
fn a_private_ancestor_binding_is_invisible() {
    let composed = compose(
        &[ChainLayer::inherited(
            vec![binding(0, "private-guard", GUARD_BASE)],
            SharingMode::Private,
        )],
        &[binding(0, "u1", GUARD_BASE)],
        &[],
        None,
    )
    .expect("composed");
    assert_eq!(composed.bindings.len(), 1);
    assert!(composed.bindings[0].plugin_ref.ends_with("u1"));
}

/// `inst-ps-ord-3`: an ancestor binding under `sharing: enforce` is retained,
/// so no descendant composition can remove it.
#[test]
fn an_enforced_ancestor_binding_is_retained() {
    let composed = compose(
        &[ChainLayer::inherited(
            vec![binding(0, "enforced-guard", GUARD_BASE)],
            SharingMode::Enforce,
        )],
        &[],
        &[],
        None,
    )
    .expect("composed");
    assert_eq!(composed.bindings.len(), 1);
    assert!(composed.bindings[0].plugin_ref.ends_with("enforced-guard"));
}

/// The caller's own level honours its own `private` choice: `owned()` is never
/// inherited, so the mode is not even consulted.
#[test]
fn the_callers_own_private_choice_is_honoured() {
    let composed = compose(
        &[ChainLayer::owned(vec![binding(0, "own-guard", GUARD_BASE)])],
        &[],
        &[],
        None,
    )
    .expect("composed");
    assert_eq!(composed.bindings.len(), 1);
}

/// `inst-ps-ord-4`: the composed positions are contiguous from zero, so a
/// chain entry is addressable by position.
#[test]
fn the_composed_positions_are_contiguous_from_zero() {
    let composed = compose(
        &[],
        &[binding(0, "u1", GUARD_BASE), binding(1, "u2", TRANSFORM_BASE)],
        &[binding(0, "r1", GUARD_BASE)],
        None,
    )
    .expect("composed");
    assert_eq!(
        composed.bindings.iter().map(|binding| binding.position).collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

/// A level whose positions are not contiguous from zero is rejected, which is
/// the binding invariant the storage step established.
#[test]
fn a_non_contiguous_level_is_rejected() {
    for (upstream, route) in [
        (vec![binding(1, "u1", GUARD_BASE)], Vec::new()),
        (Vec::new(), vec![binding(2, "r1", GUARD_BASE)]),
        (vec![binding(0, "u1", GUARD_BASE)], vec![binding(3, "r1", GUARD_BASE)]),
    ] {
        let error = compose(&[], &upstream, &route, None).expect_err("rejected");
        assert!(
            matches!(&error, DomainError::ValidationError { path, .. } if path.as_deref() == Some("plugins.items")),
            "{error}"
        );
    }
}

/// `inst-ps-ord-5`: the single auth plugin comes from the upstream `auth`
/// block, never from the chain.
#[test]
fn the_auth_plugin_comes_from_the_auth_block() {
    let composed = compose(
        &[],
        &[],
        &[],
        Some(&AuthConfig {
            auth_type: Some(crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned()),
            sharing: SharingMode::Private,
            config: None,
        }),
    )
    .expect("composed");
    assert_eq!(composed.auth_ref.as_deref(), Some(crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID));
    assert!(!composed.is_empty());

    let no_auth = compose(&[], &[], &[], None).expect("composed");
    assert!(no_auth.is_empty());
    assert!(ComposedChain::default().is_empty());
}

/// A chain with no level and no auth block composes to an empty chain, which
/// is the no-plugin posture a request with no bindings runs.
#[test]
fn an_empty_composition_is_empty() {
    let composed = compose(&[], &[], &[], None).expect("composed");
    assert!(composed.is_empty());
    assert!(composed.bindings.is_empty());
}
