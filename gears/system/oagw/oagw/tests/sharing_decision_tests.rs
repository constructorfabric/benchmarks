//! The sharing-mode and permission decision.
//!
//! Covers `cpt-cf-oagw-dod-sharing-mode-decision` and
//! `cpt-cf-oagw-dod-descendant-override-permissions`: every row of the
//! decision table for every family, the priority the refusal order fixes, the
//! deny-by-default posture of the four descendant override permissions, and
//! the absence of any fifth permission for the CORS family.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use oagw::control_plane::sharing::{DecisionKind, OverridePermissions};
use oagw::control_plane::shadow::contributed;
use oagw::domain::effective::{AncestorBinding, ContributedFamilies, Family};
use oagw::domain::upstream::{
    AuthConfig, CorsConfig, PluginsConfig, RateLimitConfig, SharingMode, Upstream,
};
use toolkit_security::SecurityContext;

/// Builds the binding one ancestor row produces, from a row that carries the
/// four families in the one mode the test names.
fn binding(sharing: SharingMode) -> AncestorBinding {
    let mut upstream = row();
    match sharing {
        SharingMode::Enforce | SharingMode::Inherit => {
            upstream.auth = Some(AuthConfig {
                r#type: Some(String::from("gts.cf.core.oagw.auth_plugin.v1~x.v1")),
                sharing: Some(sharing),
                config: None,
            });
            upstream.rate_limit = Some(RateLimitConfig {
                sharing: Some(sharing),
                algorithm: None,
                sustained: None,
                burst: None,
                scope: None,
                strategy: None,
                cost: None,
            });
            upstream.plugins = Some(PluginsConfig {
                sharing: Some(sharing),
                items: Vec::new(),
            });
            upstream.cors = Some(CorsConfig {
                sharing: Some(sharing),
                enabled: true,
                allowed_origins: Vec::new(),
                allowed_methods: Vec::new(),
                expose_headers: Vec::new(),
                allow_credentials: false,
            });
        }
        SharingMode::Private => {}
    }

    binding_of(upstream, 1)
}

/// Builds the binding one ancestor row produces, from a row that carries the
/// one family the test names in the one mode the test names.
fn binding_with(family: Family, sharing: SharingMode) -> AncestorBinding {
    let mut upstream = row();
    match (family, sharing) {
        (Family::Auth, SharingMode::Enforce | SharingMode::Inherit) => {
            upstream.auth = Some(AuthConfig {
                r#type: Some(String::from("gts.cf.core.oagw.auth_plugin.v1~x.v1")),
                sharing: Some(sharing),
                config: None,
            });
        }
        (Family::RateLimit, SharingMode::Enforce | SharingMode::Inherit) => {
            upstream.rate_limit = Some(RateLimitConfig {
                sharing: Some(sharing),
                algorithm: None,
                sustained: None,
                burst: None,
                scope: None,
                strategy: None,
                cost: None,
            });
        }
        (Family::Plugins, SharingMode::Enforce | SharingMode::Inherit) => {
            upstream.plugins = Some(PluginsConfig {
                sharing: Some(sharing),
                items: Vec::new(),
            });
        }
        (Family::Cors, SharingMode::Enforce | SharingMode::Inherit) => {
            upstream.cors = Some(CorsConfig {
                sharing: Some(sharing),
                enabled: true,
                allowed_origins: Vec::new(),
                allowed_methods: Vec::new(),
                expose_headers: Vec::new(),
                allow_credentials: false,
            });
        }
        _ => {}
    }
    binding_of(upstream, 1)
}

/// The upstream row a test binding carries, before any family is set.
fn row() -> Upstream {
    Upstream::new(
        uuid::Uuid::new_v4(),
        oagw::ServerConfig {
            endpoints: Vec::new(),
        },
        String::from("gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"),
    )
}

/// The binding one stored ancestor row produces.
fn binding_of(upstream: Upstream, depth: usize) -> AncestorBinding {
    AncestorBinding {
        tenant_id: uuid::Uuid::from_u128(0xa001),
        depth,
        upstream_id: upstream.id,
        enabled: true,
        contributed: contributed(&upstream),
    }
}

/// A binding that carries no family at all.
fn bare() -> AncestorBinding {
    AncestorBinding {
        tenant_id: uuid::Uuid::from_u128(0xa002),
        depth: 2,
        upstream_id: uuid::Uuid::new_v4(),
        enabled: true,
        contributed: ContributedFamilies {
            auth: None,
            rate_limit: None,
            plugins: None,
            cors: None,
            tags: None,
        },
    }
}

