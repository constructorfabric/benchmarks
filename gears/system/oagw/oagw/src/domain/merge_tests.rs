//! Unit tests for the effective-configuration merge engine
//! (`cpt-cf-oagw-algo-gear-foundation-config-merge`, `inst-gf-merge-1..14`).
// @cpt-dod:cpt-cf-oagw-dod-gear-foundation-merge-engine:p1

use uuid::Uuid;

use super::*;
use crate::domain::dto::{
    AuthConfig, CorsConfig, HttpMethod, HttpMatch, MatchConfig, PathSuffixMode, PluginsConfig,
    RateAlgorithm, RateLimitConfig, Route, SharingMode, SustainedRate, Upstream,
};

fn tenant_a() -> Uuid {
    Uuid::from_u128(0xA001)
}

fn tenant_b() -> Uuid {
    Uuid::from_u128(0xB002)
}

fn rate(rate: u32, sharing: SharingMode) -> RateLimitConfig {
    RateLimitConfig {
        sharing,
        algorithm: RateAlgorithm::TokenBucket,
        sustained: SustainedRate { rate, window: crate::domain::dto::RateWindow::Minute },
        burst: None,
        budget: None,
        scope: crate::domain::dto::RateScope::Tenant,
        strategy: crate::domain::dto::RateStrategy::Reject,
        cost: 1,
        response_headers: true,
    }
}

fn auth(sharing: SharingMode, key: &str) -> AuthConfig {
    AuthConfig {
        auth_type: Some(key.to_owned()),
        sharing,
        config: Some(serde_json::json!({ "api_key_ref": "cred://t/key" })),
    }
}

fn cors(sharing: SharingMode, origins: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing,
        enabled: true,
        allowed_origins: Some(origins.iter().map(|o| (*o).to_owned()).collect()),
        ..CorsConfig::default()
    }
}

fn plugins(sharing: SharingMode, items: &[&str]) -> PluginsConfig {
    PluginsConfig {
        sharing,
        items: items.iter().map(|i| (*i).to_owned()).collect(),
    }
}

