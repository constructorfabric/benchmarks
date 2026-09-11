//! Control Plane behaviour: CRUD validation, tenant scoping, alias
//! resolution, route matching and hierarchical shadowing.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::*;
use crate::domain::plugin::PluginCatalog;
use crate::domain::repo::TenantDirectory;
use crate::infra::plugin::{PluginRegistries, TokenCacheConfig};
use crate::infra::storage::{InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo};

const HTTP: &str = gts_helpers::PROTOCOL_HTTP;
const GRPC: &str = gts_helpers::PROTOCOL_GRPC;

/// Directory returning a fixed descendant → root chain.
struct ScriptedDirectory(Vec<Uuid>);

#[async_trait]
impl TenantDirectory for ScriptedDirectory {
    async fn chain(&self, _ctx: &SecurityContext, tenant_id: Uuid) -> Vec<Uuid> {
        if self.0.first() == Some(&tenant_id) {
            self.0.clone()
        } else {
            vec![tenant_id]
        }
    }
}

struct Harness {
    cp: Arc<ControlPlaneService>,
}

impl Harness {
    fn with_directory(directory: Arc<dyn TenantDirectory>) -> Self {
        let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        let registries = Arc::new(PluginRegistries::with_builtins(
            credstore,
            TokenCacheConfig::default(),
        ));
        let catalog: Arc<dyn PluginCatalog> = registries;
        Self {
            cp: Arc::new(ControlPlaneService::new(
                Arc::new(InMemoryUpstreamRepo::new()),
                Arc::new(InMemoryRouteRepo::new()),
                Arc::new(InMemoryPluginRepo::new()),
                directory,
                catalog,
            )),
        }
    }

    fn new() -> Self {
        Self::with_directory(Arc::new(crate::infra::tenant_directory::FlatTenantDirectory))
    }
}

fn ctx_for(tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant)
        .build()
        .expect("security context")
}

fn upstream_input(value: serde_json::Value) -> UpstreamWriteInput {
    serde_json::from_value(value).expect("upstream input")
}

fn route_input(value: serde_json::Value) -> RouteWriteInput {
    serde_json::from_value(value).expect("route input")
}

fn plugin_input(value: serde_json::Value) -> PluginWriteInput {
    serde_json::from_value(value).expect("plugin input")
}

fn simple_upstream(alias: Option<&str>, host: &str, port: u16) -> serde_json::Value {
    let mut body = json!({
        "server": {"endpoints": [{"scheme": "http", "host": host, "port": port}]},
        "protocol": HTTP,
    });
    if let Some(alias) = alias {
        body["alias"] = json!(alias);
    }
    body
}

// ---------------------------------------------------------------------------
// Upstream CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_derives_the_alias_and_fills_default_ports() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let created = h
        .cp
        .create_upstream(
            &ctx,
            upstream_input(json!({
                "server": {"endpoints": [{"scheme": "https", "host": "API.OpenAI.com."}]},
                "protocol": HTTP,
            })),
        )
        .await
        .expect("created");
    assert_eq!(created.alias, "api.openai.com");
    assert_eq!(created.server.endpoints[0].port, 443);
    assert_eq!(created.server.endpoints[0].host, "api.openai.com");
    assert!(created.enabled);
}

#[tokio::test]
async fn http_is_a_legal_endpoint_scheme() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let created = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "example.com", 80)))
        .await
        .expect("plaintext upstreams are accepted at create time");
    assert_eq!(created.alias, "example.com");
    assert_eq!(created.server.endpoints[0].port, 80);
}

