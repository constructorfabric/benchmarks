//! Control Plane CRUD, validation and tenant-scoping tests.

use super::{ControlPlaneService, PluginInput, RouteInput, UpstreamInput};
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, HttpMatch, MatchConfig, PathSuffixMode, PluginBinding,
    PluginKind, PluginsConfig, ServerConfig, SharingMode,
};
use crate::domain::ports::{PluginCatalog, TenantDirectory};
use crate::infra::storage::{MemoryPluginRepo, MemoryRouteRepo, MemoryStore, MemoryUpstreamRepo};
use async_trait::async_trait;
use serde_json::Map;
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

struct FlatDirectory;

#[async_trait]
impl TenantDirectory for FlatDirectory {
    async fn chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        vec![tenant_id]
    }
}

struct BuiltinCatalog;

impl PluginCatalog for BuiltinCatalog {
    fn has_auth(&self, plugin_ref: &str) -> bool {
        matches!(
            plugin_ref,
            gts::NOOP_AUTH_PLUGIN_ID
                | gts::APIKEY_AUTH_PLUGIN_ID
                | gts::OAUTH2_CLIENT_CRED_AUTH_PLUGIN_ID
                | gts::OAUTH2_CLIENT_CRED_BASIC_AUTH_PLUGIN_ID
        )
    }
    fn has_guard(&self, plugin_ref: &str) -> bool {
        plugin_ref == gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }
    fn has_transform(&self, plugin_ref: &str) -> bool {
        plugin_ref == gts::REQUEST_ID_TRANSFORM_PLUGIN_ID
    }
}

fn service() -> ControlPlaneService {
    let store = MemoryStore::shared();
    ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepo::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepo::new(Arc::clone(&store))),
        Arc::new(MemoryPluginRepo::new(Arc::clone(&store))),
        Arc::new(BuiltinCatalog),
        Arc::new(FlatDirectory),
    )
}

fn ctx(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

fn https_pool(hosts: &[&str], port: u16) -> ServerConfig {
    ServerConfig {
        endpoints: hosts
            .iter()
            .map(|h| Endpoint {
                scheme: "https".to_owned(),
                host: (*h).to_owned(),
                port,
            })
            .collect(),
    }
}

fn upstream_input(server: ServerConfig) -> UpstreamInput {
    UpstreamInput {
        server: Some(server),
        protocol: Some(gts::PROTOCOL_HTTP.to_owned()),
        ..UpstreamInput::default()
    }
}

fn http_route_input(upstream_id: Uuid, path: &str, methods: &[&str]) -> RouteInput {
    RouteInput {
        upstream_id: Some(upstream_id),
        match_config: Some(MatchConfig {
            http: Some(HttpMatch {
                methods: methods.iter().map(|m| (*m).to_owned()).collect(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        }),
        ..RouteInput::default()
    }
}

// -- upstream CRUD ---------------------------------------------------------

#[tokio::test]
async fn create_derives_the_alias_and_defaults_enabled() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let created = svc
        .create_upstream(&ctx, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    assert_eq!(created.alias(), "api.openai.com");
    assert!(created.spec.enabled, "enabled defaults to true");
    assert_eq!(created.tenant_id, ctx.subject_tenant_id());
}

#[tokio::test]
async fn plaintext_http_upstream_is_accepted_at_create_time() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let created = svc
        .create_upstream(
            &ctx,
            UpstreamInput {
                server: Some(ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: "http".to_owned(),
                        host: "mock.local".to_owned(),
                        port: 80,
                    }],
                }),
                protocol: Some(gts::PROTOCOL_HTTP.to_owned()),
                ..UpstreamInput::default()
            },
        )
        .await
        .expect("http is a legal endpoint scheme");
    assert_eq!(created.alias(), "mock.local");
}

