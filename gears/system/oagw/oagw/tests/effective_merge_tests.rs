//! The per-field-family effective merge and the resolution entry point.
//!
//! Covers `cpt-cf-oagw-dod-field-family-merge`,
//! `cpt-cf-oagw-dod-alias-shadowing`, and
//! `cpt-cf-oagw-dod-effective-config-result`: every strategy row of the merge
//! table for every family, the effective `enabled` state, the common scale the
//! rate minimum is decided on, and the fail-closed exits of
//! `cpt-cf-oagw-flow-resolve-effective-config`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

// @cpt-dod:cpt-cf-oagw-dod-resolution-tests:p1

use std::sync::Arc;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::plugin_def;
use oagw::domain::plugin_contract::PluginFamily;
use oagw::control_plane::effective::{ResolveError, resolve_effective};
use oagw::control_plane::service::ManagementService;
use oagw::control_plane::effective::EffectiveResolution;
use oagw::domain::effective::{EffectiveUpstreamConfig, RouteSelector};
use oagw::domain::upstream::{PluginsConfig, ServerConfig, SharingMode, Upstream};
use oagw::{AuthConfig, CorsConfig, Endpoint, EndpointHost, Scheme};
use oagw::store::OagwStore;
use oagw::{OagwConfig, UpstreamRow};
use serde_json::{Value, json};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

const ALIAS: &str = "api.openai.com";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// The calling tenant and its single ancestor, in chain order.
fn chain() -> (Uuid, Uuid) {
    (tenant(0xb001), tenant(0xb002))
}

fn service_with_store() -> (ManagementService, Arc<OagwStore>) {
    let store = Arc::new(OagwStore::new());
    let service = ManagementService::new(
        Arc::clone(&store),
        &OagwConfig::default(),
        Arc::new(ControlPlaneCache::new()),
    )
    .expect("the validators compile");
    (service, store)
}

/// An upstream body whose endpoints derive the alias, with no family set.
fn base(alias: &str) -> Value {
    json!({
        "server": { "endpoints": [{ "scheme": "https", "host": alias, "port": 443 }] },
        "protocol": HTTP_PROTOCOL,
        "alias": alias,
        "tags": []
    })
}

fn with_families(body: Value, families: Value) -> Value {
    let mut object = body.as_object().expect("the body is an object").clone();
    for (key, value) in families.as_object().expect("the families are an object") {
        object.insert(key.clone(), value.clone());
    }
    Value::Object(object)
}

fn auth(sharing: &str, kind: &str) -> Value {
    json!({ "sharing": sharing, "type": kind, "config": { "key": "leaf" } })
}

fn rate_limit(sharing: &str, rate: u64, window: &str, capacity: u64) -> Value {
    json!({
        "sharing": sharing,
        "algorithm": "token_bucket",
        "sustained": { "rate": rate, "window": window },
        "burst": { "capacity": capacity },
        "scope": "tenant",
        "strategy": "reject",
        "cost": 1
    })
}

fn rate_limit_with(
    sharing: &str,
    rate: u64,
    window: &str,
    capacity: u64,
    overrides: Value,
) -> Value {
    let mut body = rate_limit(sharing, rate, window, capacity);
    for (key, value) in overrides.as_object().expect("the overrides are an object") {
        body[key] = value.clone();
    }
    body
}

fn plugins(sharing: &str, items: &[&str]) -> Value {
    json!({ "sharing": sharing, "items": items })
}

/// A built-in plugin identifier, the form the shipped schema describes for a
/// `plugins.items` entry.
const PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";

/// Creates one custom transform plugin for the tenant and answers the
/// anonymous identifier it is addressed by.
///
/// The chain-composition tests of the plugin feature resolve every binding
/// item before any row is written, so a binding that names no resolvable
/// plugin is refused: the route bodies below bind a plugin row the test
/// creates first.
fn other_plugin(management: &ManagementService, tenant: Uuid) -> String {
    let row = management
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": "route-tag",
                "phases": ["on_response"],
                "source_code": "def on_response(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom plugin is created");
    plugin_def::plugin_instance(PluginFamily::Transform, row.plugin.id)
}
/// The two resolvable auth identifiers the effective-merge tests bind: the
/// ancestor's and the descendant's own, distinct so no assertion can pass by
/// reading one where the other was written.
const ROOT_AUTH: &str = oagw::gts::plugin_catalog::AUTH_NOOP;
const LEAF_AUTH: &str = oagw::gts::plugin_catalog::AUTH_APIKEY;