#[tokio::test]
async fn alias_is_unique_per_tenant_not_globally() {
    let h = Harness::new();
    let a = ctx_for(Uuid::new_v4());
    let b = ctx_for(Uuid::new_v4());
    h.cp.create_upstream(&a, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("first");

    let err = h
        .cp
        .create_upstream(&a, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect_err("duplicate for the same tenant");
    assert_eq!(err.status(), 409);

    h.cp.create_upstream(&b, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("another tenant may reuse the alias");
}

#[tokio::test]
async fn an_unknown_protocol_is_rejected() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let err = h
        .cp
        .create_upstream(
            &ctx,
            upstream_input(json!({
                "server": {"endpoints": [{"scheme": "https", "host": "a.example.com"}]},
                "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.smtp.v1",
            })),
        )
        .await
        .expect_err("bad protocol");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn tags_must_match_the_schema_pattern() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let mut body = simple_upstream(None, "a.example.com", 80);
    body["tags"] = json!(["openai", "llm"]);
    h.cp.create_upstream(&ctx, upstream_input(body.clone()))
        .await
        .expect("valid tags");

    body["server"]["endpoints"][0]["host"] = json!("b.example.com");
    body["tags"] = json!(["Not Valid"]);
    let err = h
        .cp
        .create_upstream(&ctx, upstream_input(body))
        .await
        .expect_err("invalid tag");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn cors_credentials_cannot_be_combined_with_a_wildcard_origin() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let mut body = simple_upstream(None, "a.example.com", 80);
    body["cors"] = json!({"enabled": true, "allowed_origins": ["*"], "allow_credentials": true});
    let err = h
        .cp
        .create_upstream(&ctx, upstream_input(body))
        .await
        .expect_err("rejected at validation time");
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("allow_credentials"));
}

#[tokio::test]
async fn catalog_only_auth_plugins_have_no_implementation() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    for id in [
        gts_helpers::BASIC_AUTH_PLUGIN_ID,
        gts_helpers::BEARER_AUTH_PLUGIN_ID,
    ] {
        let mut body = simple_upstream(Some("x"), "10.0.0.1", 80);
        body["auth"] = json!({"type": id});
        let err = h
            .cp
            .create_upstream(&ctx, upstream_input(body))
            .await
            .expect_err(id);
        assert_eq!(err.status(), 400);
        assert!(err.detail.contains("unknown auth plugin"), "{}", err.detail);
    }

    let mut body = simple_upstream(Some("y"), "10.0.0.2", 80);
    body["auth"] = json!({"type": gts_helpers::APIKEY_AUTH_PLUGIN_ID});
    h.cp.create_upstream(&ctx, upstream_input(body))
        .await
        .expect("apikey is implemented");
}

#[tokio::test]
async fn timeout_and_cors_identifiers_are_not_bindable_as_guards() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    for id in [
        gts_helpers::TIMEOUT_GUARD_PLUGIN_ID,
        gts_helpers::CORS_GUARD_PLUGIN_ID,
    ] {
        let mut body = simple_upstream(Some("z"), "10.0.0.3", 80);
        body["plugins"] = json!({"items": [id]});
        let err = h
            .cp
            .create_upstream(&ctx, upstream_input(body))
            .await
            .expect_err(id);
        assert_eq!(err.status(), 400);
    }

    let mut body = simple_upstream(Some("ok"), "10.0.0.4", 80);
    body["plugins"] = json!({"items": [gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID]});
    h.cp.create_upstream(&ctx, upstream_input(body))
        .await
        .expect("required_headers is bindable");
}