/// The four families the body carries, in the order the merge reads them.
const ALL: [Family; 4] = [
    Family::Auth,
    Family::RateLimit,
    Family::Plugins,
    Family::Cors,
];

/// The token that holds every one of the four permissions.
fn all_held() -> OverridePermissions {
    OverridePermissions::of(
        &SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::from_u128(0xb001))
            .token_scopes(vec![
                String::from("oagw:upstream:bind"),
                String::from("oagw:upstream:override_auth"),
                String::from("oagw:upstream:override_rate"),
                String::from("oagw:upstream:add_plugins"),
            ])
            .build()
            .expect("the context builds"),
    )
}

#[test]
fn a_private_ancestor_makes_the_carried_value_the_descendants_own() {
    // A `private` family is never carried into a binding, so the row is the
    // same as a bare ancestor's: the decision is `own` and no permission is
    // consulted.
    let decisions = oagw::control_plane::sharing::decide(&[bare()], &ALL, &all_held())
        .expect("nothing is refused");
    for family in ALL {
        assert_eq!(
            decisions.kind_of(family),
            Some(DecisionKind::Own),
            "{family:?} is the descendant's own configuration"
        );
    }
}

#[test]
fn an_inherit_ancestor_and_a_held_permission_give_the_body_the_override() {
    let ancestor = binding(SharingMode::Inherit);
    let decisions = oagw::control_plane::sharing::decide(&[ancestor], &ALL, &all_held())
        .expect("nothing is refused");
    for family in ALL {
        assert_eq!(
            decisions.kind_of(family),
            Some(DecisionKind::InheritBase),
            "{family:?} overrides the ancestor's base"
        );
    }
}

#[test]
fn an_inherit_ancestor_and_a_missing_permission_refuse_with_403() {
    let ancestor = binding(SharingMode::Inherit);
    let refusal = oagw::control_plane::sharing::decide(&[ancestor], &ALL, &OverridePermissions::none())
        .expect_err("the auth override is refused");
    assert_eq!(refusal.family(), Family::Auth);
    assert_eq!(
        refusal.permission(),
        Some("oagw:upstream:override_auth"),
        "the refusal names the permission the token lacks"
    );
}

#[test]
fn an_enforce_ancestor_and_a_carried_value_refuse_with_400() {
    let ancestor = binding(SharingMode::Enforce);
    let refusal = oagw::control_plane::sharing::decide(&[ancestor], &ALL, &all_held())
        .expect_err("the enforced family is refused");
    assert_eq!(refusal.family(), Family::Auth);
    assert_eq!(refusal.permission(), None, "a 400 is not a permission answer");
}

#[test]
fn an_enforce_ancestor_and_an_omitted_value_force_the_family() {
    let ancestor = binding(SharingMode::Enforce);
    // The body carries nothing, so no family is refused and every family the
    // ancestor contributes is forced.
    let decisions = oagw::control_plane::sharing::decide(&[ancestor], &[], &all_held())
        .expect("nothing is refused");
    for family in ALL {
        assert!(
            decisions.kind_of(family).is_none(),
            "{family:?} is not decided because the body does not carry it"
        );
        assert!(!decisions.writes(family));
    }
}

#[test]
fn the_permission_403_precedes_any_enforce_400() {
    // The nearest ancestor enforces auth; the more distant one inherits the
    // rate limit. The token holds no permission at all, so both families are
    // blocked and the permission refusal must be the one returned.
    let enforced = binding_with(Family::Auth, SharingMode::Enforce);
    let inherited = binding_with(Family::RateLimit, SharingMode::Inherit);
    let refusal = oagw::control_plane::sharing::decide(
        &[enforced, inherited],
        &[Family::Auth, Family::RateLimit],
        &OverridePermissions::none(),
    )
    .expect_err("both families are blocked");
    assert_eq!(
        refusal,
        oagw::control_plane::sharing::Refusal::Permission {
            family: Family::RateLimit
        },
        "the 403 is answered before the 400"
    );
}

#[test]
fn the_enforce_400_is_answered_only_once_the_permission_holds() {
    let inherited = binding(SharingMode::Inherit);
    // The same chain with every permission held: the permission refusal is
    // gone and the enforce refusal is the one that surfaces.
    let refusal = oagw::control_plane::sharing::decide(
        &[binding_with(Family::Auth, SharingMode::Enforce), inherited],
        &[Family::Auth, Family::RateLimit],
        &all_held(),
    )
    .expect_err("the enforced family is still refused");
    assert_eq!(refusal.family(), Family::Auth);
}

