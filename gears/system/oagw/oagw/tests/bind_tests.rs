//! The bind-style create and the inherited-field override.
//!
//! Covers `cpt-cf-oagw-dod-binding-style-creation` and the write half of
//! `cpt-cf-oagw-dod-sharing-mode-decision` and
//! `cpt-cf-oagw-dod-descendant-override-permissions`: a create whose alias
//! matches an ancestor binds instead of conflicting, the four permissions gate
//! the four families on both rows, an `enforce` family answers 400 and a
//! missing permission answers 403 in that order, the ancestor's rows — tag
//! rows included — are byte-identical after every operation, and no refused
//! answer carries an ancestor value.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::{ManagementService, ServiceError};
use oagw::control_plane::sharing::OverridePermissions;
use oagw::domain::error::ErrorKind;
use oagw::domain::route::{HttpMatch, MatchConfig, Route};
use oagw::domain::upstream::{
    AuthConfig, Burst, PluginsConfig, RateLimitConfig, ServerConfig, SharingMode, Sustained,
    Upstream,
};
use oagw::store::OagwStore;
use oagw::{CorsConfig, Endpoint, EndpointHost, OagwConfig, Scheme, UpstreamRow};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// The alias the ancestor holds and the descendant binds against.
const ALIAS: &str = "api.openai.com";

const BIND: &str = "oagw:upstream:bind";
const OVERRIDE_AUTH: &str = "oagw:upstream:override_auth";
const OVERRIDE_RATE: &str = "oagw:upstream:override_rate";
const ADD_PLUGINS: &str = "oagw:upstream:add_plugins";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// The calling tenant and its single ancestor, in chain order.
fn chain() -> (Uuid, Uuid) {
    (tenant(0xb001), tenant(0xb002))
}

/// The chain the tests pass the service, calling tenant first.
fn ancestors() -> Vec<Uuid> {
    let (_, ancestor) = chain();
    vec![ancestor]
}

/// A chain the resolver could not have produced: the calling tenant appears
/// twice, which the walk refuses to order.
fn cyclic_ancestors() -> Vec<Uuid> {
    let (calling, ancestor) = chain();
    vec![ancestor, calling, calling]
}

fn context(scopes: &[&str]) -> SecurityContext {
    let (calling, _) = chain();
    SecurityContext::builder()
        .subject_id(tenant(0xc001))
        .subject_tenant_id(calling)
        .token_scopes(scopes.iter().map(|scope| String::from(*scope)).collect())
        .build()
        .expect("the context builds")
}

fn permissions(scopes: &[&str]) -> OverridePermissions {
    OverridePermissions::of(&context(scopes))
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

fn auth_body(sharing: &str, key: &str) -> Value {
    // The auth identifier is one the built-in registry backs: the plugin
    // feature resolves it before the sharing-mode decision runs, and an
    // identifier it does not back is answered 400 ahead of that decision.
    json!({
        "sharing": sharing,
        "type": oagw::gts::plugin_catalog::AUTH_APIKEY,
        "config": { "key": key }
    })
}

fn rate_limit_body(sharing: &str, rate: u64) -> Value {
    json!({
        "sharing": sharing,
        "algorithm": "token_bucket",
        "sustained": { "rate": rate, "window": "minute" },
        "burst": { "capacity": rate },
        "scope": "tenant",
        "strategy": "reject",
        "cost": 1
    })
}

fn cors_body(sharing: &str, origin: &str) -> Value {
    json!({
        "sharing": sharing,
        "enabled": true,
        "allowed_origins": [origin],
        "allowed_methods": ["GET"],
        "expose_headers": [],
        "allow_credentials": false
    })
}

/// Builds the upstream value one store-level row holds, with no family set.
fn stored(alias: &str) -> Upstream {
    let mut upstream = Upstream::new(
        Uuid::new_v4(),
        ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: EndpointHost::parse(alias).expect("the host parses"),
                port: Some(443),
            }],
        },
        String::from(HTTP_PROTOCOL),
    );
    upstream.alias = Some(String::from(alias));
    upstream
}

fn with_auth(upstream: Upstream, sharing: SharingMode, key: &str) -> Upstream {
    let mut upstream = upstream;
    upstream.auth = Some(AuthConfig {
        r#type: Some(String::from("gts.cf.core.oagw.auth_plugin.v1~x.v1")),
        sharing: Some(sharing),
        config: Some(json_value(key)),
    });
    upstream
}