#[tokio::test]
async fn replace_clears_omitted_members_and_pins_the_alias() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let mut body = simple_upstream(None, "api.example.com", 80);
    body["tags"] = json!(["one"]);
    let created = h
        .cp
        .create_upstream(&ctx, upstream_input(body))
        .await
        .expect("created");
    assert_eq!(created.tags, vec!["one"]);

    let replaced = h
        .cp
        .replace_upstream(
            &ctx,
            created.id,
            upstream_input(simple_upstream(None, "api.example.com", 80)),
        )
        .await
        .expect("replaced");
    assert!(replaced.tags.is_empty(), "omitted members are cleared");
    assert_eq!(replaced.id, created.id);
    assert_eq!(replaced.created_at, created.created_at);

    let err = h
        .cp
        .replace_upstream(
            &ctx,
            created.id,
            upstream_input(simple_upstream(None, "other.example.com", 80)),
        )
        .await
        .expect_err("alias is immutable");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn reads_and_writes_are_scoped_to_the_calling_tenant() {
    let h = Harness::new();
    let owner = ctx_for(Uuid::new_v4());
    let stranger = ctx_for(Uuid::new_v4());
    let created = h
        .cp
        .create_upstream(&owner, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("created");

    assert_eq!(h.cp.get_upstream(&stranger, created.id).await.unwrap_err().status(), 404);
    assert_eq!(
        h.cp.replace_upstream(
            &stranger,
            created.id,
            upstream_input(simple_upstream(None, "api.example.com", 80))
        )
        .await
        .unwrap_err()
        .status(),
        404
    );
    assert_eq!(
        h.cp.delete_upstream(&stranger, created.id).await.unwrap_err().status(),
        404
    );
    assert!(h.cp.list_upstreams(&stranger).await.expect("list").is_empty());
}

#[tokio::test]
async fn deleting_an_upstream_cascades_to_its_routes() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");
    let route = h
        .cp
        .create_route(
            &ctx,
            route_input(json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .await
        .expect("route");

    h.cp.delete_upstream(&ctx, upstream.id).await.expect("deleted");
    assert_eq!(h.cp.get_route(&ctx, route.id).await.unwrap_err().status(), 404);
}

// ---------------------------------------------------------------------------
// Route CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_route_needs_an_upstream_owned_by_the_caller() {
    let h = Harness::new();
    let owner = ctx_for(Uuid::new_v4());
    let stranger = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&owner, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");

    let body = json!({
        "upstream_id": upstream.id,
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
    });
    // Ancestor / foreign upstreams are not addressable: a 400, per the use case.
    let err = h
        .cp
        .create_route(&stranger, route_input(body.clone()))
        .await
        .expect_err("foreign upstream");
    assert_eq!(err.status(), 400);

    let err = h
        .cp
        .create_route(
            &owner,
            route_input(json!({
                "upstream_id": Uuid::new_v4(),
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .await
        .expect_err("unknown upstream");
    assert_eq!(err.status(), 400);

    h.cp.create_route(&owner, route_input(body))
        .await
        .expect("own upstream");
}

#[tokio::test]
async fn match_must_name_exactly_one_protocol() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");

    for bad in [
        json!({}),
        json!({"http": {"methods": ["GET"], "path": "/v1"},
               "grpc": {"service": "s", "method": "m"}}),
    ] {
        let err = h
            .cp
            .create_route(
                &ctx,
                route_input(json!({"upstream_id": upstream.id, "match": bad})),
            )
            .await
            .expect_err("exactly one of http|grpc");
        assert_eq!(err.status(), 400);
    }

    // An HTTP upstream cannot carry a gRPC route.
    let err = h
        .cp
        .create_route(
            &ctx,
            route_input(json!({
                "upstream_id": upstream.id,
                "match": {"grpc": {"service": "s", "method": "m"}},
            })),
        )
        .await
        .expect_err("protocol mismatch");
    assert_eq!(err.status(), 400);
}

#[tokio::test]
async fn grpc_upstreams_take_grpc_match_keys() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(
            &ctx,
            upstream_input(json!({
                "server": {"endpoints": [{"scheme": "grpc", "host": "grpc.example.com"}]},
                "protocol": GRPC,
            })),
        )
        .await
        .expect("upstream");
    let route = h
        .cp
        .create_route(
            &ctx,
            route_input(json!({
                "upstream_id": upstream.id,
                "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}},
            })),
        )
        .await
        .expect("grpc route");
    assert_eq!(route.match_type, "grpc");
}

#[tokio::test]
async fn duplicate_match_rules_conflict_but_differing_priority_does_not() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");

    let body = json!({
        "upstream_id": upstream.id,
        "match": {"http": {"methods": ["GET", "POST"], "path": "/v1"}},
    });
    h.cp.create_route(&ctx, route_input(body.clone()))
        .await
        .expect("first");
    let err = h
        .cp
        .create_route(&ctx, route_input(body.clone()))
        .await
        .expect_err("same path + priority + method");
    assert_eq!(err.status(), 409);

    // A different priority disambiguates.
    let mut other = body.clone();
    other["priority"] = json!(10);
    h.cp.create_route(&ctx, route_input(other))
        .await
        .expect("different priority");

    // So does a disjoint method set.
    let mut other = body;
    other["match"]["http"]["methods"] = json!(["DELETE"]);
    h.cp.create_route(&ctx, route_input(other))
        .await
        .expect("disjoint methods");
}

#[tokio::test]
async fn route_upstream_id_is_immutable() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let a = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "a.example.com", 80)))
        .await
        .expect("a");
    let b = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "b.example.com", 80)))
        .await
        .expect("b");
    let route = h
        .cp
        .create_route(
            &ctx,
            route_input(json!({
                "upstream_id": a.id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .await
        .expect("route");

    let err = h
        .cp
        .replace_route(
            &ctx,
            route.id,
            route_input(json!({
                "upstream_id": b.id,
                "match": {"http": {"methods": ["GET"], "path": "/v1"}},
            })),
        )
        .await
        .expect_err("immutable");
    assert_eq!(err.status(), 400);

    // Omitting it, or repeating the current value, is fine.
    let replaced = h
        .cp
        .replace_route(
            &ctx,
            route.id,
            route_input(json!({
                "match": {"http": {"methods": ["GET", "POST"], "path": "/v1"}},
            })),
        )
        .await
        .expect("replaced");
    assert_eq!(replaced.upstream_id, a.id);
    assert_eq!(replaced.match_config.http.expect("http").methods.len(), 2);
}

// ---------------------------------------------------------------------------
// Plugin CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugins_are_named_uniquely_and_carry_a_gts_id() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let created = h
        .cp
        .create_plugin(
            &ctx,
            plugin_input(json!({
                "name": "redact_pii",
                "plugin_type": "transform",
                "phases": ["on_response"],
                "config_schema": {"type": "object"},
                "source_code": "def on_response(ctx):\n    return ctx.next()\n",
            })),
        )
        .await
        .expect("created");
    assert_eq!(
        created.gts_id(),
        format!("gts.cf.core.oagw.transform_plugin.v1~{}", created.id)
    );

    let err = h
        .cp
        .create_plugin(
            &ctx,
            plugin_input(json!({
                "name": "redact_pii",
                "plugin_type": "guard",
                "source_code": "x",
            })),
        )
        .await
        .expect_err("duplicate name");
    assert_eq!(err.status(), 409);
}

#[tokio::test]
async fn a_referenced_plugin_cannot_be_deleted() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let plugin = h
        .cp
        .create_plugin(
            &ctx,
            plugin_input(json!({
                "name": "validator",
                "plugin_type": "guard",
                "source_code": "def on_request(ctx):\n    return ctx.next()\n",
            })),
        )
        .await
        .expect("plugin");

    let mut body = simple_upstream(None, "api.example.com", 80);
    body["plugins"] = json!({"items": [plugin.gts_id()]});
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(body))
        .await
        .expect("upstream");

    let err = h
        .cp
        .delete_plugin(&ctx, plugin.id)
        .await
        .expect_err("in use");
    assert_eq!(err.status(), 409);
    let referenced = err.extensions.get("referenced_by").expect("referenced_by");
    assert_eq!(
        referenced["upstreams"][0],
        json!(gts_helpers::anonymous_id(
            gts_helpers::UPSTREAM_TYPE,
            upstream.id
        ))
    );

    h.cp.delete_upstream(&ctx, upstream.id).await.expect("unlink");
    h.cp.delete_plugin(&ctx, plugin.id).await.expect("now deletable");
}

