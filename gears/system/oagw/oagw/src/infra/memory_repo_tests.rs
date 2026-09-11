//! Unit tests for the in-memory repositories.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{
    Endpoint, EndpointScheme, HeadersConfig, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode,
    PluginsConfig, Protocol, ServerConfig,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000001");
const OTHER_TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-000000000002");

fn upstream(alias: &str) -> Upstream {
    Upstream {
        id: Uuid::new_v4().to_string(),
        tenant_id: TENANT,
        alias: alias.to_owned(),
        enabled: true,
        protocol: Protocol::Http,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: "api.example.com".to_owned(),
                port: 80,
            }],
        },
        auth: None,
        headers: HeadersConfig::default(),
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

fn route(upstream_id: &str, path: &str) -> Route {
    Route {
        id: Uuid::new_v4().to_string(),
        tenant_id: TENANT,
        upstream_id: upstream_id.to_owned(),
        enabled: true,
        priority: 0,
        match_config: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::default(),
            }),
            grpc: None,
        },
        rate_limit: None,
        cors: None,
        plugins: PluginsConfig::default(),
        tags: Vec::new(),
        created_at: None,
        updated_at: None,
    }
}

#[tokio::test]
async fn create_get_and_find_by_alias_round_trip() {
    let repo = MemoryUpstreamRepo::new();
    let created = repo.create(upstream("api.example.com")).await.unwrap();
    let fetched = repo.find_by_alias(TENANT, "api.example.com").await.unwrap();
    assert_eq!(fetched.id, created.id);

    let by_id = repo.get(TENANT, &created.id).await.unwrap();
    assert_eq!(by_id.alias, "api.example.com");
}

#[tokio::test]
async fn alias_is_unique_per_tenant() {
    let repo = MemoryUpstreamRepo::new();
    repo.create(upstream("api.example.com")).await.unwrap();
    let second = repo.create(upstream("api.example.com")).await.unwrap_err();
    assert!(matches!(second, DomainError::Conflict(_)));
}

#[tokio::test]
async fn same_alias_on_another_tenant_is_allowed() {
    let repo = MemoryUpstreamRepo::new();
    repo.create(upstream("api.example.com")).await.unwrap();
    let mut other = upstream("api.example.com");
    other.tenant_id = OTHER_TENANT;
    assert!(repo.create(other).await.is_ok());
}

#[tokio::test]
async fn reads_are_tenant_scoped() {
    let repo = MemoryUpstreamRepo::new();
    let created = repo.create(upstream("api.example.com")).await.unwrap();
    assert!(repo.get(OTHER_TENANT, &created.id).await.is_err());
    assert!(
        repo.find_by_alias(OTHER_TENANT, "api.example.com")
            .await
            .is_err()
    );
    assert_eq!(repo.list(OTHER_TENANT).await.unwrap().len(), 0);
    assert_eq!(repo.list(TENANT).await.unwrap().len(), 1);
}

#[tokio::test]
async fn replace_updates_and_delete_removes() {
    let repo = MemoryUpstreamRepo::new();
    let created = repo.create(upstream("api.example.com")).await.unwrap();
    let mut replacement = created.clone();
    replacement.enabled = false;
    repo.replace(replacement).await.unwrap();
    assert!(!repo.get(TENANT, &created.id).await.unwrap().enabled);

    repo.delete(TENANT, &created.id).await.unwrap();
    assert!(repo.get(TENANT, &created.id).await.is_err());
    assert!(repo.delete(TENANT, &created.id).await.is_err());
}

#[tokio::test]
async fn route_crud_and_duplicate_detection() {
    let upstream = upstream("api.example.com").id;
    let repo = MemoryRouteRepo::new();
    let first = route(&upstream, "/v1/chat");
    repo.create(first.clone()).await.unwrap();

    let duplicate = route(&upstream, "/v1/chat");
    assert!(matches!(
        repo.create(duplicate).await,
        Err(DomainError::Conflict(_))
    ));

    let other_path = route(&upstream, "/v1/other");
    assert!(repo.create(other_path).await.is_ok());

    let by_upstream = repo.list_by_upstream(TENANT, &upstream).await.unwrap();
    assert_eq!(by_upstream.len(), 2);
}

#[tokio::test]
async fn route_replacement_keeps_upstream_immutable() {
    let upstream = upstream("api.example.com").id;
    let repo = MemoryRouteRepo::new();
    let created = repo.create(route(&upstream, "/v1/chat")).await.unwrap();

    let mut replacement = created.clone();
    replacement.upstream_id = "some-other-upstream".to_owned();
    let stored = repo.replace(replacement).await.unwrap();
    assert_eq!(stored.upstream_id, upstream);
}

#[tokio::test]
async fn route_cascade_delete_by_upstream() {
    let upstream = upstream("api.example.com").id;
    let repo = MemoryRouteRepo::new();
    let created = repo.create(route(&upstream, "/v1/chat")).await.unwrap();
    repo.create(route(&upstream, "/v1/other")).await.unwrap();

    repo.delete_by_upstream(TENANT, &upstream).await.unwrap();
    assert!(repo.get(TENANT, &created.id).await.is_err());
    assert_eq!(repo.list(TENANT).await.unwrap().len(), 0);
}

#[tokio::test]
async fn route_reads_are_tenant_scoped() {
    let upstream = upstream("api.example.com").id;
    let repo = MemoryRouteRepo::new();
    let created = repo.create(route(&upstream, "/v1/chat")).await.unwrap();
    assert!(repo.get(OTHER_TENANT, &created.id).await.is_err());
}

#[tokio::test]
async fn plugin_crud_round_trip() {
    let repo = MemoryPluginRepo::new();
    let plugin = Plugin {
        id: Uuid::new_v4().to_string(),
        tenant_id: TENANT,
        plugin_type: crate::domain::model::PluginKind::Guard,
        name: "my-guard".to_owned(),
        description: None,
        config_schema: None,
        source_code: "def on_request(ctx):\n    return ctx\n".to_owned(),
        phases: vec![crate::domain::model::PluginPhase::OnRequest],
    };
    let created = repo.create(plugin).await.unwrap();
    assert_eq!(
        repo.get(TENANT, &created.id).await.unwrap().name,
        "my-guard"
    );
    assert!(repo.get(OTHER_TENANT, &created.id).await.is_err());

    let duplicate = Plugin {
        id: Uuid::new_v4().to_string(),
        tenant_id: TENANT,
        ..created.clone()
    };
    assert!(matches!(
        repo.create(duplicate).await,
        Err(DomainError::Conflict(_))
    ));

    repo.delete(TENANT, &created.id).await.unwrap();
    assert!(repo.get(TENANT, &created.id).await.is_err());
}
