//! Multi-tenant behaviour observed through the proxy: alias shadowing,
//! inherited upstreams and routes, and enforced ancestor limits.
//!
//! `cpt-cf-oagw-fr-hierarchical-config` and `cpt-cf-oagw-fr-alias-resolution`.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use common::{Fixture, assert_gateway_problem, get};
use http::StatusCode;
use oagw::domain::error::DomainResult;
use oagw::domain::gts_helpers::errors;
use oagw::domain::model::SharingMode;
use oagw::domain::services::tenancy::TenantHierarchy;
use oagw::test_utils::TestGatewayBuilder;
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Hierarchy driven by an explicit child → ancestors map.
struct Scripted(HashMap<Uuid, Vec<Uuid>>);

#[async_trait]
impl TenantHierarchy for Scripted {
    async fn ancestors(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> DomainResult<Vec<Uuid>> {
        Ok(self.0.get(&tenant_id).cloned().unwrap_or_default())
    }
}

/// A gateway whose caller is `leaf`, with `root` as its parent.
async fn two_tier() -> (Fixture, SecurityContext, SecurityContext) {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let hierarchy = Arc::new(Scripted(HashMap::from([(leaf, vec![root])])));

    let fx = Fixture::with_builder(
        TestGatewayBuilder::new()
            .hierarchy(hierarchy)
            .tenant(leaf),
    )
    .await;

    let root_ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(root)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous());
    let leaf_ctx = fx.gateway.security_context.clone();
    (fx, root_ctx, leaf_ctx)
}