#[tokio::test]
async fn an_unknown_plugin_reference_is_rejected_at_write_time() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let mut body = simple_upstream(None, "api.example.com", 80);
    body["plugins"] = json!({"items": [Uuid::new_v4().to_string()]});
    let err = h
        .cp
        .create_upstream(&ctx, upstream_input(body))
        .await
        .expect_err("dangling reference");
    assert_eq!(err.status(), 400);
}

// ---------------------------------------------------------------------------
// Alias resolution and route matching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn alias_resolution_is_case_insensitive() {
    let h = Harness::new();
    let tenant = Uuid::new_v4();
    let ctx = ctx_for(tenant);
    h.cp.create_upstream(&ctx, upstream_input(simple_upstream(None, "api.openai.com", 80)))
        .await
        .expect("upstream");

    let resolved = h
        .cp
        .resolve_alias(&ctx, "Api.OpenAI.COM")
        .await
        .expect("resolve")
        .expect("found");
    assert_eq!(resolved.selected.alias, "api.openai.com");
    assert!(resolved.enabled);
    assert!(h.cp.resolve_alias(&ctx, "nope").await.expect("resolve").is_none());
}

#[tokio::test]
async fn the_longest_matching_route_path_wins() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");
    for path in ["/v1", "/v1/chat/completions"] {
        h.cp.create_route(
            &ctx,
            route_input(json!({
                "upstream_id": upstream.id,
                "match": {"http": {"methods": ["POST"], "path": path}},
            })),
        )
        .await
        .expect("route");
    }

    let resolved = h
        .cp
        .resolve_alias(&ctx, "api.example.com")
        .await
        .expect("resolve")
        .expect("found");
    let target = h
        .cp
        .match_route(&resolved, "POST", "/v1/chat/completions")
        .await
        .expect("matched");
    assert_eq!(
        target.route.match_config.http.expect("http").path,
        "/v1/chat/completions"
    );

    let target = h
        .cp
        .match_route(&resolved, "POST", "/v1/models")
        .await
        .expect("matched");
    assert_eq!(target.route.match_config.http.expect("http").path, "/v1");
    assert_eq!(target.outbound_path, "/v1/models");
}