#[tokio::test]
async fn duplicate_alias_conflicts_within_a_tenant_only() {
    let svc = service();
    let tenant = Uuid::new_v4();
    let ctx_a = ctx(tenant);
    svc.create_upstream(&ctx_a, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("first");
    let err = svc
        .create_upstream(&ctx_a, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect_err("duplicate");
    assert_eq!(err.status, 409);

    let ctx_b = ctx(Uuid::new_v4());
    svc.create_upstream(&ctx_b, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("another tenant may shadow the alias");
}

#[tokio::test]
async fn missing_required_fields_are_rejected() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let err = svc
        .create_upstream(
            &ctx,
            UpstreamInput {
                protocol: Some(gts::PROTOCOL_HTTP.to_owned()),
                ..UpstreamInput::default()
            },
        )
        .await
        .expect_err("server required");
    assert_eq!(err.status, 400);

    let err = svc
        .create_upstream(
            &ctx,
            UpstreamInput {
                server: Some(https_pool(&["api.openai.com"], 443)),
                ..UpstreamInput::default()
            },
        )
        .await
        .expect_err("protocol required");
    assert_eq!(err.status, 400);
}

#[tokio::test]
async fn a_tenantless_caller_is_forbidden() {
    let svc = service();
    let anonymous = SecurityContext::anonymous();
    let err = svc
        .create_upstream(
            &anonymous,
            upstream_input(https_pool(&["api.openai.com"], 443)),
        )
        .await
        .expect_err("nil tenant refused");
    assert_eq!(err.status, 403);
}

#[tokio::test]
async fn reads_and_writes_are_tenant_scoped() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let stranger = ctx(Uuid::new_v4());
    let created = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");

    svc.get_upstream(&owner, created.id).await.expect("visible");
    assert_eq!(
        svc.get_upstream(&stranger, created.id)
            .await
            .expect_err("invisible")
            .status,
        404
    );
    assert_eq!(
        svc.delete_upstream(&stranger, created.id)
            .await
            .expect_err("invisible")
            .status,
        404
    );
    assert!(
        svc.list_upstreams(&stranger)
            .await
            .expect("listed")
            .is_empty()
    );
}

#[tokio::test]
async fn replace_rejects_an_alias_moving_endpoint_change() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let created = svc
        .create_upstream(&ctx, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");

    let err = svc
        .replace_upstream(
            &ctx,
            created.id,
            upstream_input(https_pool(&["api.anthropic.com"], 443)),
        )
        .await
        .expect_err("alias would move");
    assert_eq!(err.status, 400);

    // Same endpoints: the replace goes through and clears omitted blocks.
    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.enabled = Some(false);
    let replaced = svc
        .replace_upstream(&ctx, created.id, input)
        .await
        .expect("replaced");
    assert!(!replaced.spec.enabled);
    assert_eq!(replaced.created_at, created.created_at);
}

#[tokio::test]
async fn replace_of_a_missing_upstream_is_404() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let err = svc
        .replace_upstream(
            &ctx,
            Uuid::new_v4(),
            upstream_input(https_pool(&["api.openai.com"], 443)),
        )
        .await
        .expect_err("missing");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn catalog_only_auth_plugin_is_rejected_at_create_time() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.auth = Some(AuthConfig {
        plugin_type: Some(gts::BASIC_AUTH_PLUGIN_ID.to_owned()),
        sharing: SharingMode::Private,
        config: Map::new(),
    });
    let err = svc
        .create_upstream(&ctx, input)
        .await
        .expect_err("rejected");
    assert_eq!(err.status, 400);
    assert!(err.detail.contains("unknown auth plugin"));
}

#[tokio::test]
async fn cors_credentials_with_wildcard_is_rejected_at_create_time() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.cors = Some(CorsConfig {
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allow_credentials: true,
        ..CorsConfig::default()
    });
    assert_eq!(
        svc.create_upstream(&ctx, input)
            .await
            .expect_err("rejected")
            .status,
        400
    );
}