const OTHER_PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.tag.v1";
const THIRD_PLUGIN: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.correlation_id.v1";

/// Builds the upstream value one store-level row holds, with no family set.
///
/// The shipped `upstream.v1` schema states `plugins.items` as a `oneOf` of two
/// `type: string` branches, so every non-empty item array is rejected by the
/// write path the two branches are indistinguishable under: one entry can
/// never satisfy exactly one of two identical subschemas. The merge this
/// feature delivers is downstream of that write-path defect, so the rows that
/// carry a chain are supplied through [`OagwStore::insert_upstream`], which
/// takes the validated domain value directly, and the item shape of
/// `cpt-cf-oagw-feature-plugin-system`, which owns it, is not exercised here.
fn stored(alias: &str) -> Upstream {
    let mut upstream = Upstream::new(
        Uuid::new_v4(),
        ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: EndpointHost::parse(alias).expect("a valid endpoint host"),
                port: Some(443),
            }],
        },
        String::from(HTTP_PROTOCOL),
    );
    upstream.alias = Some(String::from(alias));
    upstream
}

/// Inserts the value, answering the stored row.
fn insert(store: &OagwStore, owner: Uuid, upstream: &Upstream) -> UpstreamRow {
    store
        .insert_upstream(owner, upstream)
        .expect("the row is inserted")
}

/// The `plugins` family one store-level row carries.
fn shared_plugins(sharing: SharingMode, items: &[&str]) -> PluginsConfig {
    PluginsConfig {
        sharing: Some(sharing),
        items: items.iter().map(|item| String::from(*item)).collect(),
    }
}

fn cors(sharing: &str, origins: &[&str]) -> Value {
    json!({
        "sharing": sharing,
        "enabled": true,
        "allowed_origins": origins,
        "allowed_methods": ["GET"],
        "allow_credentials": false
    })
}

fn create(service: &ManagementService, owner: Uuid, body: &Value) -> UpstreamRow {
    service
        .create_upstream(owner, body)
        .expect("the create succeeds")
}

fn http_selector(path: &str) -> RouteSelector {
    RouteSelector::Http {
        method: "GET".to_string(),
        path: path.to_string(),
    }
}

/// Resolves the upstream layer for one alias against one chain.
fn resolve_upstream(
    store: &OagwStore,
    calling: Uuid,
    ancestors: &[Uuid],
    alias: &str,
) -> Option<EffectiveUpstreamConfig> {
    resolve_effective(store, calling, ancestors, alias, &http_selector("/v1/chat"))
        .expect("an ordered chain")
        .map(|answer| answer.upstream)
}

/// Resolves the whole answer for one alias against one chain.
fn resolve_all(
    store: &OagwStore,
    calling: Uuid,
    ancestors: &[Uuid],
    alias: &str,
) -> Option<EffectiveResolution> {
    resolve_effective(store, calling, ancestors, alias, &http_selector("/v1/chat"))
        .expect("an ordered chain")
}

