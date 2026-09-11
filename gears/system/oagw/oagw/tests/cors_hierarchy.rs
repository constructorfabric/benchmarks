//! Integration tests of the hierarchical CORS origin resolution
//! (`cpt-cf-oagw-flow-cors-hierarchical-origins`,
//! `cpt-cf-oagw-algo-cors-origin-set-merge`).
//!
//! The tests address an alias the calling tenant does not own, so the merge
//! engine receives both the leaf layer and the ancestor layer, exactly as the
//! canonical order resolves it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use uuid::Uuid;

use oagw::test_support::{
    route_for, seed_route, seed_upstream, stub_upstream, upstream_at, FakeHierarchyTenantResolver,
};

const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
const OVERRIDE_PERMISSION: &str = "gts.cf.core.oagw.upstream.v1~:override";
const ORIGIN_NOT_ALLOWED: &str = "gts://gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";

fn proxy_config() -> Option<Value> {
    Some(json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

fn cors(sharing: &str, origins: &[&str], methods: &[&str]) -> Value {
    json!({
        "sharing": sharing,
        "enabled": true,
        "allowed_origins": origins,
        "allowed_methods": methods,
    })
}

/// The surface over the `leaf -> root` chain, with both tenants holding an
/// upstream at the same alias over the stub: the leaf's is the routing target
/// and the root's is the shadowed ancestor layer.
async fn seeded(
    root_cors: Value,
    leaf_cors: Value,
) -> (
    oagw::test_support::ManagementSurface,
    oagw::test_support::StubUpstream,
    Uuid,
    Uuid,
) {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default()),
        FakeHierarchyTenantResolver::over(&[leaf, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let mut leaf_upstream =
        upstream_at(leaf, "shared.vendor.com", oagw::domain::dto::EndpointScheme::Http, &host, port);
    leaf_upstream.cors = serde_json::from_value(leaf_cors).unwrap_or(None);
    let leaf_id = seed_upstream(&surface, leaf_upstream);
    seed_route(&surface, route_for(leaf, leaf_id, "/v1", &[oagw::domain::dto::HttpMethod::Get]));

    let mut root_upstream =
        upstream_at(root, "shared.vendor.com", oagw::domain::dto::EndpointScheme::Http, &host, port);
    root_upstream.cors = serde_json::from_value(root_cors).unwrap_or(None);
    let root_id = seed_upstream(&surface, root_upstream);
    seed_route(&surface, route_for(root, root_id, "/v1", &[oagw::domain::dto::HttpMethod::Get]));

    (surface, stub, leaf, root)
}

#[tokio::test]
async fn an_ancestor_origin_set_is_unioned_under_inherit() {
    let (surface, _stub, leaf, _root) = seeded(
        cors("inherit", &["https://root.dev"], &["GET"]),
        cors("private", &["https://leaf.dev"], &["GET"]),
    )
    .await;
    for (origin, expected) in
        [("https://leaf.dev", http::StatusCode::OK), ("https://root.dev", http::StatusCode::OK)]
    {
        let exchange = surface
            .proxy_for(
                leaf,
                Uuid::new_v4(),
                "GET",
                "/oagw/v1/proxy/shared.vendor.com/v1/orders",
                &[("origin", origin)],
                b"",
            )
            .await;
        assert_eq!(exchange.status, expected, "{origin}: {}", exchange.text());
    }
    // The ancestor's origin was never removed by the descendant's set.
    let exchange = surface
        .proxy_for(
            leaf,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/shared.vendor.com/v1/orders",
            &[("origin", "https://stranger.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_enforced_ancestor_origin_set_discards_the_descendant_addition() {
    let (surface, _stub, leaf, _root) = seeded(
        cors("enforce", &["https://root.dev"], &["GET"]),
        cors("private", &["https://leaf.dev"], &["GET"]),
    )
    .await;
    for (origin, expected) in [
        ("https://leaf.dev", http::StatusCode::FORBIDDEN),
        ("https://root.dev", http::StatusCode::OK),
    ] {
        let exchange = surface
            .proxy_for(
                leaf,
                Uuid::new_v4(),
                "GET",
                "/oagw/v1/proxy/shared.vendor.com/v1/orders",
                &[("origin", origin)],
                b"",
            )
            .await;
        assert_eq!(exchange.status, expected, "{origin}: {}", exchange.text());
    }
}

#[tokio::test]
async fn a_private_ancestor_cors_block_contributes_nothing() {
    let (surface, _stub, leaf, _root) = seeded(
        cors("private", &["https://root.dev"], &["GET"]),
        Value::Null,
    )
    .await;
    // The leaf holds no `cors` block of its own, so the ancestor's `private`
    // block is invisible to it and the request is forwarded with no CORS
    // header and no `403`.
    let exchange = surface
        .proxy_for(
            leaf,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/shared.vendor.com/v1/orders",
            &[("origin", "https://root.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());
    assert_eq!(exchange.header("vary"), Some("Origin"));
    assert!(exchange.headers.iter().all(|(name, _)| !name.starts_with("access-control")));
}

#[tokio::test]
async fn an_inherited_wildcard_cannot_be_turned_into_a_credential_bearing_policy() {
    let (surface, _stub, leaf, _root) = seeded(
        cors("inherit", &["*"], &["GET"]),
        json!({
            "sharing": "private",
            "enabled": true,
            "allowed_origins": ["https://leaf.dev"],
            "allowed_methods": ["GET"],
            "allow_credentials": true
        }),
    )
    .await;
    let exchange = surface
        .proxy_for(
            leaf,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/shared.vendor.com/v1/orders",
            &[("origin", "https://leaf.dev")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN, "{}", exchange.text());
    let body: Value = serde_json::from_slice(&exchange.body).expect("the problem+json body");
    assert_eq!(body["type"], ORIGIN_NOT_ALLOWED, "{body}");
    assert_eq!(exchange.header("vary"), Some("Origin"));
    // The stored records are unchanged: no merged configuration is persisted.
    let (status, listed) = surface
        .send(
            http::Method::GET,
            "/oagw/v1/upstreams",
            Some(oagw::test_support::security_context(leaf, Uuid::new_v4())),
            None,
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{listed:?}");
    let listed: Value = serde_json::from_slice(&listed).expect("the list body");
    let stored = &listed["items"].as_array().expect("items")[0];
    assert_eq!(stored["cors"]["allow_credentials"], json!(true), "{stored}");
    assert_eq!(stored["cors"]["allowed_origins"], json!(["https://leaf.dev"]), "{stored}");
}

/// An origin addition enters the effective set only through a write the
/// `:override` permission authorized: the unauthorized replacement is refused
/// at the write and so can never widen the union, and the merge itself performs
/// no per-request permission check.
#[tokio::test]
async fn an_unauthorized_origin_addition_never_reaches_the_effective_set() {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let authz = std::sync::Arc::new(oagw::test_support::FakePolicyAuthZ::default());
    let surface = oagw::test_support::management_surface(
        proxy_config(),
        authz.clone(),
        FakeHierarchyTenantResolver::over(&[leaf, root]),
    )
    .await;
    let stub = stub_upstream(Vec::new()).await;
    let (host, port) = stub.endpoint();
    let mut upstream =
        upstream_at(leaf, "shared.vendor.com", oagw::domain::dto::EndpointScheme::Http, &host, port);
    upstream.cors = Some(serde_json::from_value(cors("inherit", &["https://leaf.dev"], &["GET"])).unwrap());
    let id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(leaf, id, "/v1", &[oagw::domain::dto::HttpMethod::Get]));

    let body = |origins: &[&str]| {
        json!({
            "alias": "shared.vendor.com",
            "protocol": PROTOCOL_HTTP,
            "server": { "endpoints": [ { "host": host, "port": port, "scheme": "http" } ] },
            "cors": cors("inherit", origins, &["GET"])
        })
    };
    let path = "/oagw/v1/proxy/shared.vendor.com/v1/orders";

    // An authorized write unions its addition into the effective set.
    let (status, stored) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(oagw::test_support::security_context(leaf, Uuid::new_v4())),
            Some(body(&["https://leaf.dev", "https://admin.dev"])),
        )
        .await;
    assert_eq!(status, http::StatusCode::OK, "{}", String::from_utf8_lossy(&stored));
    let exchange = surface
        .proxy_for(leaf, Uuid::new_v4(), "GET", path, &[("origin", "https://admin.dev")], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK, "{}", exchange.text());

    // A write the override permission did not authorize is refused, so its
    // origin addition can never reach the merge.
    authz.deny(OVERRIDE_PERMISSION);
    let (status, body_bytes) = surface
        .send(
            http::Method::PUT,
            &format!("/oagw/v1/upstreams/{id}"),
            Some(oagw::test_support::security_context(leaf, Uuid::new_v4())),
            Some(body(&["https://leaf.dev", "https://admin.dev", "https://intruder.dev"])),
        )
        .await;
    assert_eq!(status, http::StatusCode::FORBIDDEN, "{}", String::from_utf8_lossy(&body_bytes));

    let evaluated_before = authz.evaluated.lock().expect("the evaluation log").len();
    let exchange = surface
        .proxy_for(leaf, Uuid::new_v4(), "GET", path, &[("origin", "https://intruder.dev")], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::FORBIDDEN, "{}", exchange.text());
    let body: Value = serde_json::from_slice(&exchange.body).expect("the problem+json body");
    assert_eq!(body["type"], ORIGIN_NOT_ALLOWED, "{body}");
    // The only permission the proxy path evaluates is its own invocation gate;
    // the merge asks for no override permission of its own.
    let evaluated: Vec<String> =
        authz.evaluated.lock().expect("the evaluation log").clone()[evaluated_before..].to_vec();
    assert_eq!(
        evaluated,
        vec!["gts.cf.core.oagw.proxy.v1~:invoke".to_owned()],
        "the merge performed no per-request permission check: {evaluated:?}"
    );
}