#[tokio::test]
async fn an_unknown_chain_plugin_is_rejected() {
    let svc = service();
    let ctx = ctx(Uuid::new_v4());
    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: gts::CORS_GUARD_PLUGIN_ID.to_owned(),
            config: Map::new(),
        }],
    });
    let err = svc
        .create_upstream(&ctx, input)
        .await
        .expect_err("rejected");
    assert_eq!(err.status, 400);
}

// -- route CRUD ------------------------------------------------------------

#[tokio::test]
async fn route_requires_an_upstream_owned_by_the_caller() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let stranger = ctx(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");

    svc.create_route(&owner, http_route_input(upstream.id, "/v1/chat", &["POST"]))
        .await
        .expect("created");

    // An ancestor's (or anyone else's) upstream is not addressable: 400, per
    // the "Upstream not found" alternative flow.
    let err = svc
        .create_route(
            &stranger,
            http_route_input(upstream.id, "/v1/chat", &["POST"]),
        )
        .await
        .expect_err("not addressable");
    assert_eq!(err.status, 400);

    let err = svc
        .create_route(&owner, RouteInput::default())
        .await
        .expect_err("upstream_id required");
    assert_eq!(err.status, 400);
}

#[tokio::test]
async fn duplicate_route_match_rules_conflict() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    svc.create_route(&owner, http_route_input(upstream.id, "/v1/chat", &["POST"]))
        .await
        .expect("first");
    let err = svc
        .create_route(&owner, http_route_input(upstream.id, "/v1/chat", &["POST"]))
        .await
        .expect_err("collision");
    assert_eq!(err.status, 409);
}

#[tokio::test]
async fn route_path_is_normalized_and_methods_uppercased() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    let route = svc
        .create_route(&owner, http_route_input(upstream.id, "v1/chat/", &["post"]))
        .await
        .expect("created");
    let http = route.http().expect("http match");
    assert_eq!(http.path, "/v1/chat");
    assert_eq!(http.methods, vec!["POST"]);
}

#[tokio::test]
async fn route_upstream_id_is_immutable() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let first = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    let second = svc
        .create_upstream(
            &owner,
            upstream_input(https_pool(&["api.anthropic.com"], 443)),
        )
        .await
        .expect("created");
    let route = svc
        .create_route(&owner, http_route_input(first.id, "/v1/chat", &["POST"]))
        .await
        .expect("created");

    let err = svc
        .replace_route(
            &owner,
            route.id,
            http_route_input(second.id, "/v1/chat", &["POST"]),
        )
        .await
        .expect_err("immutable");
    assert_eq!(err.status, 400);

    // Re-sending the same upstream_id is fine.
    let replaced = svc
        .replace_route(
            &owner,
            route.id,
            http_route_input(first.id, "/v1/messages", &["POST"]),
        )
        .await
        .expect("replaced");
    assert_eq!(replaced.http().expect("http").path, "/v1/messages");
    assert_eq!(replaced.upstream_id, first.id);
}

#[tokio::test]
async fn grpc_match_is_rejected_for_an_http_upstream() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    let err = svc
        .create_route(
            &owner,
            RouteInput {
                upstream_id: Some(upstream.id),
                match_config: Some(MatchConfig {
                    http: None,
                    grpc: Some(crate::domain::model::GrpcMatch {
                        service: "foo.v1.Users".to_owned(),
                        method: "Get".to_owned(),
                    }),
                }),
                ..RouteInput::default()
            },
        )
        .await
        .expect_err("protocol mismatch");
    assert_eq!(err.status, 400);
}

#[tokio::test]
async fn deleting_an_upstream_cascades_to_its_routes() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let upstream = svc
        .create_upstream(&owner, upstream_input(https_pool(&["api.openai.com"], 443)))
        .await
        .expect("created");
    svc.create_route(&owner, http_route_input(upstream.id, "/v1/chat", &["POST"]))
        .await
        .expect("created");
    svc.delete_upstream(&owner, upstream.id)
        .await
        .expect("deleted");
    assert!(svc.list_routes(&owner).await.expect("listed").is_empty());
}

// -- plugin CRUD -----------------------------------------------------------