fn with_rate_limit(upstream: Upstream, sharing: SharingMode, rate: u64) -> Upstream {
    let mut upstream = upstream;
    upstream.rate_limit = Some(RateLimitConfig {
        sharing: Some(sharing),
        algorithm: None,
        sustained: Some(Sustained {
            rate,
            window: None,
        }),
        burst: Some(Burst { capacity: rate }),
        scope: None,
        strategy: None,
        cost: None,
    });
    upstream
}

fn with_cors(upstream: Upstream, sharing: SharingMode, origin: &str) -> Upstream {
    let mut upstream = upstream;
    upstream.cors = Some(CorsConfig {
        sharing: Some(sharing),
        enabled: true,
        allowed_origins: vec![String::from(origin)],
        allowed_methods: vec![String::from("GET")],
        expose_headers: Vec::new(),
        allow_credentials: false,
    });
    upstream
}

/// The open `config` object the auth family carries.
fn json_value(key: &str) -> Value {
    json!({ "key": key })
}

/// Inserts the ancestor's row and answers its identifier.
fn insert_ancestor(store: &OagwStore, upstream: &Upstream) -> Uuid {
    let (_, ancestor) = chain();
    store
        .insert_upstream(ancestor, upstream)
        .expect("the ancestor row is inserted")
        .upstream
        .id
}

/// Creates the descendant's row through the service and answers it.
#[allow(clippy::result_large_err)]
fn bind(
    service: &ManagementService,
    scopes: &[&str],
    families: Value,
) -> Result<UpstreamRow, ServiceError> {
    let (calling, _) = chain();
    let body = with_families(base(ALIAS), families);
    service.create_upstream_in_chain(
        calling,
        &ancestors(),
        &permissions(scopes),
        &body,
    )
}

#[test]
fn a_create_whose_alias_matches_an_ancestor_binds_instead_of_conflicting() {
    let (service, store) = service_with_store();
    let ancestor_id = insert_ancestor(&store, &stored(ALIAS));

    let written =
        bind(&service, &[BIND], json!({})).expect("a bind is answered 201, not 409");
    let (calling, _) = chain();
    assert_eq!(written.tenant_id, calling, "the row is the descendant's own");
    assert_ne!(written.upstream.id, ancestor_id, "a new row is written");
    assert_eq!(written.upstream.alias.as_deref(), Some(ALIAS));
}

#[test]
fn a_create_whose_alias_matches_no_ancestor_is_ordinary() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &stored("other.example.com"));

    let written = service
        .create_upstream_in_chain(
            chain().0,
            &ancestors(),
            &OverridePermissions::none(),
            &base(ALIAS),
        )
        .expect("no bind is performed and no bind permission is consumed");
    assert_eq!(written.upstream.alias.as_deref(), Some(ALIAS));
}

#[test]
fn a_bind_without_the_bind_permission_is_refused_403() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &stored(ALIAS));

    let refusal = bind(&service, &[], json!({})).expect_err("the bind is refused");
    assert_eq!(
        refusal.permission(),
        Some(BIND),
        "the refusal names the permission the token lacks"
    );
    let (calling, _) = chain();
    assert!(
        store.list_upstreams(calling).is_empty(),
        "a refused bind writes no row"
    );
}

#[test]
fn a_bind_carrying_an_enforced_family_is_refused_400() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_auth(stored(ALIAS), SharingMode::Enforce, "root"));

    let refusal = bind(
        &service,
        &[BIND],
        json!({ "auth": auth_body("private", "leaf") }),
    )
    .expect_err("the enforced family is refused");
    let ServiceError::Domain(error) = &refusal else {
        panic!("an enforced family answers 400, not {refusal:?}");
    };
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(
        error.detail.contains("auth"),
        "the 400 names the family: {}",
        error.detail
    );
}

#[test]
fn a_bind_with_no_value_for_an_enforced_family_succeeds() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_auth(stored(ALIAS), SharingMode::Enforce, "root"));

    let written = bind(&service, &[BIND], json!({})).expect("nothing is overridden");
    assert!(written.upstream.auth.is_none(), "the body carried no auth");
}

#[test]
fn a_bind_with_a_private_ancestor_family_writes_the_descendants_own_value() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_auth(stored(ALIAS), SharingMode::Private, "root"));

    let written = bind(
        &service,
        &[BIND],
        json!({ "auth": auth_body("private", "leaf") }),
    )
    .expect("a private family blocks no bind and consumes no permission");
    assert!(written.upstream.auth.is_some(), "the body's value reaches the row");
}