#[tokio::test]
async fn a_method_outside_the_allowlist_finds_no_route() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");
    h.cp.create_route(
        &ctx,
        route_input(json!({
            "upstream_id": upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        })),
    )
    .await
    .expect("route");

    let resolved = h
        .cp
        .resolve_alias(&ctx, "api.example.com")
        .await
        .expect("resolve")
        .expect("found");
    assert!(h.cp.match_route(&resolved, "GET", "/v1").await.is_ok());
    let err = h
        .cp
        .match_route(&resolved, "DELETE", "/v1")
        .await
        .expect_err("no route");
    assert_eq!(err.status(), 404);
}

#[tokio::test]
async fn a_disabled_route_is_excluded_from_matching() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");
    h.cp.create_route(
        &ctx,
        route_input(json!({
            "upstream_id": upstream.id,
            "enabled": false,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        })),
    )
    .await
    .expect("route");

    let resolved = h
        .cp
        .resolve_alias(&ctx, "api.example.com")
        .await
        .expect("resolve")
        .expect("found");
    assert_eq!(
        h.cp.match_route(&resolved, "GET", "/v1").await.unwrap_err().status(),
        404
    );
}

#[tokio::test]
async fn path_suffix_mode_disabled_rejects_a_suffix() {
    let h = Harness::new();
    let ctx = ctx_for(Uuid::new_v4());
    let upstream = h
        .cp
        .create_upstream(&ctx, upstream_input(simple_upstream(None, "api.example.com", 80)))
        .await
        .expect("upstream");
    h.cp.create_route(
        &ctx,
        route_input(json!({
            "upstream_id": upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1", "path_suffix_mode": "disabled"}},
        })),
    )
    .await
    .expect("route");

    let resolved = h
        .cp
        .resolve_alias(&ctx, "api.example.com")
        .await
        .expect("resolve")
        .expect("found");
    h.cp.match_route(&resolved, "GET", "/v1")
        .await
        .expect("exact path is fine");
    let err = h
        .cp
        .match_route(&resolved, "GET", "/v1/extra")
        .await
        .expect_err("suffix refused");
    assert_eq!(err.status(), 400);
}

// ---------------------------------------------------------------------------
// Hierarchy: shadowing, enabled inheritance, enforced limits
// ---------------------------------------------------------------------------

async fn hierarchy_harness() -> (Harness, Uuid, Uuid, SecurityContext, SecurityContext) {
    let leaf = Uuid::new_v4();
    let root = Uuid::new_v4();
    let h = Harness::with_directory(Arc::new(ScriptedDirectory(vec![leaf, root])));
    (h, leaf, root, ctx_for(leaf), ctx_for(root))
}

#[tokio::test]
async fn a_descendant_inherits_an_ancestor_alias() {
    let (h, _leaf, _root, leaf_ctx, root_ctx) = hierarchy_harness().await;
    let upstream = h
        .cp
        .create_upstream(&root_ctx, upstream_input(simple_upstream(None, "api.openai.com", 80)))
        .await
        .expect("root upstream");
    h.cp.create_route(
        &root_ctx,
        route_input(json!({
            "upstream_id": upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        })),
    )
    .await
    .expect("root route");

    let resolved = h
        .cp
        .resolve_alias(&leaf_ctx, "api.openai.com")
        .await
        .expect("resolve")
        .expect("inherited");
    assert_eq!(resolved.selected.id, upstream.id);
    h.cp.match_route(&resolved, "GET", "/v1")
        .await
        .expect("ancestor routes are inherited");

    // ...but the management API keeps ancestor resources invisible.
    assert_eq!(
        h.cp.get_upstream(&leaf_ctx, upstream.id).await.unwrap_err().status(),
        404
    );
}

#[tokio::test]
async fn the_closest_tenant_shadows_the_ancestor_alias() {
    let (h, _leaf, _root, leaf_ctx, root_ctx) = hierarchy_harness().await;
    h.cp.create_upstream(&root_ctx, upstream_input(simple_upstream(None, "api.openai.com", 80)))
        .await
        .expect("root upstream");
    let leaf_upstream = h
        .cp
        .create_upstream(&leaf_ctx, upstream_input(simple_upstream(None, "api.openai.com", 80)))
        .await
        .expect("leaf upstream");

    let resolved = h
        .cp
        .resolve_alias(&leaf_ctx, "api.openai.com")
        .await
        .expect("resolve")
        .expect("found");
    assert_eq!(resolved.selected.id, leaf_upstream.id);
    assert_eq!(resolved.chain_root_first.len(), 2);
}