fn plugin_input(kind: PluginKind, name: &str) -> PluginInput {
    PluginInput {
        kind: Some(kind),
        name: Some(name.to_owned()),
        source_code: Some("def on_request(ctx):\n    return ctx.next()\n".to_owned()),
        ..PluginInput::default()
    }
}

#[tokio::test]
async fn plugin_create_get_delete() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let created = svc
        .create_plugin(&owner, plugin_input(PluginKind::Guard, "validator"))
        .await
        .expect("created");
    assert!(created.gts_id().starts_with(gts::GUARD_PLUGIN_TYPE));
    svc.get_plugin(&owner, created.id).await.expect("visible");
    svc.delete_plugin(&owner, created.id)
        .await
        .expect("deleted");
    assert_eq!(
        svc.get_plugin(&owner, created.id)
            .await
            .expect_err("gone")
            .status,
        404
    );
}

#[tokio::test]
async fn plugin_in_use_blocks_deletion() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&owner, plugin_input(PluginKind::Guard, "validator"))
        .await
        .expect("created");
    let plugin_ref = plugin.gts_id();

    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: plugin_ref.clone(),
            config: Map::new(),
        }],
    });
    svc.create_upstream(&owner, input).await.expect("bound");

    let err = svc
        .delete_plugin(&owner, plugin.id)
        .await
        .expect_err("in use");
    assert_eq!(err.status, 409);
    assert_eq!(err.error_type, gts::ERR_PLUGIN_IN_USE);
    assert_eq!(
        err.extensions.get("plugin_id").and_then(|v| v.as_str()),
        Some(plugin_ref.as_str())
    );
    let referenced = err.extensions.get("referenced_by").expect("references");
    assert_eq!(referenced["upstreams"].as_array().map(Vec::len), Some(1));
    assert_eq!(referenced["routes"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn linked_plugin_ids_covers_auth_and_chain_bindings() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let guard = svc
        .create_plugin(&owner, plugin_input(PluginKind::Guard, "guard"))
        .await
        .expect("created");
    let auth = svc
        .create_plugin(&owner, plugin_input(PluginKind::Auth, "auth"))
        .await
        .expect("created");

    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.auth = Some(AuthConfig {
        plugin_type: Some(auth.gts_id()),
        sharing: SharingMode::Private,
        config: Map::new(),
    });
    input.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: guard.gts_id(),
            config: Map::new(),
        }],
    });
    svc.create_upstream(&owner, input).await.expect("created");

    let linked = svc.linked_plugin_ids().await;
    assert!(linked.contains(&guard.id));
    assert!(linked.contains(&auth.id));
}

#[tokio::test]
async fn binding_a_plugin_from_another_tenant_is_rejected() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let stranger = ctx(Uuid::new_v4());
    let plugin = svc
        .create_plugin(&owner, plugin_input(PluginKind::Guard, "validator"))
        .await
        .expect("created");

    let mut input = upstream_input(https_pool(&["api.openai.com"], 443));
    input.plugins = Some(PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginBinding {
            plugin_ref: plugin.gts_id(),
            config: Map::new(),
        }],
    });
    let err = svc
        .create_upstream(&stranger, input)
        .await
        .expect_err("cross-tenant binding refused");
    assert_eq!(err.status, 400);
}

#[tokio::test]
async fn plugin_requires_name_and_source() {
    let svc = service();
    let owner = ctx(Uuid::new_v4());
    let err = svc
        .create_plugin(
            &owner,
            PluginInput {
                kind: Some(PluginKind::Guard),
                source_code: Some("x".to_owned()),
                ..PluginInput::default()
            },
        )
        .await
        .expect_err("name required");
    assert_eq!(err.status, 400);

    let err = svc
        .create_plugin(
            &owner,
            PluginInput {
                kind: Some(PluginKind::Guard),
                name: Some("x".to_owned()),
                ..PluginInput::default()
            },
        )
        .await
        .expect_err("source required");
    assert_eq!(err.status, 400);
}