#[test]
fn a_bind_stores_the_request_tags_on_the_descendants_row_only() {
    let (service, store) = service_with_store();
    let ancestor_id = insert_ancestor(&store, &stored(ALIAS));

    let written = bind(&service, &[BIND], json!({ "tags": ["edge"] }))
        .expect("the bind is written");
    let (calling, ancestor) = chain();
    assert_eq!(written.tags, vec![String::from("edge")]);
    assert_eq!(
        store.upstream_tag_rows(calling, written.upstream.id),
        vec![String::from("edge")],
        "the request tags are tenant-local additions on the descendant's row"
    );
    assert!(
        store.upstream_tag_rows(ancestor, ancestor_id).is_empty(),
        "no tag reaches the ancestor's tag rows"
    );
}

#[test]
fn a_bind_leaves_every_ancestor_row_byte_identical() {
    let (service, store) = service_with_store();
    let ancestor_id = insert_ancestor(&store, &with_auth(stored(ALIAS), SharingMode::Inherit, "root"));
    let (_, ancestor) = chain();
    let before = store
        .get_upstream(ancestor, ancestor_id)
        .expect("the ancestor row exists");

    let _ = bind(
        &service,
        &[BIND, OVERRIDE_AUTH],
        json!({ "auth": auth_body("private", "leaf"), "tags": ["edge"] }),
    )
    .expect("the bind is written");

    let after = store
        .get_upstream(ancestor, ancestor_id)
        .expect("the ancestor row still exists");
    assert_eq!(&before, &after, "the ancestor row is byte-identical");
    assert_eq!(
        store.upstream_tag_rows(ancestor, ancestor_id),
        before.tags,
        "the ancestor's tag rows are byte-identical"
    );
}

#[test]
fn a_refused_answer_carries_no_ancestor_value() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_auth(stored(ALIAS), SharingMode::Enforce, "root-secret"));

    // The 400 names the family and nothing of the ancestor's configuration.
    let refusal = bind(
        &service,
        &[BIND],
        json!({ "auth": auth_body("private", "leaf") }),
    )
    .expect_err("the enforced family is refused");
    let ServiceError::Domain(error) = &refusal else {
        panic!("an enforced family answers 400, not {refusal:?}");
    };
    assert!(
        !error.detail.contains("root-secret"),
        "no ancestor value is disclosed: {}",
        error.detail
    );
    assert!(
        !error.detail.contains(ALIAS),
        "the ancestor's alias is not disclosed: {}",
        error.detail
    );

    // The 403 carries no detail at all beyond the permission it names.
    let refusal = bind(&service, &[], json!({})).expect_err("the bind is refused");
    assert_eq!(refusal.permission(), Some(BIND));
    assert!(
        matches!(refusal, ServiceError::Forbidden { .. }),
        "a 403 is not a catalogue answer"
    );
}

#[test]
fn a_same_tenant_duplicate_is_answered_409_before_the_walk() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &stored(ALIAS));

    // The calling tenant already holds the alias: the conflict is answered
    // before the walk runs, so a missing bind permission never surfaces.
    let (calling, _) = chain();
    service
        .create_upstream(calling, &base(ALIAS))
        .expect("the first create is ordinary");
    let refusal = service
        .create_upstream_in_chain(
            calling,
            &ancestors(),
            &OverridePermissions::none(),
            &base(ALIAS),
        )
        .expect_err("the duplicate is refused");
    let ServiceError::Domain(error) = &refusal else {
        panic!("a same-tenant duplicate answers 409, not {refusal:?}");
    };
    assert_eq!(error.kind, ErrorKind::AliasConflict);
}

#[test]
fn an_unordered_chain_fails_closed_without_writing() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &stored(ALIAS));

    let (calling, _) = chain();
    let refusal = service
        .create_upstream_in_chain(
            calling,
            &cyclic_ancestors(),
            &permissions(&[BIND]),
            &base(ALIAS),
        )
        .expect_err("an unordered chain cannot be resolved");
    assert!(
        refusal.is_storage(),
        "an unordered chain fails closed with the platform 500 shape"
    );
    assert!(store.list_upstreams(calling).is_empty(), "nothing is written");
}

#[test]
fn an_inherited_rate_limit_overrides_with_the_permission_held() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_rate_limit(stored(ALIAS), SharingMode::Inherit, 100));

    let (calling, _) = chain();
    let written = service
        .replace_upstream_in_chain(
            calling,
            created(&store),
            &ancestors(),
            &permissions(&[OVERRIDE_RATE]),
            &with_families(base(ALIAS), json!({ "rate_limit": rate_limit_body("private", 50) })),
        )
        .expect("the override is permitted");
    assert_eq!(
        written.upstream.rate_limit.as_ref().and_then(|limit| limit.sustained.as_ref().map(|rate| rate.rate)),
        Some(50),
        "the body's value reaches the row"
    );
}

