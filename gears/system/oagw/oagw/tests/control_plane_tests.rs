// Control Plane integration tests: alias derivation, tenant shadowing, CRUD
// and the error semantics of the management surface.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tenant_resolver_sdk::{TenantId, TenantResolverClient};

use common::{
    Harness, ProxyOptions, StaticTenants, context_for, gateway_request, http_route, local_server,
    text, upstream_shell,
};
use oagw::domain::model::{Endpoint, EndpointScheme};

fn options() -> ProxyOptions {
    ProxyOptions {
        proxy_timeout_secs: 5,
        allow_http_upstream: true,
        ssrf_enabled: false,
    }
}

#[tokio::test]
async fn a_hostname_alias_is_derived_from_a_single_endpoint() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let mut upstream = upstream_shell(1);
    upstream.server = oagw::domain::model::ServerConfig {
        endpoints: vec![Endpoint {
            scheme: EndpointScheme::Https,
            host: "api.openai.com".to_owned(),
            port: 443,
        }],
    };
    upstream.alias = String::new();

    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream)
        .await
        .expect("upstream");
    assert_eq!(created.alias, "api.openai.com");
}

#[tokio::test]
async fn a_registrable_common_suffix_is_derived_and_requires_a_target_host() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let mut upstream = upstream_shell(1);
    upstream.alias = String::new();
    upstream.server = oagw::domain::model::ServerConfig {
        endpoints: vec![
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "us.vendor.com".to_owned(),
                port: 443,
            },
            Endpoint {
                scheme: EndpointScheme::Https,
                host: "eu.vendor.com".to_owned(),
                port: 443,
            },
        ],
    };

    let created = harness
        .control_plane()
        .create_upstream(&ctx, upstream)
        .await
        .expect("upstream");
    assert_eq!(created.alias, "vendor.com");

    let route = http_route(created.id, "/v1", &["GET"]);
    harness
        .control_plane()
        .create_route(&ctx, route)
        .await
        .expect("route");

    let request = gateway_request("GET", "/oagw/v1/proxy/vendor.com/v1/items");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = text(&mut response).await;
    assert!(
        body.contains("cf.oagw.routing.missing_target_host.v1"),
        "{body}"
    );

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/vendor.com/v1")
        .header("x-oagw-target-host", "eu.vendor.com")
        .body(Body::empty())
        .unwrap();
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/proxy/vendor.com/v1")
        .header("x-oagw-target-host", "other.vendor.com")
        .body(Body::empty())
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = text(&mut response).await;
    assert!(
        body.contains("cf.oagw.routing.unknown_target_host.v1"),
        "{body}"
    );
}

#[tokio::test]
async fn a_descendant_upstream_shadows_the_ancestor_of_the_same_alias() {
    let root = TenantId(uuid::Uuid::new_v4());
    let child = TenantId(uuid::Uuid::new_v4());
    let tenants: Arc<dyn TenantResolverClient> = Arc::new(StaticTenants::root_child(root, child));
    let harness = Harness::new_with_tenants(options(), Some(tenants));

    let root_ctx = context_for(root);
    let child_ctx = context_for(child);

    // The root owns the alias first.
    let parent = harness
        .control_plane()
        .create_upstream(&root_ctx, upstream_shell(1))
        .await
        .expect("parent upstream");
    harness
        .control_plane()
        .create_route(&root_ctx, http_route(parent.id, "/", &["GET"]))
        .await
        .expect("parent route");

    // The child re-declares the same alias: its own upstream must win.
    let own = harness
        .control_plane()
        .create_upstream(&child_ctx, upstream_shell(1))
        .await
        .expect("child upstream");
    harness
        .control_plane()
        .create_route(&child_ctx, http_route(own.id, "/", &["GET"]))
        .await
        .expect("child route");

    assert_ne!(parent.id, own.id);
    // The root's upstream stays visible to the root alone.
    let fetched = harness
        .control_plane()
        .get_upstream(&root_ctx, &parent.id.to_string())
        .await
        .expect("parent upstream");
    assert_eq!(fetched.id, parent.id);
    let hidden = harness
        .control_plane()
        .get_upstream(&child_ctx, &parent.id.to_string())
        .await
        .is_err();
    assert!(hidden, "ancestors are invisible through the management API");
}