#[test]
fn a_descendants_row_shadows_an_ancestors_but_its_enforce_families_still_apply() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let ancestor = create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({
                "auth": auth("enforce", ROOT_AUTH),
                "rate_limit": rate_limit("enforce", 10_000, "minute", 1_000),
                "tags": ["root"]
            }),
        ),
    );
    let descendant = create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({
                "auth": auth("private", LEAF_AUTH),
                "rate_limit": rate_limit("private", 100, "minute", 100),
                "tags": ["leaf"]
            }),
        ),
    );

    let answer = resolve_all(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert!(answer.enabled);
    assert_eq!(answer.upstream.upstream_id, descendant.upstream.id);
    assert_eq!(answer.upstream.tenant_id, leaf, "the descendant is the target");

    let forced = answer.upstream.auth.expect("the enforce ancestor forces auth");
    assert_eq!(forced.owner, root);
    assert_eq!(forced.mode, SharingMode::Enforce);
    assert_eq!(forced.auth.r#type.as_deref(), Some(ROOT_AUTH));
    assert!(
        ancestor.upstream.enabled,
        "the ancestor's rows are unchanged by a resolution"
    );
}

#[test]
fn an_alias_held_only_by_an_ancestor_makes_it_the_target_and_its_inherit_families_the_base() {
    let (_management, store) = service_with_store();
    let (leaf, root) = chain();
    let mut value = stored(ALIAS);
    value.auth = Some(AuthConfig {
        r#type: Some(String::from(ROOT_AUTH)),
        sharing: Some(SharingMode::Inherit),
        config: None,
    });
    value.plugins = Some(shared_plugins(SharingMode::Inherit, &[OTHER_PLUGIN]));
    value.tags = vec![String::from("root")];
    let ancestor = insert(&store, root, &value);

    let answer = resolve_all(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert_eq!(answer.upstream.upstream_id, ancestor.upstream.id);
    assert_eq!(answer.upstream.tenant_id, root);

    let inherited = answer.upstream.auth.expect("the ancestor contributes auth");
    assert_eq!(inherited.owner, root);
    assert_eq!(inherited.mode, SharingMode::Inherit);
    let chain = answer
        .upstream
        .plugins
        .expect("the ancestor contributes plugins");
    assert_eq!(chain.items, vec![OTHER_PLUGIN]);
    assert_eq!(chain.owner, root);
}

#[test]
fn an_alias_held_by_no_chain_element_resolves_to_nothing() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    management
        .create_upstream(root, &base("other.example.com"))
        .expect("the create succeeds");

    let answer = resolve_effective(&store, leaf, &[root], ALIAS, &http_selector("/v1/chat"))
        .expect("an ordered chain");
    assert!(answer.is_none(), "the consumer answers 404");
}

#[test]
fn an_ancestor_auth_marked_private_contributes_nothing_and_consumes_no_permission() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "auth": auth("private", ROOT_AUTH) })),
    );
    create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "auth": auth("private", LEAF_AUTH) })),
    );

    let upstream = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    let merged = upstream.auth.expect("the descendant's own auth is effective");
    assert_eq!(merged.owner, leaf);
    assert_eq!(merged.mode, SharingMode::Private);
    assert_eq!(
        merged.auth.r#type.as_deref(),
        Some(LEAF_AUTH),
        "the ancestor's private value is never read into a result"
    );
    assert!(
        !serde_json::to_string(&merged.auth)
            .expect("the auth serializes")
            .contains(ROOT_AUTH),
        "no ancestor value is echoed in the answer"
    );
}

#[test]
fn an_ancestor_auth_marked_inherit_is_the_base_a_descendants_own_object_replaces() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "auth": auth("inherit", ROOT_AUTH) })),
    );
    let without_own = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    let inherited = without_own.auth.expect("the base is the ancestor's object");
    assert_eq!(inherited.auth.r#type.as_deref(), Some(ROOT_AUTH));

    create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "auth": auth("private", LEAF_AUTH) })),
    );
    let with_own = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    let overridden = with_own.auth.expect("the descendant's own object is effective");
    assert_eq!(overridden.auth.r#type.as_deref(), Some(LEAF_AUTH));
    assert_eq!(overridden.owner, leaf);
}

#[test]
fn an_ancestor_auth_marked_enforce_is_effective_regardless_of_the_descendant() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "auth": auth("enforce", ROOT_AUTH) })),
    );
    create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "auth": auth("private", LEAF_AUTH) })),
    );

    let upstream = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    let forced = upstream.auth.expect("the ancestor's object is effective");
    assert_eq!(forced.auth.r#type.as_deref(), Some(ROOT_AUTH));
    assert_eq!(forced.owner, root);
    assert_eq!(forced.mode, SharingMode::Enforce);
}

#[test]
fn the_rate_limit_resolves_to_the_minimum_of_the_visible_rates() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("enforce", 10_000, "minute", 1_000) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("private", 100, "minute", 100) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .rate_limit
        .expect("the descendant's limit is visible");
    assert_eq!(merged.rate_limit.sustained.expect("a sustained rate").rate, 100);
    assert_eq!(merged.owner, leaf, "the descendant's value supplied the minimum");
}

