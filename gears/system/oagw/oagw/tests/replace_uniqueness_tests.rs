//! Full-replacement diff and match-uniqueness tests.
//!
//! Covers `cpt-cf-oagw-algo-put-replace-diff` and
//! `cpt-cf-oagw-algo-match-uniqueness`: the immutable fields taken from the
//! addressed row, the route's upstream reference taken from the stored row and
//! a body supplying one refused, the optional families cleared on a
//! replacement, the tags replaced in full, `enabled` carried forward when
//! omitted and set when explicit, match uniqueness re-checked excluding the
//! replaced row, and the match keys one route expands into.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use oagw::control_plane::match_uniqueness::{confirm_route, match_keys};
use oagw::control_plane::replace::{route_diff, upstream_diff};
use oagw::domain::alias::Alias;
use oagw::domain::error::ErrorKind;
use oagw::store::{OagwStore, RouteRow};
use oagw::{
    Endpoint, EndpointHost, GrpcMatch, HttpMatch, MatchConfig, Route, Scheme, ServerConfig, Upstream,
};
use uuid::Uuid;

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

fn tenant(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: EndpointHost::parse(host).expect("valid endpoint host"),
        port: Some(port),
    }
}

/// An upstream holding one endpoint set and the alias it derives.
fn upstream(endpoints: &[Endpoint], alias: Option<&str>, tags: &[&str]) -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        enabled: true,
        alias: alias.map(str::to_owned),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        server: ServerConfig {
            endpoints: endpoints.to_vec(),
        },
        protocol: String::from(HTTP_PROTOCOL),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    }
}

fn single(host: &str) -> Endpoint {
    endpoint(host, 443)
}

/// A route with one `http` match.
fn route(upstream_id: Uuid, path: &str, priority: i64, enabled: Option<bool>) -> Route {
    Route {
        id: Uuid::new_v4(),
        upstream_id,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![String::from("GET")],
                path: String::from(path),
                query_allowlist: vec![],
                path_suffix_mode: None,
            }),
            grpc: None,
        },
        plugins: None,
        rate_limit: None,
        tags: vec![String::from("edge")],
        cors: None,
        priority: Some(priority),
        enabled,
    }
}

/// A store holding one upstream and one enabled route under it.
fn seeded() -> (OagwStore, RouteRow) {
    let store = OagwStore::new();
    let owner = tenant(1);
    let stored = store
        .insert_upstream(owner, &upstream(&[single("api.openai.com")], None, &["llm"]))
        .expect("the upstream");
    let row = store
        .insert_route(owner, &route(stored.upstream.id, "/v1/chat", 10, Some(true)))
        .expect("the route");
    (store, row)
}

#[test]
fn the_immutable_identifier_comes_from_the_addressed_row() {
    let (store, stored) = seeded();
    let replacement = stored.route.clone();
    let diff = route_diff(&store, &stored, None, replacement).expect("the write set");
    assert_eq!(diff.id, stored.route.id);
    assert_eq!(diff.tenant_id, stored.tenant_id);
}

#[test]
fn a_body_stating_a_different_identifier_is_refused() {
    let (store, stored) = seeded();
    let replacement = stored.route.clone();
    let error = route_diff(&store, &stored, Some(Uuid::new_v4()), replacement)
        .expect_err("the stated identifier differs");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(error.detail.contains("id"), "{error}");
}

#[test]
fn the_tenant_is_never_taken_from_the_body() {
    let (store, stored) = seeded();
    let replacement = stored.route.clone();
    let diff = route_diff(&store, &stored, None, replacement).expect("the write set");
    assert_eq!(diff.tenant_id, tenant(1), "the tenant comes from the row");
}

#[test]
fn the_upstream_reference_comes_from_the_stored_row() {
    let (store, stored) = seeded();
    let mut replacement = stored.route.clone();
    replacement.upstream_id = Uuid::new_v4();

    let error = route_diff(&store, &stored, None, replacement)
        .expect_err("the body names another upstream");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(error.detail.contains("upstream_id"), "{error}");

    let mut conformance = stored.route.clone();
    conformance.upstream_id = Uuid::nil();
    let diff = route_diff(&store, &stored, None, conformance).expect("the write set");
    assert_eq!(diff.value.upstream_id, stored.route.upstream_id);
}

#[test]
fn the_optional_families_a_body_omits_are_cleared() {
    let (store, stored) = seeded();
    let mut replacement = stored.route.clone();
    replacement.rate_limit = None;
    replacement.plugins = None;
    let before = replacement.clone();

    let diff = route_diff(&store, &stored, None, replacement).expect("the write set");
    assert_eq!(diff.value.rate_limit, None);
    assert_eq!(diff.value.plugins, None);
    assert_eq!(diff.value.tags, before.tags);
}