#[test]
fn the_cors_family_takes_no_permission_and_the_mode_alone_decides() {
    let ancestor = binding(SharingMode::Inherit);
    let decisions = oagw::control_plane::sharing::decide(
        &[ancestor],
        &[Family::Cors],
        &OverridePermissions::none(),
    )
    .expect("CORS is never gated by a permission");
    assert_eq!(decisions.kind_of(Family::Cors), Some(DecisionKind::InheritBase));
    assert!(decisions.writes(Family::Cors));
}

#[test]
fn an_ancestor_that_carries_no_family_is_never_a_decision_input() {
    // A bare ancestor contributes nothing, so a body that carries every family
    // is decided `own` even with no permission held.
    let decisions = oagw::control_plane::sharing::decide(
        &[bare()],
        &ALL,
        &OverridePermissions::none(),
    )
    .expect("nothing is refused");
    for family in ALL {
        assert_eq!(decisions.kind_of(family), Some(DecisionKind::Own));
    }
}

#[test]
fn the_strictest_mode_among_the_ancestors_decides() {
    // One ancestor enforces the rate limit and a closer one inherits it: the
    // enforce still refuses the write.
    let closer = binding_with(Family::RateLimit, SharingMode::Inherit);
    let distant = binding_with(Family::RateLimit, SharingMode::Enforce);
    let refusal = oagw::control_plane::sharing::decide(
        &[closer, distant],
        &[Family::RateLimit],
        &all_held(),
    )
    .expect_err("the enforce is strictest");
    assert_eq!(refusal.family(), Family::RateLimit);
}

#[test]
fn the_permissions_deny_by_default() {
    let none = OverridePermissions::none();
    for permission in [
        "oagw:upstream:bind",
        "oagw:upstream:override_auth",
        "oagw:upstream:override_rate",
        "oagw:upstream:add_plugins",
    ] {
        assert!(!none.holds(permission), "{permission} is denied by default");
    }
    assert!(!none.holds("oagw:upstream:delete"), "an unknown literal is never held");
    assert!(!none.holds("*"), "the sentinel is not itself a permission");
}

#[test]
fn the_token_scopes_are_the_only_grant_source() {
    let held = all_held();
    for permission in [
        "oagw:upstream:bind",
        "oagw:upstream:override_auth",
        "oagw:upstream:override_rate",
        "oagw:upstream:add_plugins",
    ] {
        assert!(held.holds(permission), "the token carries {permission}");
    }

    // The platform's unrestricted sentinel names every one of the four.
    let unrestricted = OverridePermissions::of(
        &SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::from_u128(0xb001))
            .token_scopes(vec![String::from("*")])
            .build()
            .expect("the context builds"),
    );
    for permission in [
        "oagw:upstream:bind",
        "oagw:upstream:override_auth",
        "oagw:upstream:override_rate",
        "oagw:upstream:add_plugins",
    ] {
        assert!(unrestricted.holds(permission), "the sentinel grants {permission}");
    }

    // One scope grants exactly one permission.
    let single = OverridePermissions::of(
        &SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::from_u128(0xb001))
            .token_scopes(vec![String::from("oagw:upstream:override_rate")])
            .build()
            .expect("the context builds"),
    );
    assert!(single.holds("oagw:upstream:override_rate"));
    assert!(!single.holds("oagw:upstream:bind"));
    assert!(!single.holds("oagw:upstream:override_auth"));
    assert!(!single.holds("oagw:upstream:add_plugins"));
}

#[test]
fn the_same_four_permissions_gate_the_families_of_a_route_row() {
    // A route carries three of the four sharing-bearing families and no auth
    // family at all, so the decision over a route's carried set is the same
    // family-driven decision: the permission names the override ability, not a
    // table.
    let ancestor = binding(SharingMode::Inherit);
    let carried = [Family::RateLimit, Family::Plugins, Family::Cors];

    let permitted =
        oagw::control_plane::sharing::decide(std::slice::from_ref(&ancestor), &carried, &all_held())
            .expect("every family is overridable");
    for family in carried {
        assert_eq!(permitted.kind_of(family), Some(DecisionKind::InheritBase));
    }

    let refusal =
        oagw::control_plane::sharing::decide(&[ancestor], &carried, &OverridePermissions::none())
            .expect_err("the rate limit override is refused");
    assert_eq!(refusal.family(), Family::RateLimit);
}

#[test]
fn the_decision_reports_which_families_the_body_may_write() {
    let ancestor = binding(SharingMode::Inherit);
    let decisions = oagw::control_plane::sharing::decide(&[ancestor], &ALL, &all_held())
        .expect("nothing is refused");
    for family in ALL {
        assert!(decisions.writes(family), "{family:?} reaches the row");
        assert!(!decisions.forced(family), "{family:?} is not forced");
    }
}