#[test]
fn a_looser_descendant_rate_limit_cannot_exceed_an_enforced_ancestor() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("enforce", 10_000, "minute", 1_000) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("private", 20_000, "minute", 2_000) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .rate_limit
        .expect("the ancestor's limit is visible");
    assert_eq!(
        merged.rate_limit.sustained.expect("a sustained rate").rate,
        10_000,
        "the ancestor's limit is the effective one"
    );
    assert_eq!(merged.owner, root);
    assert_eq!(merged.mode, SharingMode::Enforce);
}

#[test]
fn the_rate_minimum_is_decided_on_a_common_scale_and_reported_in_the_winners_window() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("inherit", 5_000, "minute", 500) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("private", 100, "second", 600) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .rate_limit
        .expect("both limits are visible");
    let sustained = merged.rate_limit.sustained.expect("a sustained rate");
    assert_eq!(sustained.rate, 5_000, "5000/minute is stricter than 100/second");
    assert_eq!(
        sustained.window,
        Some(oagw::domain::upstream::Window::Minute),
        "the winner's window is reported"
    );
    assert_eq!(merged.rate_limit.burst.expect("a burst").capacity, 500);
}

#[test]
fn the_burst_capacity_is_minimized_and_the_remaining_members_are_never_merged() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit_with("enforce", 5_000, "minute", 1_000, json!({
                "algorithm": "sliding_window",
                "scope": "ip",
                "strategy": "queue",
                "cost": 7
            })) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("private", 100, "second", 100) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .rate_limit
        .expect("both limits are visible");
    assert_eq!(merged.rate_limit.burst.expect("a burst").capacity, 100);
    assert_eq!(
        merged.rate_limit.algorithm,
        Some(oagw::domain::upstream::Algorithm::TokenBucket),
        "the algorithm is carried unchanged from the routing target's own object"
    );
    assert_eq!(
        merged.rate_limit.scope,
        Some(oagw::domain::upstream::RateLimitScope::Tenant)
    );
    assert_eq!(
        merged.rate_limit.strategy,
        Some(oagw::domain::upstream::Strategy::Reject)
    );
    assert_eq!(merged.rate_limit.cost, Some(1));
}

#[test]
fn an_ancestor_rate_limit_marked_private_contributes_nothing() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "rate_limit": rate_limit("private", 50, "minute", 50) }),
        ),
    );
    create(&management, leaf, &base(ALIAS));

    let upstream = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert!(
        upstream.rate_limit.is_none(),
        "a descendant with no rate_limit resolves to no limit rather than to the ancestor's"
    );
}

#[test]
fn an_inherited_plugin_chain_is_concatenated_ancestor_then_descendant() {
    let (_management, store) = service_with_store();
    let (leaf, root) = chain();
    let mut ancestor_value = stored(ALIAS);
    ancestor_value.plugins = Some(shared_plugins(SharingMode::Inherit, &[PLUGIN, OTHER_PLUGIN]));
    insert(&store, root, &ancestor_value);

    let mut descendant_value = stored(ALIAS);
    descendant_value.plugins = Some(shared_plugins(SharingMode::Private, &[THIRD_PLUGIN]));
    insert(&store, leaf, &descendant_value);

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .plugins
        .expect("both chains are visible");
    assert_eq!(merged.items, vec![PLUGIN, OTHER_PLUGIN, THIRD_PLUGIN]);
    assert_eq!(merged.owner, leaf, "the nearest items are the descendant's");
    assert_eq!(merged.contributors, vec![root, leaf]);
}

#[test]
fn an_enforce_ancestors_plugin_items_survive_a_replacement_that_omits_them() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let mut value = stored(ALIAS);
    value.plugins = Some(shared_plugins(SharingMode::Enforce, &[PLUGIN]));
    insert(&store, root, &value);
    create(&management, leaf, &base(ALIAS));

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .plugins
        .expect("the ancestor's chain is visible");
    assert_eq!(merged.items, vec![PLUGIN]);
    assert_eq!(merged.owner, root);
    assert_eq!(merged.mode, SharingMode::Enforce);
}

#[test]
fn cors_origins_union_under_inherit() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "cors": cors("inherit", &["https://app.example.com"]) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "cors": cors("private", &["https://admin.example.com"]) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .cors
        .expect("both origins are visible");
    assert_eq!(
        merged.cors.allowed_origins,
        vec!["https://app.example.com", "https://admin.example.com"]
    );
    assert_eq!(merged.owner, leaf, "the routing target's object carries the merge");
    assert_eq!(merged.mode, SharingMode::Inherit);
    assert!(
        !merged.cors.allow_credentials,
        "the union is confined to allowed_origins"
    );
}