#[test]
fn tags_are_replaced_in_full() {
    let store = OagwStore::new();
    let owner = tenant(1);
    let stored_upstream = store
        .insert_upstream(owner, &upstream(&[single("api.openai.com")], None, &["a", "b", "c"]))
        .expect("the upstream");

    let mut replacement = stored_upstream.upstream.clone();
    replacement.tags = vec![String::from("a")];
    let diff = upstream_diff(&stored_upstream, None, replacement).expect("the write set");
    assert_eq!(diff.value.tags, vec![String::from("a")], "the body's set in full");

    let written = store
        .replace_upstream(owner, stored_upstream.upstream.id, &diff.value)
        .expect("the replacement");
    assert_eq!(written.tags, vec![String::from("a")], "the difference is removed");
}

#[test]
fn enabled_is_carried_forward_when_the_body_omits_it() {
    let (store, stored) = seeded();
    let disabled = route(stored.route.upstream_id, "/v1/chat", 10, Some(false));
    let diff = route_diff(&store, &stored, None, disabled).expect("the write set");
    assert_eq!(diff.value.enabled, Some(false), "an explicit value wins");

    let omitted = route(stored.route.upstream_id, "/v1/chat", 10, None);
    let diff = route_diff(&store, &stored, None, omitted).expect("the write set");
    assert_eq!(diff.value.enabled, Some(true), "the stored value carries forward");
}

#[test]
fn a_write_set_that_changes_nothing_reports_itself_empty() {
    let (store, stored) = seeded();
    let replacement = stored.route.clone();
    let diff = route_diff(&store, &stored, None, replacement).expect("the write set");
    assert!(!diff.changed, "nothing differs");

    let mut different = stored.route.clone();
    different.priority = Some(11);
    let diff = route_diff(&store, &stored, None, different).expect("the write set");
    assert!(diff.changed, "the priority differs");
}

#[test]
fn match_keys_expand_one_key_per_declared_method() {
    let owner = tenant(1);
    let mut http = route(owner, "/v1/chat", 10, Some(true));
    if let Some(matched) = &mut http.match_config.http {
        matched.methods = vec![
            String::from("GET"),
            String::from("POST"),
            String::from("DELETE"),
        ];
    }
    assert_eq!(match_keys(&http).len(), 3, "three methods, three keys");
    for key in match_keys(&http) {
        assert_eq!(key.upstream_id, owner);
        assert_eq!(key.path, "/v1/chat");
        assert_eq!(key.priority, 10);
    }

    let grpc = Route {
        upstream_id: owner,
        match_config: MatchConfig {
            http: None,
            grpc: Some(GrpcMatch {
                service: String::from("foo.v1.UserService"),
                method: String::from("GetUser"),
            }),
        },
        ..route(owner, "/v1", 1, Some(true))
    };
    let keys = match_keys(&grpc);
    assert_eq!(keys.len(), 1, "one grpc method, one key");
    assert_eq!(keys[0].method, "GetUser");
}

#[test]
fn a_route_of_another_upstream_never_collides() {
    let (store, stored) = seeded();
    let _ = &stored;
    let other = store
        .insert_upstream(tenant(1), &upstream(&[single("eu.openai.com")], None, &[]))
        .expect("the second upstream");
    let foreign_upstream = route(other.upstream.id, "/v1/chat", 10, Some(true));
    confirm_route(&store, tenant(1), &foreign_upstream, None).expect("a different upstream");
}

#[test]
fn the_replaced_row_is_excluded_from_its_own_key() {
    let (store, stored) = seeded();
    let mut replacement = stored.route.clone();
    replacement.match_config.http.as_mut().expect("http").path = String::from("/v1/chat");
    confirm_route(&store, tenant(1), &replacement, Some(stored.route.id))
        .expect("the replaced row is excluded");
}

#[test]
fn a_colliding_route_is_named() {
    let (store, stored) = seeded();
    let colliding = route(stored.route.upstream_id, "/v1/chat", 10, Some(true));
    let error = confirm_route(&store, tenant(1), &colliding, None)
        .expect_err("the key is already held");
    assert_eq!(error.kind, ErrorKind::MatchConflict);
    assert_eq!(error.http_status(), 409);
    assert!(
        error.detail.contains(&stored.route.id.to_string()),
        "the colliding route is named: {error}"
    );
}