fn upstream(tenant_id: Uuid) -> Upstream {
    Upstream {
        id: Uuid::nil(),
        tenant_id,
        alias: "api.vendor.com".to_owned(),
        protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: crate::domain::dto::ServerConfig {
            endpoints: vec![crate::domain::dto::Endpoint {
                scheme: crate::domain::dto::EndpointScheme::Https,
                host: "api.vendor.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

fn route(tenant_id: Uuid, upstream_id: Uuid) -> Route {
    Route {
        id: Uuid::nil(),
        tenant_id,
        upstream_id,
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

/// `inst-gf-merge-1`: the upstream base layer is the initial effective value.
#[test]
fn the_upstream_base_layer_is_the_initial_effective_value() {
    let mut base = upstream(tenant_a());
    base.rate_limit = Some(rate(100, SharingMode::Private));
    let layers = vec![upstream_base_layer(&base)];
    let effective = merge(&layers, tenant_a(), &OverridePermissions::NONE);
    assert_eq!(effective.enabled, Some(true));
    assert_eq!(effective.protocol.as_deref(), Some(crate::domain::gts_helpers::PROTOCOL_HTTP));
    assert_eq!(effective.rate_limit.as_ref().map(|r| r.sustained.rate), Some(100));
    assert!(effective.tags.is_empty(), "no tags declared -> none");
}

/// `inst-gf-merge-2`: layers are consumed in increasing priority.
#[test]
fn layers_are_consumed_in_increasing_priority() {
    let mut base = upstream(tenant_a());
    base.rate_limit = Some(rate(100, SharingMode::Inherit));
    let mut layer = route_layer(&route(tenant_a(), base.id));
    layer.rate_limit = Some(rate(40, SharingMode::Inherit));
    let layers = vec![upstream_base_layer(&base), layer];
    let effective = merge(&layers, tenant_a(), &OverridePermissions::ALL);
    assert_eq!(effective.rate_limit.as_ref().map(|r| r.sustained.rate), Some(40));
}

/// `inst-gf-merge-3` / `-4`: a `private` ancestor field is invisible to a
/// descendant requester, and the requester's own layer still sees its own.
#[test]
fn a_private_ancestor_field_is_invisible_to_a_descendant() {
    let mut base = upstream(tenant_a());
    base.auth = Some(auth(SharingMode::Private, "noop-auth"));
    let layers = vec![upstream_base_layer(&base)];
    // A descendant requester sees no auth at all.
    let descendant = merge(&layers, tenant_b(), &OverridePermissions::ALL);
    assert!(descendant.auth.is_none(), "private ancestor auth is invisible");
    // The owner still sees its own value.
    let owner = merge(&layers, tenant_a(), &OverridePermissions::NONE);
    assert_eq!(owner.auth.as_ref().and_then(|a| a.auth_type.as_deref()), Some("noop-auth"));
}

/// `inst-gf-merge-5` / `-6`: `enforce` on the ancestor discards every
/// descendant value.
#[test]
fn an_enforced_ancestor_value_discards_descendant_values() {
    let mut base = upstream(tenant_a());
    base.auth = Some(auth(SharingMode::Enforce, "noop-auth"));
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.sharing.auth = Sharing::Inherit;
    layer.auth = Some(auth(SharingMode::Inherit, "bearer"));
    let layers = vec![upstream_base_layer(&base), layer];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::ALL);
    assert_eq!(
        effective.auth.as_ref().and_then(|a| a.auth_type.as_deref()),
        Some("noop-auth"),
        "enforce wins over the descendant value"
    );
}

/// An enforced ancestor limit survives a descendant route that declares its
/// own rate limit (alias shadowing across the tenant chain).
#[test]
fn an_enforced_rate_limit_survives_descendant_shadowing() {
    let mut base = upstream(tenant_a());
    base.rate_limit = Some(rate(100, SharingMode::Enforce));
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.rate_limit = Some(rate(5, SharingMode::Inherit));
    let layers = vec![upstream_base_layer(&base), layer];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::ALL);
    assert_eq!(effective.rate_limit.as_ref().map(|r| r.sustained.rate), Some(100));
}

/// `inst-gf-merge-7`: auth merges by override, gated on
/// `oagw:upstream:override_auth`.
#[test]
fn auth_override_is_gated_on_the_override_auth_permission() {
    let mut base = upstream(tenant_a());
    base.auth = Some(auth(SharingMode::Inherit, "noop-auth"));

    let without = merge(&[upstream_base_layer(&base)], tenant_b(), &OverridePermissions::NONE);
    assert_eq!(
        without.auth.as_ref().and_then(|a| a.auth_type.as_deref()),
        Some("noop-auth"),
        "no permission -> the ancestor value stands, no error surfaced"
    );

    let with = merge(&[upstream_base_layer(&base)], tenant_b(), &OverridePermissions::ALL);
    assert_eq!(
        with.auth.as_ref().and_then(|a| a.auth_type.as_deref()),
        Some("noop-auth"),
        "permission held but no descendant value -> the ancestor value stands"
    );
}

/// A descendant `inherit` auth block replaces the ancestor's only when the
/// override permission is held.
#[test]
fn a_descendant_auth_block_needs_the_override_permission() {
    let mut base = upstream(tenant_a());
    base.auth = Some(auth(SharingMode::Inherit, "noop-auth"));
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.sharing.auth = Sharing::Inherit;
    layer.auth = Some(auth(SharingMode::Inherit, "bearer"));

    let denied = merge(&[upstream_base_layer(&base), layer.clone()], tenant_b(), &OverridePermissions::NONE);
    assert_eq!(denied.auth.as_ref().and_then(|a| a.auth_type.as_deref()), Some("noop-auth"));

    let granted =
        merge(&[upstream_base_layer(&base), layer], tenant_b(), &OverridePermissions::ALL);
    assert_eq!(granted.auth.as_ref().and_then(|a| a.auth_type.as_deref()), Some("bearer"));
}

/// `inst-gf-merge-8`: rate limits merge by `min(ancestor, descendant)`, gated
/// on `oagw:upstream:override_rate`.
#[test]
fn rate_limits_merge_by_min_and_are_permission_gated() {
    let mut base = upstream(tenant_a());
    base.rate_limit = Some(rate(100, SharingMode::Inherit));
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.rate_limit = Some(rate(30, SharingMode::Inherit));

    let denied = merge(&[upstream_base_layer(&base), layer.clone()], tenant_b(), &OverridePermissions::NONE);
    assert_eq!(
        denied.rate_limit.as_ref().map(|r| r.sustained.rate),
        Some(100),
        "no override_rate permission -> the ancestor limit stands"
    );

    let granted =
        merge(&[upstream_base_layer(&base), layer], tenant_b(), &OverridePermissions::ALL);
    assert_eq!(
        granted.rate_limit.as_ref().map(|r| r.sustained.rate),
        Some(30),
        "min(100, 30) = 30"
    );
}

/// `inst-gf-merge-9`: tags merge by add-only union.
#[test]
fn tags_merge_by_add_only_union() {
    let mut base = upstream(tenant_a());
    base.tags = vec!["openai".to_owned(), "llm".to_owned()];
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.tags = Some(vec!["llm".to_owned(), "beta".to_owned()]);
    let layers = vec![upstream_base_layer(&base), layer];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::NONE);
    assert_eq!(effective.tags, vec!["openai".to_owned(), "llm".to_owned(), "beta".to_owned()]);
}

/// A `private` ancestor tag list is invisible to a descendant.
#[test]
fn a_private_ancestor_tag_list_is_invisible() {
    let mut base = upstream(tenant_a());
    base.tags = vec!["internal".to_owned()];
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.tags = Some(vec!["beta".to_owned()]);
    // The upstream layer declares tags `private` on the scalars slot only, so
    // drive the visibility through a private tags sharing mode.
    let mut base_layer = upstream_base_layer(&base);
    base_layer.sharing.tags = Sharing::Private;
    let layers = vec![base_layer, layer];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::NONE);
    assert_eq!(effective.tags, vec!["beta".to_owned()]);
}

/// `inst-gf-merge-10`: CORS origins union under `inherit` and stay as-is under
/// `enforce`.
#[test]
fn cors_origins_union_under_inherit_and_stand_as_is_under_enforce() {
    let mut base = upstream(tenant_a());
    base.cors = Some(cors(SharingMode::Inherit, &["https://a.vendor.com"]));

    let inherit = {
        let mut layer = route_layer(&route(tenant_b(), base.id));
        layer.sharing.cors = Sharing::Inherit;
        layer.cors = Some(cors(SharingMode::Inherit, &["https://b.vendor.com", "https://a.vendor.com"]));
        let layers = vec![upstream_base_layer(&base), layer];
        merge(&layers, tenant_b(), &OverridePermissions::NONE)
    };
    assert_eq!(
        inherit.cors.and_then(|c| c.allowed_origins),
        Some(vec![
            "https://a.vendor.com".to_owned(),
            "https://b.vendor.com".to_owned()
        ])
    );

    let enforce = {
        let mut layer = route_layer(&route(tenant_b(), base.id));
        layer.sharing.cors = Sharing::Inherit;
        layer.cors = Some(cors(SharingMode::Inherit, &["https://b.vendor.com"]));
        let mut ancestor = upstream_base_layer(&base);
        ancestor.sharing.cors = Sharing::Enforce;
        let layers = vec![ancestor, layer];
        merge(&layers, tenant_b(), &OverridePermissions::NONE)
    };
    assert_eq!(
        enforce.cors.and_then(|c| c.allowed_origins),
        Some(vec!["https://a.vendor.com".to_owned()]),
        "the enforced ancestor origin set stays as-is"
    );
}

/// `inst-gf-merge-11`: plugin chains concatenate, gated on
/// `oagw:upstream:add_plugins`.
#[test]
fn plugin_chains_concatenate_and_are_permission_gated() {
    let mut base = upstream(tenant_a());
    base.plugins = Some(plugins(SharingMode::Inherit, &["a"]));
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.sharing.plugins = Sharing::Inherit;
    layer.plugins = Some(plugins(SharingMode::Inherit, &["b"]));

    let denied = merge(&[upstream_base_layer(&base), layer.clone()], tenant_b(), &OverridePermissions::NONE);
    assert_eq!(
        denied.plugins.as_ref().map(|p| p.items.clone()),
        Some(vec!["a".to_owned()]),
        "no add_plugins permission -> the ancestor chain stands"
    );

    let granted = merge(&[upstream_base_layer(&base), layer], tenant_b(), &OverridePermissions::ALL);
    assert_eq!(
        granted.plugins.as_ref().map(|p| p.items.clone()),
        Some(vec!["a".to_owned(), "b".to_owned()]),
        "ancestor chain first, descendant appended"
    );
}

/// `inst-gf-merge-12`: scalars take the more specific value.
#[test]
fn scalars_take_the_more_specific_value() {
    let mut base = upstream(tenant_a());
    base.enabled = true;
    let mut layer = route_layer(&route(tenant_b(), base.id));
    layer.enabled = Some(false);
    let layers = vec![upstream_base_layer(&base), layer];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::NONE);
    assert_eq!(effective.enabled, Some(false), "the route layer is more specific");
}

/// A tenant-chain layer after the route layer wins the scalar slots.
#[test]
fn the_tenant_chain_is_walked_root_to_leaf() {
    let base = upstream(tenant_a());
    let root = tenant_layer(Uuid::from_u128(0x100));
    let leaf = tenant_layer(tenant_b());
    let mut root = root;
    root.enabled = Some(true);
    let mut leaf = leaf;
    leaf.enabled = Some(false);
    let layers = vec![upstream_base_layer(&base), root, leaf];
    let effective = merge(&layers, tenant_b(), &OverridePermissions::NONE);
    assert_eq!(effective.enabled, Some(false), "the leaf tenant layer wins");
}

/// `inst-gf-merge-13`: a field no layer specifies stays absent.
#[test]
fn a_field_no_layer_specifies_stays_absent() {
    let base = upstream(tenant_a());
    let layers = vec![upstream_base_layer(&base)];
    let effective = merge(&layers, tenant_a(), &OverridePermissions::NONE);
    assert!(effective.auth.is_none());
    assert!(effective.headers.is_none());
    assert!(effective.rate_limit.is_none());
    assert!(effective.cors.is_none());
    assert!(effective.plugins.is_none());
}

/// `inst-gf-merge-14`: the merge returns one effective configuration, and it
/// is recomputed per request rather than cached.
#[test]
fn the_merge_returns_one_effective_configuration() {
    let mut base = upstream(tenant_a());
    base.auth = Some(auth(SharingMode::Inherit, "noop-auth"));
    base.rate_limit = Some(rate(10, SharingMode::Inherit));
    base.cors = Some(cors(SharingMode::Inherit, &["https://a.vendor.com"]));
    base.plugins = Some(plugins(SharingMode::Inherit, &["noop-auth"]));
    base.tags = vec!["openai".to_owned()];
    let layers = vec![upstream_base_layer(&base)];
    let effective = merge(&layers, tenant_a(), &OverridePermissions::ALL);
    assert_eq!(effective.auth.as_ref().and_then(|a| a.auth_type.as_deref()), Some("noop-auth"));
    assert_eq!(effective.rate_limit.as_ref().map(|r| r.sustained.rate), Some(10));
    assert_eq!(
        effective.cors.as_ref().and_then(|c| c.allowed_origins.as_ref().cloned()),
        Some(vec!["https://a.vendor.com".to_owned()])
    );
    assert_eq!(effective.plugins.as_ref().map(|p| p.items.clone()), Some(vec!["noop-auth".to_owned()]));
    assert_eq!(effective.tags, vec!["openai".to_owned()]);
    // Recomputing with the same input yields the same result (pure function).
    let again = merge(&layers, tenant_a(), &OverridePermissions::ALL);
    assert_eq!(again, effective);
}

/// `inst-gf-eff-8`: the override permissions are projected from the granted
/// hierarchy permission strings.
#[test]
fn override_permissions_are_projected_from_granted_permissions() {
    let granted = OverridePermissions::from_granted([
        crate::domain::gts_helpers::PERM_OVERRIDE_AUTH,
        "oagw:unrelated:read",
    ]);
    assert!(granted.override_auth);
    assert!(!granted.override_rate);
    assert!(!granted.add_plugins);
    assert_eq!(OverridePermissions::NONE, OverridePermissions::from_granted([]));
    assert_eq!(OverridePermissions::ALL, OverridePermissions::from_granted([
        crate::domain::gts_helpers::PERM_OVERRIDE_AUTH,
        crate::domain::gts_helpers::PERM_OVERRIDE_RATE,
        crate::domain::gts_helpers::PERM_ADD_PLUGINS,
    ]));
}

/// The invalid-value echo the error contract permits is bounded.
#[test]
fn the_target_host_echo_is_bounded() {
    let long = "x".repeat(400);
    let bounded = bound_target_host_echo(&long);
    assert!(bounded.len() <= 128);
    assert!(!bounded.contains('\n'));
}