#[tokio::test]
async fn an_inherited_route_and_upstream_are_reported_as_inherited() {
    let root = TenantId(uuid::Uuid::new_v4());
    let child = TenantId(uuid::Uuid::new_v4());
    let tenants: Arc<dyn TenantResolverClient> = Arc::new(StaticTenants::root_child(root, child));
    let harness = Harness::new_with_tenants(options(), Some(tenants));

    let root_ctx = context_for(root);
    let child_ctx = context_for(child);
    let parent = harness
        .control_plane()
        .create_upstream(&root_ctx, upstream_shell(1))
        .await
        .expect("parent upstream");
    harness
        .control_plane()
        .create_route(&root_ctx, http_route(parent.id, "/", &["GET"]))
        .await
        .expect("parent route");

    let target = harness
        .control_plane()
        .resolve_proxy_target(&child_ctx, "local", "GET", "/", "")
        .await
        .expect("target");
    assert!(target.inherited);
    assert!(target.route_inherited);
    assert_eq!(target.upstream.id, parent.id);
}

#[tokio::test]
async fn management_crud_round_trips_a_resource() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let control = harness.control_plane();

    let created = control
        .create_upstream(&ctx, upstream_shell(1))
        .await
        .expect("upstream");
    control
        .create_route(&ctx, http_route(created.id, "/v1", &["GET"]))
        .await
        .expect("route");

    // Deleting an upstream that still has routes is a 409.
    let error = control
        .delete_upstream(&ctx, &created.id.to_string())
        .await
        .expect_err("routes still reference it");
    assert_eq!(error.status(), 409);

    // The alias is unique per tenant.
    let clash = control
        .create_upstream(&ctx, upstream_shell(2))
        .await
        .expect_err("duplicate alias");
    assert_eq!(clash.status(), 409);

    // A tenant only sees its own resources.
    let stranger = context_for(TenantId(uuid::Uuid::new_v4()));
    assert!(
        control
            .get_upstream(&stranger, &created.id.to_string())
            .await
            .is_err()
    );
    assert!(
        control
            .delete_upstream(&stranger, &created.id.to_string())
            .await
            .is_err()
    );
    assert!(
        control
            .get_upstream(&ctx, &created.id.to_string())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn cleartext_endpoints_are_rejected_unless_the_deployment_allows_them() {
    let harness = Harness::new(ProxyOptions::default());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));
    let error = harness
        .control_plane()
        .create_upstream(&ctx, upstream_shell(8080))
        .await
        .expect_err("cleartext endpoint");
    assert_eq!(error.status(), 400);
    assert_eq!(harness.store.upstream_count(), 0);

    let permissive = Harness::new(options());
    let created = permissive
        .control_plane()
        .create_upstream(&ctx, upstream_shell(8080))
        .await
        .expect("allowed when the deployment permits cleartext");
    assert_eq!(created.server, local_server(8080));
}

#[tokio::test]
async fn the_rest_surface_reports_the_documented_status_codes() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"alias":"local","protocol":"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1","server":{"endpoints":[{"scheme":"http","host":"127.0.0.1","port":8080}]}}"#,
        ))
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = text(&mut response).await;
    assert!(body.contains("\"alias\":\"local\""), "{body}");

    let request = Request::builder()
        .method("GET")
        .uri("/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000")
        .body(Body::empty())
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    let body = text(&mut response).await;
    assert!(body.contains("cf.oagw.route.not_found.v1"), "{body}");

    let request = Request::builder()
        .method("DELETE")
        .uri("/oagw/v1/upstreams/00000000-0000-0000-0000-000000000000")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        harness.send(&ctx, request).await.status(),
        StatusCode::NOT_FOUND
    );

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/upstreams")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"unknown":true}"#))
        .unwrap();
    let response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn plugins_are_created_listed_and_only_deleted_when_unlinked() {
    let harness = Harness::new(options());
    let ctx = context_for(TenantId(uuid::Uuid::new_v4()));

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/plugins")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"type":"transform","name":"correlation","source_code":"def transform(ctx): pass"}"#,
        ))
        .unwrap();
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = text(&mut response).await;
    assert!(body.contains("\"name\":\"correlation\""), "{body}");

    let request = gateway_request("GET", "/oagw/v1/plugins");
    let mut response = harness.send(&ctx, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(text(&mut response).await.contains("correlation"));

    let request = Request::builder()
        .method("POST")
        .uri("/oagw/v1/plugins")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"type":"transform","name":"correlation","source_code":"def transform(ctx): pass"}"#,
        ))
        .unwrap();
    assert_eq!(
        harness.send(&ctx, request).await.status(),
        StatusCode::CONFLICT
    );
}
