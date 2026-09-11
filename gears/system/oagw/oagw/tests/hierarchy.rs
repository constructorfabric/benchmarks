//! Tenant hierarchy: shadowing, fallback and enforced ancestor constraints.

mod common;

use common::*;

/// The root tenant's app and a child's, over one store.
fn pair() -> (App, App) {
    let resolver = MockResolver::default_hierarchy();
    let root = App::new(resolver.clone(), context_for(TENANT_ROOT), test_config());
    let child = App::with_store(
        root.store.clone(),
        resolver,
        context_for(TENANT_L1A),
        test_config(),
    );
    (root, child)
}

#[tokio::test]
async fn a_descendant_shadows_the_ancestors_upstream() {
    let (mut root, mut child) = pair();
    let root_upstream = root
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    root.create_route(route_body(&root_upstream, "/v1")).await;

    // The child publishes its own upstream under the same alias.
    let child_upstream = child
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    assert_ne!(root_upstream, child_upstream);

    // The child sees exactly its own upstream.
    let (_, list) = child.send("GET", "/oagw/v1/upstreams", None).await;
    assert_eq!(list["count"], 1, "{list}");
    assert_eq!(list["items"][0]["id"], child_upstream.as_str());
    // …and cannot read the parent's, even though it would resolve through it.
    let (status, _) = child
        .send("GET", &format!("/oagw/v1/upstreams/{root_upstream}"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_descendant_with_nothing_of_its_own_falls_back_to_the_ancestor() {
    let (mut root, mut child) = pair();
    let id = root
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    root.create_route(route_body(&id, "/v1")).await;

    // The child owns nothing, but the parent's alias still resolves for it.
    let (status, _) = child
        .send("GET", &proxy_path("api.vendor.com", "v1"), None)
        .await;
    // There is no upstream behind it, so the dial fails — but the alias was
    // found: a missing alias is a 404 route-not-found instead.
    assert_ne!(status, http::StatusCode::NOT_FOUND, "{status}");

    // An alias the hierarchy never registered is a 404.
    let (status, problem) = child
        .send("GET", &proxy_path("nowhere.example", "v1"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["type"], oagw::gts::errors::ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn a_tenant_outside_the_hierarchy_sees_nothing() {
    let (mut root, _child) = pair();
    let id = root
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    root.create_route(route_body(&id, "/v1")).await;

    let mut outsider = App::new(
        MockResolver::default_hierarchy(),
        context_for(TENANT_OTHER),
        test_config(),
    );
    let (status, problem) = outsider
        .send("GET", &proxy_path("api.vendor.com", "v1"), None)
        .await;
    assert_eq!(status, http::StatusCode::NOT_FOUND, "{problem}");
    assert_eq!(problem["type"], oagw::gts::errors::ROUTE_NOT_FOUND);
}

#[tokio::test]
async fn an_enforced_ancestor_rate_limit_applies_to_a_descendant() {
    let (mut root, mut child) = pair();
    let mock = echo_upstream().await;
    let mut body = loopback_upstream("vendor", mock.port());
    body["rate_limit"] = serde_json::json!({
        "sharing": "enforce",
        "sustained": {"rate": 1, "window": "minute"},
        "burst": {"capacity": 1}
    });
    let id = root.create_upstream(body).await;
    root.create_route(route_body(&id, "/v1")).await;

    let (first, _) = child.send("GET", &proxy_path("vendor", "v1"), None).await;
    assert_eq!(first, http::StatusCode::OK, "{first}");

    // The child owns no rate limit of its own; the ancestor's `enforce`
    // supplies one, and its bucket runs dry on the second call.
    let (second, _) = child.send("GET", &proxy_path("vendor", "v1"), None).await;
    assert_eq!(second, http::StatusCode::TOO_MANY_REQUESTS, "{second}");
    let (_, problem) = child.send("GET", &proxy_path("vendor", "v1"), None).await;
    assert_eq!(problem["type"], oagw::gts::errors::RATE_LIMIT_EXCEEDED);
}

#[tokio::test]
async fn a_disabled_ancestor_upstream_is_disabled_for_descendants() {
    let (mut root, mut child) = pair();
    let id = root
        .create_upstream(upstream_body("api.vendor.com", 443))
        .await;
    root.create_route(route_body(&id, "/v1")).await;

    let mut disabled = upstream_body("api.vendor.com", 443);
    disabled["enabled"] = serde_json::json!(false);
    let (status, _) = root
        .send("PUT", &format!("/oagw/v1/upstreams/{id}"), Some(disabled))
        .await;
    assert_eq!(status, http::StatusCode::OK);

    let (status, problem) = child
        .send("GET", &proxy_path("api.vendor.com", "v1"), None)
        .await;
    assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE, "{problem}");
    assert_eq!(problem["type"], oagw::gts::errors::LINK_UNAVAILABLE);
}

#[tokio::test]
async fn an_alias_resolves_case_insensitively() {
    let (mut root, _child) = pair();
    let id = root
        .create_upstream(upstream_body("api.Vendor.COM", 443))
        .await;
    root.create_route(route_body(&id, "/v1")).await;

    // The stored alias is normalized to lowercase.
    let (_, got) = root
        .send("GET", &format!("/oagw/v1/upstreams/{id}"), None)
        .await;
    assert_eq!(got["alias"], "api.vendor.com");

    let (status, _) = root
        .send("GET", &proxy_path("API.VENDOR.COM", "v1"), None)
        .await;
    assert_ne!(
        status,
        http::StatusCode::NOT_FOUND,
        "resolution is case-insensitive"
    );
}