#[test]
fn an_inherited_rate_limit_without_the_permission_is_refused_403() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_rate_limit(stored(ALIAS), SharingMode::Inherit, 100));

    let (calling, _) = chain();
    let refusal = service
        .replace_upstream_in_chain(
            calling,
            created(&store),
            &ancestors(),
            &OverridePermissions::none(),
            &with_families(base(ALIAS), json!({ "rate_limit": rate_limit_body("private", 50) })),
        )
        .expect_err("the override is refused");
    assert_eq!(refusal.permission(), Some(OVERRIDE_RATE));
}

#[test]
fn an_enforced_family_in_a_replacement_is_refused_400() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_cors(stored(ALIAS), SharingMode::Enforce, "https://root.example.com"));

    let (calling, _) = chain();
    let refusal = service
        .replace_upstream_in_chain(
            calling,
            created(&store),
            &ancestors(),
            &permissions(&[OVERRIDE_RATE]),
            &with_families(base(ALIAS), json!({ "cors": cors_body("private", "https://leaf.example.com") })),
        )
        .expect_err("the enforced family is refused");
    let ServiceError::Domain(error) = &refusal else {
        panic!("an enforced family answers 400, not {refusal:?}");
    };
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(error.detail.contains("cors"), "{}", error.detail);
    assert!(
        !error.detail.contains("root.example.com"),
        "no ancestor value is disclosed: {}",
        error.detail
    );
}

#[test]
fn a_replacement_that_omits_the_enforced_family_succeeds() {
    let (service, store) = service_with_store();
    insert_ancestor(&store, &with_cors(stored(ALIAS), SharingMode::Enforce, "https://root.example.com"));

    let (calling, _) = chain();
    let written = service
        .replace_upstream_in_chain(
            calling,
            created(&store),
            &ancestors(),
            &permissions(&[]),
            &base(ALIAS),
        )
        .expect("nothing is overridden");
    assert!(written.upstream.cors.is_none());
}

#[test]
fn the_permission_403_precedes_any_enforce_400_on_a_replacement() {
    let (_, ancestor) = chain();
    let (service, store) = service_with_store();
    // The nearer ancestor enforces the CORS family and the more distant one
    // inherits the rate limit; the alias the descendant's row carries is the
    // one both hold.
    insert_ancestor(&store, &with_cors(stored(ALIAS), SharingMode::Enforce, "https://root.example.com"));
    store
        .insert_upstream(
            tenant(0xb003),
            &with_rate_limit(stored(ALIAS), SharingMode::Inherit, 100),
        )
        .expect("the second ancestor row is inserted");

    // The token holds no permission at all, so both families are blocked and
    // the permission refusal is the one returned.
    let (calling, _) = chain();
    let refusal = service
        .replace_upstream_in_chain(
            calling,
            created(&store),
            &[ancestor, tenant(0xb003)],
            &OverridePermissions::none(),
            &with_families(
                base(ALIAS),
                json!({
                    "cors": cors_body("private", "https://leaf.example.com"),
                    "rate_limit": rate_limit_body("private", 50)
                }),
            ),
        )
        .expect_err("both families are blocked");
    assert_eq!(refusal.permission(), Some(OVERRIDE_RATE));
}

#[test]
fn a_route_replacement_gates_the_same_families() {
    let (service, store) = service_with_store();
    let ancestor_id = insert_ancestor(&store, &stored(ALIAS));
    let (_, ancestor) = chain();
    store
        .insert_route(
            ancestor,
            &Route {
                id: Uuid::new_v4(),
                upstream_id: ancestor_id,
                match_config: match_of("/v1/chat"),
                plugins: None,
                rate_limit: Some(RateLimitConfig {
                    sharing: Some(SharingMode::Inherit),
                    algorithm: None,
                    sustained: Some(Sustained {
                        rate: 100,
                        window: None,
                    }),
                    burst: Some(Burst { capacity: 100 }),
                    scope: None,
                    strategy: None,
                    cost: None,
                }),
                tags: Vec::new(),
                cors: None,
                priority: Some(1),
                enabled: Some(true),
            },
        )
        .expect("the ancestor route is inserted");

    // The descendant binds against the alias and adds a route of its own.
    let bound = bind(&service, &[BIND], json!({})).expect("the bind is written");
    let (calling, _) = chain();
    let route = service
        .create_route(
            calling,
            &json!({
                "upstream_id": bound.upstream.id.to_string(),
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1,
                "tags": []
            }),
        )
        .expect("the route is created");

    let refusal = service
        .replace_route_in_chain(
            calling,
            route.route.id,
            &ancestors(),
            &OverridePermissions::none(),
            &json!({
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "rate_limit": rate_limit_body("private", 50),
                "priority": 1,
                "tags": []
            }),
        )
        .expect_err("the override is refused");
    let ServiceError::Forbidden { permission, .. } = &refusal else {
        panic!("the override is refused 403, not {refusal:?}");
    };
    assert_eq!(
        *permission,
        Some(OVERRIDE_RATE),
        "the same four permissions gate the same families on a route row"
    );
}