/// Register an upstream + route for a specific tenant.
async fn seed(
    fx: &Fixture,
    ctx: &SecurityContext,
    alias: &str,
    tweak: impl FnOnce(&mut oagw::domain::services::management::UpstreamSpec),
) -> oagw::domain::model::Upstream {
    let mut spec = oagw::domain::services::management::UpstreamSpec {
        alias: Some(alias.to_owned()),
        enabled: None,
        tags: Vec::new(),
        server: fx.endpoints(),
        protocol: oagw::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
        auth: None,
        headers: None,
        plugins: None,
        rate_limit: None,
        cors: None,
    };
    tweak(&mut spec);
    let upstream = fx
        .gateway
        .control_plane
        .create_upstream(ctx, spec)
        .await
        .expect("create upstream");

    fx.gateway
        .control_plane
        .create_route(
            ctx,
            oagw::domain::services::management::RouteSpec {
                upstream_id: Some(upstream.id),
                enabled: None,
                priority: None,
                tags: Vec::new(),
                match_config: oagw::domain::model::MatchConfig {
                    http: Some(oagw::domain::model::HttpMatch {
                        methods: vec!["GET".to_owned()],
                        path: "/v1".to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: oagw::domain::model::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                plugins: None,
                rate_limit: None,
                cors: None,
            },
        )
        .await
        .expect("create route");
    upstream
}

#[tokio::test]
async fn a_descendant_inherits_an_ancestors_upstream_and_route() {
    let (fx, root_ctx, _leaf_ctx) = two_tier().await;
    seed(&fx, &root_ctx, "shared", |spec| {
        let mut set = std::collections::BTreeMap::new();
        set.insert("x-owner".to_owned(), "root".to_owned());
        spec.headers = Some(oagw::domain::model::HeadersConfig {
            request: oagw::domain::model::RequestHeadersConfig {
                set,
                ..oagw::domain::model::RequestHeadersConfig::default()
            },
            ..oagw::domain::model::HeadersConfig::default()
        });
    })
    .await;

    // The leaf tenant owns nothing named `shared`, yet the proxy resolves it.
    let res = get(&fx.proxy_url("shared", "v1/echo")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(res.json()["headers"]["x-owner"], json!("root"));
}

#[tokio::test]
async fn an_ancestor_resource_is_invisible_through_the_management_api() {
    let (fx, root_ctx, leaf_ctx) = two_tier().await;
    let root_upstream = seed(&fx, &root_ctx, "shared", |_| {}).await;

    // Proxy-time inheritance does not make it manageable.
    let err = fx
        .gateway
        .control_plane
        .get_upstream(&leaf_ctx, root_upstream.id)
        .await
        .expect_err("ancestor resources are 404 to descendants");
    assert_eq!(err.status(), 404);
    assert!(
        fx.gateway
            .control_plane
            .list_upstreams(&leaf_ctx)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn a_descendant_shadows_an_ancestor_alias() {
    let (fx, root_ctx, leaf_ctx) = two_tier().await;
    seed(&fx, &root_ctx, "shared", |spec| {
        let mut set = std::collections::BTreeMap::new();
        set.insert("x-owner".to_owned(), "root".to_owned());
        spec.headers = Some(oagw::domain::model::HeadersConfig {
            request: oagw::domain::model::RequestHeadersConfig {
                set,
                ..oagw::domain::model::RequestHeadersConfig::default()
            },
            ..oagw::domain::model::HeadersConfig::default()
        });
    })
    .await;
    seed(&fx, &leaf_ctx, "shared", |spec| {
        let mut set = std::collections::BTreeMap::new();
        set.insert("x-owner".to_owned(), "leaf".to_owned());
        spec.headers = Some(oagw::domain::model::HeadersConfig {
            request: oagw::domain::model::RequestHeadersConfig {
                set,
                ..oagw::domain::model::RequestHeadersConfig::default()
            },
            ..oagw::domain::model::HeadersConfig::default()
        });
    })
    .await;

    let res = get(&fx.proxy_url("shared", "v1/echo")).await;
    assert_eq!(
        res.json()["headers"]["x-owner"],
        json!("leaf"),
        "the closest tenant in the chain wins"
    );
}

#[tokio::test]
async fn an_ancestor_disabling_an_upstream_disables_it_for_descendants() {
    let (fx, root_ctx, leaf_ctx) = two_tier().await;
    seed(&fx, &root_ctx, "shared", |spec| spec.enabled = Some(false)).await;
    seed(&fx, &leaf_ctx, "shared", |_| {}).await;

    let res = get(&fx.proxy_url("shared", "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::SERVICE_UNAVAILABLE,
        errors::LINK_UNAVAILABLE,
    );
}

#[tokio::test]
async fn an_enforced_ancestor_rate_limit_caps_a_looser_descendant() {
    let (fx, root_ctx, leaf_ctx) = two_tier().await;
    seed(&fx, &root_ctx, "shared", |spec| {
        let mut limit = common::per_minute(2, 2);
        limit.sharing = SharingMode::Enforce;
        spec.rate_limit = Some(limit);
    })
    .await;
    seed(&fx, &leaf_ctx, "shared", |spec| {
        // The descendant asks for far more than the ancestor allows.
        spec.rate_limit = Some(common::per_minute(10_000, 10_000));
    })
    .await;

    for attempt in 0..2 {
        assert_eq!(
            get(&fx.proxy_url("shared", "v1/echo")).await.status,
            StatusCode::OK,
            "burst token {attempt} is within the enforced ceiling"
        );
    }
    let res = get(&fx.proxy_url("shared", "v1/echo")).await;
    assert_gateway_problem(
        &res,
        StatusCode::TOO_MANY_REQUESTS,
        errors::RATE_LIMIT_EXCEEDED,
    );
}

#[tokio::test]
async fn an_inherited_auth_plugin_is_used_when_the_descendant_declares_none() {
    let root = Uuid::new_v4();
    let leaf = Uuid::new_v4();
    let fx = Fixture::with_builder(
        TestGatewayBuilder::new()
            .hierarchy(Arc::new(Scripted(HashMap::from([(leaf, vec![root])]))))
            .tenant(leaf)
            .secrets(vec![("partner-key", "sk-partner")]),
    )
    .await;
    let root_ctx = SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(root)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous());
    let leaf_ctx = fx.gateway.security_context.clone();

    seed(&fx, &root_ctx, "shared", |spec| {
        let mut auth = common::auth(
            oagw::domain::gts_helpers::APIKEY_AUTH_PLUGIN_ID,
            json!({ "secret_ref": "cred://partner-key", "name": "X-Api-Key" }),
        );
        auth.sharing = SharingMode::Inherit;
        spec.auth = Some(auth);
    })
    .await;
    // The descendant shadows the alias but declares no auth of its own.
    seed(&fx, &leaf_ctx, "shared", |_| {}).await;

    let res = get(&fx.proxy_url("shared", "v1/echo")).await;
    assert_eq!(res.status, StatusCode::OK);
    assert_eq!(
        res.json()["headers"]["x-api-key"],
        json!("sk-partner"),
        "an `inherit` ancestor supplies the credential the descendant omitted"
    );
}

#[tokio::test]
async fn tags_are_unioned_across_the_hierarchy() {
    let (fx, root_ctx, leaf_ctx) = two_tier().await;
    seed(&fx, &root_ctx, "shared", |spec| {
        spec.tags = vec!["llm".to_owned()];
    })
    .await;
    seed(&fx, &leaf_ctx, "shared", |spec| {
        spec.tags = vec!["openai".to_owned()];
    })
    .await;

    let target = fx
        .gateway
        .control_plane
        .resolve_proxy_target(&leaf_ctx, "shared", "GET", Some("v1/echo"))
        .await
        .expect("resolved");
    assert_eq!(target.tags, vec!["llm", "openai"]);
}