#[test]
fn a_disabled_route_never_collides() {
    let (store, stored) = seeded();
    let disabled = route(stored.route.upstream_id, "/v1/chat", 10, Some(false));
    confirm_route(&store, tenant(1), &disabled, None).expect("a disabled route holds no key");

    let store = OagwStore::new();
    let owner = tenant(1);
    let upstream_row = store
        .insert_upstream(owner, &upstream(&[single("api.openai.com")], None, &[]))
        .expect("the upstream");
    store
        .insert_route(owner, &route(upstream_row.upstream.id, "/v1/chat", 10, Some(false)))
        .expect("the first disabled route");
    store
        .insert_route(owner, &route(upstream_row.upstream.id, "/v1/chat", 10, Some(false)))
        .expect("two disabled routes with identical keys are stored");
}

#[test]
fn a_differing_method_or_priority_does_not_collide() {
    let (store, stored) = seeded();
    let _ = &stored;
    let other_method = route(stored.route.upstream_id, "/v1/chat", 10, Some(true));
    let mut method = other_method;
    method.match_config.http.as_mut().expect("http").methods = vec![String::from("POST")];
    confirm_route(&store, tenant(1), &method, None).expect("a different method");

    let other_priority = route(stored.route.upstream_id, "/v1/chat", 11, Some(true));
    confirm_route(&store, tenant(1), &other_priority, None).expect("a different priority");
}

#[test]
fn a_route_replacement_reruns_uniqueness() {
    let (store, stored) = seeded();
    let second = store
        .insert_route(
            tenant(1),
            &route(stored.route.upstream_id, "/v1/embed", 20, Some(true)),
        )
        .expect("the second route");

    let mut replacement = stored.route.clone();
    replacement.match_config.http.as_mut().expect("http").path = String::from("/v1/embed");
    replacement.priority = Some(20);
    let error = route_diff(&store, &stored, None, replacement)
        .expect_err("the replacement collides with the second route");
    assert_eq!(error.kind, ErrorKind::MatchConflict);
    assert!(error.detail.contains(&second.route.id.to_string()), "{error}");
}

#[test]
fn an_upstream_replacement_recomputes_the_derived_alias() {
    let store = OagwStore::new();
    let owner = tenant(1);
    let stored = store
        .insert_upstream(
            owner,
            &upstream(&[single("api.openai.com")], Some("api.openai.com"), &[]),
        )
        .expect("the upstream");

    let mut replacement = stored.upstream.clone();
    replacement.alias = None;
    replacement.server = ServerConfig {
        endpoints: vec![endpoint("api.openai.com", 8443)],
    };
    let error = upstream_diff(&stored, None, replacement)
        .expect_err("the replacement endpoints derive another alias");
    assert_eq!(error.kind, ErrorKind::AliasConflict);
    assert_eq!(error.http_status(), 409);

    let mut pooled = stored.upstream.clone();
    pooled.alias = None;
    pooled.server = ServerConfig {
        endpoints: vec![single("us.vendor.com"), single("eu.vendor.com")],
    };
    let error = upstream_diff(&stored, None, pooled)
        .expect_err("the pooled endpoints derive another alias");
    assert_eq!(error.kind, ErrorKind::AliasConflict);
    assert_eq!(error.http_status(), 409);
}

#[test]
fn a_replacement_whose_endpoints_derive_the_stored_alias_is_accepted() {
    let store = OagwStore::new();
    let owner = tenant(1);
    let stored = store
        .insert_upstream(
            owner,
            &upstream(&[single("api.openai.com")], Some("api.openai.com"), &[]),
        )
        .expect("the upstream");

    let mut replacement = stored.upstream.clone();
    replacement.alias = None;
    replacement.rate_limit = None;
    let diff = upstream_diff(&stored, None, replacement).expect("the alias still derives");
    let alias = Alias::parse(diff.value.alias.as_deref().expect("an alias"))
        .expect("the stored alias parses");
    assert_eq!(alias.to_string(), "api.openai.com");
}

#[test]
fn a_replacement_adding_a_pooled_endpoint_that_keeps_the_alias_derives_it() {
    let store = OagwStore::new();
    let owner = tenant(1);
    let stored = store
        .insert_upstream(
            owner,
            &upstream(
                &[single("us.vendor.com"), single("eu.vendor.com")],
                Some("vendor.com"),
                &[],
            ),
        )
        .expect("the upstream");
    assert_eq!(stored.upstream.alias.as_deref(), Some("vendor.com"));

    let mut replacement = stored.upstream.clone();
    replacement.alias = None;
    replacement
        .server
        .endpoints
        .push(single("ap.vendor.com"));
    let diff = upstream_diff(&stored, None, replacement).expect("the alias is unchanged");
    assert_eq!(diff.value.alias.as_deref(), Some("vendor.com"));
    assert!(diff.changed, "the endpoint pool differs");
    let written = store
        .replace_upstream(owner, stored.upstream.id, &diff.value)
        .expect("the replacement is accepted");
    assert_eq!(written.upstream.alias.as_deref(), Some("vendor.com"));
}