#[test]
fn a_route_replacement_gates_the_plugin_family() {
    let (service, store) = service_with_store();
    let ancestor_id = insert_ancestor(&store, &stored(ALIAS));
    let (_, ancestor) = chain();
    store
        .insert_route(
            ancestor,
            &Route {
                id: Uuid::new_v4(),
                upstream_id: ancestor_id,
                match_config: match_of("/v1/chat"),
                plugins: Some(PluginsConfig {
                    sharing: Some(SharingMode::Inherit),
                    items: Vec::new(),
                }),
                rate_limit: None,
                tags: Vec::new(),
                cors: None,
                priority: Some(1),
                enabled: Some(true),
            },
        )
        .expect("the ancestor route is inserted");

    let bound = bind(&service, &[BIND], json!({})).expect("the bind is written");
    let (calling, _) = chain();
    let route = service
        .create_route(
            calling,
            &json!({
                "upstream_id": bound.upstream.id.to_string(),
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1,
                "tags": []
            }),
        )
        .expect("the route is created");

    // The body carries the plugin family, so the ancestor's `inherit` makes the
    // replacement a plugin override, which the plugin permission gates.
    let refusal = service
        .replace_route_in_chain(
            calling,
            route.route.id,
            &ancestors(),
            &OverridePermissions::none(),
            &json!({
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "plugins": { "sharing": "private" },
                "priority": 1,
                "tags": []
            }),
        )
        .expect_err("the plugin override is refused");
    let ServiceError::Forbidden { permission, .. } = &refusal else {
        panic!("the plugin override is refused 403, not {refusal:?}");
    };
    assert_eq!(*permission, Some(ADD_PLUGINS));

    // Omitting the family is no override at all, and needs no permission.
    let kept = service
        .replace_route_in_chain(
            calling,
            route.route.id,
            &ancestors(),
            &OverridePermissions::none(),
            &json!({
                "match": { "http": { "methods": ["GET"], "path": "/v1/chat" } },
                "priority": 1,
                "tags": []
            }),
        )
        .expect("the family-free replacement is written");
    assert_eq!(kept.route.id, route.route.id);
}

#[test]
fn a_refused_replacement_leaves_the_ancestor_row_byte_identical() {
    let (service, store) = service_with_store();
    let ancestor_id =
        insert_ancestor(&store, &with_rate_limit(stored(ALIAS), SharingMode::Inherit, 100));
    let (_, ancestor) = chain();
    let before = store
        .get_upstream(ancestor, ancestor_id)
        .expect("the ancestor row exists");

    let (calling, _) = chain();
    let _ = service.replace_upstream_in_chain(
        calling,
        created(&store),
        &ancestors(),
        &OverridePermissions::none(),
        &with_families(base(ALIAS), json!({ "rate_limit": rate_limit_body("private", 50) })),
    );

    let after = store
        .get_upstream(ancestor, ancestor_id)
        .expect("the ancestor row still exists");
    assert_eq!(&before, &after, "no ancestor row is written");
}

/// Creates the descendant's own upstream through the service and answers its
/// identifier.
fn created(store: &Arc<OagwStore>) -> Uuid {
    let (calling, _) = chain();
    // The row is placed at store level so the replacement tests can address it
    // without depending on how a create's bind decision answers this body.
    let row = store
        .insert_upstream(calling, &stored(ALIAS))
        .expect("the descendant row is inserted");
    row.upstream.id
}

/// The HTTP match one route body states.
fn match_of(path: &str) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: vec![String::from("GET")],
            path: String::from(path),
            query_allowlist: Vec::new(),
            path_suffix_mode: None,
        }),
        grpc: None,
    }
}