#[tokio::test]
async fn an_ancestor_disable_propagates_and_cannot_be_re_enabled() {
    let (h, _leaf, _root, leaf_ctx, root_ctx) = hierarchy_harness().await;
    let mut disabled = simple_upstream(None, "api.openai.com", 80);
    disabled["enabled"] = json!(false);
    h.cp.create_upstream(&root_ctx, upstream_input(disabled))
        .await
        .expect("root upstream");
    // The descendant's own copy is enabled...
    h.cp.create_upstream(&leaf_ctx, upstream_input(simple_upstream(None, "api.openai.com", 80)))
        .await
        .expect("leaf upstream");

    let resolved = h
        .cp
        .resolve_alias(&leaf_ctx, "api.openai.com")
        .await
        .expect("resolve")
        .expect("found");
    // ...but the ancestor's disable still wins.
    assert!(!resolved.enabled);
}

#[tokio::test]
async fn enforced_ancestor_rate_limits_survive_shadowing() {
    let (h, _leaf, _root, leaf_ctx, root_ctx) = hierarchy_harness().await;
    let mut root_body = simple_upstream(None, "api.openai.com", 80);
    root_body["rate_limit"] = json!({
        "sharing": "enforce",
        "sustained": {"rate": 10000, "window": "minute"},
    });
    h.cp.create_upstream(&root_ctx, upstream_input(root_body))
        .await
        .expect("root upstream");

    let mut leaf_body = simple_upstream(None, "api.openai.com", 80);
    leaf_body["rate_limit"] = json!({"sustained": {"rate": 500, "window": "minute"}});
    let leaf_upstream = h
        .cp
        .create_upstream(&leaf_ctx, upstream_input(leaf_body))
        .await
        .expect("leaf upstream");
    h.cp.create_route(
        &leaf_ctx,
        route_input(json!({
            "upstream_id": leaf_upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        })),
    )
    .await
    .expect("route");

    let resolved = h
        .cp
        .resolve_alias(&leaf_ctx, "api.openai.com")
        .await
        .expect("resolve")
        .expect("found");
    let target = h.cp.match_route(&resolved, "GET", "/v1").await.expect("matched");
    let effective = target.effective.rate_limit.expect("rate limit");
    assert_eq!(effective.sustained.rate, 500, "min(enforced 10000, own 500)");
}

#[tokio::test]
async fn inherited_plugin_chains_prepend_the_ancestors() {
    let (h, _leaf, _root, leaf_ctx, root_ctx) = hierarchy_harness().await;
    let mut root_body = simple_upstream(None, "api.openai.com", 80);
    root_body["plugins"] = json!({
        "sharing": "inherit",
        "items": [gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID],
    });
    h.cp.create_upstream(&root_ctx, upstream_input(root_body))
        .await
        .expect("root upstream");

    let mut leaf_body = simple_upstream(None, "api.openai.com", 80);
    leaf_body["plugins"] = json!({"items": [gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID]});
    let leaf_upstream = h
        .cp
        .create_upstream(&leaf_ctx, upstream_input(leaf_body))
        .await
        .expect("leaf upstream");
    h.cp.create_route(
        &leaf_ctx,
        route_input(json!({
            "upstream_id": leaf_upstream.id,
            "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        })),
    )
    .await
    .expect("route");

    let resolved = h
        .cp
        .resolve_alias(&leaf_ctx, "api.openai.com")
        .await
        .expect("resolve")
        .expect("found");
    let target = h.cp.match_route(&resolved, "GET", "/v1").await.expect("matched");
    let refs: Vec<&str> = target
        .effective
        .plugins
        .iter()
        .map(|b| b.plugin_ref.as_str())
        .collect();
    assert_eq!(
        refs,
        vec![
            gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID,
            gts_helpers::REQUEST_ID_TRANSFORM_PLUGIN_ID
        ]
    );
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

#[test]
fn path_normalisation_and_prefix_matching() {
    assert_eq!(normalize_path("v1/models"), "/v1/models");
    assert_eq!(normalize_path("/v1/models/"), "/v1/models");
    assert_eq!(normalize_path("/"), "/");

    assert!(path_matches("/v1/models", "/v1"));
    assert!(path_matches("/v1", "/v1"));
    assert!(path_matches("/anything", "/"));
    // A prefix must end on a segment boundary.
    assert!(!path_matches("/v10/models", "/v1"));
    assert!(!path_matches("/v2", "/v1"));
}