#[test]
fn cors_is_forced_under_enforce() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({ "cors": cors("enforce", &["https://root.example.com"]) }),
        ),
    );
    create(
        &management,
        leaf,
        &with_families(
            base(ALIAS),
            json!({ "cors": cors("private", &["https://leaf.example.com"]) }),
        ),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS)
        .expect("a resolution")
        .cors
        .expect("the ancestor's object is forced");
    assert_eq!(merged.cors.allowed_origins, vec!["https://root.example.com"]);
    assert_eq!(merged.owner, root);
    assert_eq!(merged.mode, SharingMode::Enforce);
}

#[test]
fn tags_resolve_to_the_add_only_union() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let ancestor = create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "tags": ["shared", "root"] })),
    );
    let descendant = create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "tags": ["shared", "leaf"] })),
    );

    let merged = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert_eq!(merged.tags.tags, vec!["root", "shared", "leaf"]);
    assert_eq!(merged.tags.contributors, vec![root, leaf]);

    // A descendant row that omits an inherited tag leaves it in the effective
    // set, because the union is computed at resolution time and never stored.
    management
        .replace_upstream(
            leaf,
            descendant.upstream.id,
            &with_families(base(ALIAS), json!({ "tags": ["leaf"] })),
        )
        .expect("the replacement succeeds");
    management
        .replace_upstream(
            root,
            ancestor.upstream.id,
            &with_families(base(ALIAS), json!({ "tags": ["root"] })),
        )
        .expect("the replacement succeeds");
    let replaced = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert_eq!(
        replaced.tags.tags,
        vec!["root", "leaf"],
        "the union is recomputed, never materialized"
    );
    let stored = management
        .read_upstream(leaf, descendant.upstream.id)
        .expect("the row is readable");
    assert_eq!(stored.tags, vec!["leaf"], "the descendant's row holds its own tags");
}

#[test]
fn a_disabled_ancestor_disables_the_effective_state_without_a_write() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let ancestor = create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "tags": ["root"] })),
    );
    create(&management, leaf, &base(ALIAS));

    let enabled = resolve_all(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert!(enabled.enabled, "every matched row is enabled");

    let mut body = base(ALIAS);
    body["enabled"] = json!(false);
    management
        .replace_upstream(root, ancestor.upstream.id, &body)
        .expect("the ancestor disable succeeds");

    let disabled = resolve_all(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert!(
        !disabled.enabled,
        "one disabled ancestor disables the resource for every descendant"
    );
    let stored = management
        .read_upstream(leaf, resolve_upstream(&store, leaf, &[root], ALIAS)
            .expect("a resolution")
            .upstream_id)
        .expect("the descendant's row is readable");
    assert!(
        stored.upstream.enabled,
        "no write reached the descendant's row"
    );
}

#[test]
fn a_descendant_cannot_raise_the_effective_state_an_ancestor_disabled() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let ancestor = create(
        &management,
        root,
        &with_families(base(ALIAS), json!({ "tags": ["root"] })),
    );
    let descendant = create(&management, leaf, &base(ALIAS));
    let mut body = base(ALIAS);
    body["enabled"] = json!(false);
    management
        .replace_upstream(root, ancestor.upstream.id, &body)
        .expect("the ancestor disable succeeds");
    let mut raised = base(ALIAS);
    raised["enabled"] = json!(true);
    management
        .replace_upstream(leaf, descendant.upstream.id, &raised)
        .expect("the descendant's own row is re-enabled");

    let answer = resolve_all(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert!(
        !answer.enabled,
        "no descendant write can raise the effective state"
    );
}

#[test]
fn the_route_layer_merges_with_the_same_strategies_and_carries_no_auth() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let ancestor_upstream = create(
        &management,
        root,
        &with_families(
            base(ALIAS),
            json!({
                "auth": auth("enforce", ROOT_AUTH),
                "rate_limit": rate_limit("enforce", 10_000, "minute", 1_000)
            }),
        ),
    );
    let descendant_upstream = create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "tags": ["leaf"] })),
    );
    let descendant_plugin = other_plugin(&management, leaf);
    let ancestor_route = management
        .create_route(
            root,
            &json!({
                "upstream_id": ancestor_upstream.upstream.id,
                "match": { "http": { "methods": ["GET"], "path": "/v1" } },
                "priority": 1,
                "tags": ["root-route"],
                "rate_limit": rate_limit("inherit", 500, "minute", 50)
            }),
        )
        .expect("the route create succeeds");
    management
        .create_route(
            leaf,
            &json!({
                "upstream_id": descendant_upstream.upstream.id,
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1,
                "tags": ["leaf-route"],
                "plugins": plugins("private", &[&descendant_plugin]),
            }),
        )
        .expect("the route create succeeds");

    let answer = resolve_effective(&store, leaf, &[root], ALIAS, &http_selector("/v1/chat"))
        .expect("an ordered chain")
        .expect("a resolution");
    let route = answer.route.expect("the chain holds a matching route");
    assert_eq!(route.tenant_id, leaf, "the descendant's route wins");
    assert_eq!(route.tags.tags, vec!["root-route", "leaf-route"]);
    let merged = route.rate_limit.expect("both route limits are visible");
    assert_eq!(
        merged.rate_limit.sustained.expect("a sustained rate").rate,
        500,
        "the ancestor route's inherit limit participates in the minimum"
    );
    assert_eq!(
        route.plugins.expect("the descendant's chain").items,
        vec![descendant_plugin],
        "the ancestor route contributes no plugins it did not share"
    );
    assert!(
        ancestor_route.route.plugins.is_none(),
        "the ancestor route carries no plugins object of its own"
    );
}

#[test]
fn a_route_of_a_tenant_outside_the_chain_is_never_a_candidate() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let outsider = tenant(0xb999);
    let upstream = create(&management, root, &base(ALIAS));
    create(&management, leaf, &base(ALIAS));
    let refused = management.create_route(
        outsider,
        &json!({
            "upstream_id": upstream.upstream.id,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "priority": 1
        }),
    );
    assert!(
        refused.is_err(),
        "a route of a tenant outside the chain is never writable onto another tenant's upstream"
    );

    let answer = resolve_effective(&store, leaf, &[root], ALIAS, &http_selector("/v1"))
        .expect("an ordered chain")
        .expect("a resolution");
    assert!(
        answer.route.is_none(),
        "another tenant's route is never a candidate"
    );
}

#[test]
fn a_cyclic_chain_fails_the_resolution_closed() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    create(&management, leaf, &base(ALIAS));

    let answer = resolve_effective(&store, leaf, &[root, leaf], ALIAS, &http_selector("/v1"));
    assert_eq!(answer, Err(ResolveError::UnavailableChain));
}

#[test]
fn the_resolution_carries_the_per_family_sharing_modes_and_ownership() {
    let (management, store) = service_with_store();
    let (leaf, root) = chain();
    let mut value = stored(ALIAS);
    value.auth = Some(AuthConfig {
        r#type: Some(String::from(ROOT_AUTH)),
        sharing: Some(SharingMode::Enforce),
        config: None,
    });
    value.plugins = Some(shared_plugins(SharingMode::Inherit, &[PLUGIN]));
    value.cors = Some(CorsConfig {
        sharing: Some(SharingMode::Inherit),
        enabled: true,
        allowed_origins: vec![String::from("https://app.example.com")],
        allowed_methods: vec![String::from("GET")],
        expose_headers: Vec::new(),
        allow_credentials: false,
    });
    value.tags = vec![String::from("root")];
    insert(&store, root, &value);
    create(
        &management,
        leaf,
        &with_families(base(ALIAS), json!({ "tags": ["leaf"] })),
    );

    let upstream = resolve_upstream(&store, leaf, &[root], ALIAS).expect("a resolution");
    assert_eq!(
        upstream.auth.expect("auth is forced").mode,
        SharingMode::Enforce
    );
    assert_eq!(
        upstream.plugins.expect("plugins are inherited").mode,
        SharingMode::Inherit
    );
    assert_eq!(
        upstream.cors.expect("cors is inherited").mode,
        SharingMode::Inherit
    );
    assert_eq!(
        upstream.tags.contributors,
        vec![root, leaf],
        "every contributor of the tag union is named"
    );
    assert_eq!(upstream.tenant_id, leaf);
}
