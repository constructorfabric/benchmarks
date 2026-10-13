#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The chat CRUD routes as wired by the gear (`cpt-cf-mini-chat-feature-chat-crud`,
//! `cpt-cf-mini-chat-dod-chat-crud-rest-routes`): registered by `register_rest`
//! under `url_prefix`, behind the gear's own PEP, database and outbox, with a
//! fake security context, fake PDP, fake plugins and a `SQLite` database.

use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1};
use oagw_sdk::ServiceGatewayClientV1;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use toolkit::context::GearCtx;
use toolkit::contracts::RunnableCapability;
use toolkit::{Gear, RestApiCapability};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use super::MiniChatGear;
use crate::chat_test_env::{chat_id, msg_id, request_id, ts};
use crate::domain::service::quota::test_support::design_limits;
use crate::infra::db::test_support::{TestDb, message_row};
use crate::infra::llm::oagw_provisioning::ProvisioningTimeouts;
use crate::infra::test_support::FixedConfig;
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, EmitBehavior, FakeAuditPlugin, FakeAuthn, FakeOagw, FakePdp,
    FakePolicyPlugin, POLICY_VERSION, PdpReply, PluginWorld, USER, USER_TENANT, entry, entry_json,
    other_tenant_ctx, other_user_ctx, user_ctx,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

struct World {
    pdp: Arc<FakePdp>,
    oagw: Arc<FakeOagw>,
    audit: Arc<FakeAuditPlugin>,
    policy: Arc<FakePolicyPlugin>,
    db: TestDb,
    gear: MiniChatGear,
    router: Router,
    openapi: toolkit::api::OpenApiRegistryImpl,
}

fn config(extra: &Value) -> Value {
    let mut config = json!({
        "client_credentials": { "client_id": "mini-chat-client", "client_secret": "S3CRET" },
        "providers": { "primary": {
            "kind": "openai_responses",
            "host": "api.openai.com",
            "storage_kind": "openai",
            "auth_plugin_type": APIKEY_PLUGIN,
            "auth_config": { "header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://x" }
        } },
    });
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    config
}

impl World {
    /// Initialized gear with the routes registered; the outbox is started only
    /// when `started` is true.
    async fn new(extra: &Value, started: bool) -> Self {
        let plugins = PluginWorld::new();
        let policy_plugin = FakePolicyPlugin::with_catalog(vec![
            entry(entry_json("gpt-4.1-mini", true)),
            entry(entry_json("model-a", true)),
        ]);
        policy_plugin.set_limits(Some(design_limits(POLICY_VERSION)));
        plugins.register_policy(
            Arc::clone(&policy_plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>
        );
        let audit_plugin = FakeAuditPlugin::new(EmitBehavior::Ok);
        plugins.register_audit(Arc::clone(&audit_plugin) as Arc<dyn MiniChatAuditPluginClientV1>);
        let pdp = FakePdp::new(PdpReply::AllowScoped);
        plugins
            .hub
            .register::<dyn AuthZResolverApi>(Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>);
        let oagw = FakeOagw::new();
        plugins.hub.register::<dyn ServiceGatewayClientV1>(
            Arc::clone(&oagw) as Arc<dyn ServiceGatewayClientV1>
        );
        plugins.hub.register::<dyn AuthNResolverClient>(
            FakeAuthn::new(AuthnReply::Ok) as Arc<dyn AuthNResolverClient>
        );
        let db = TestDb::migrated().await;
        let ctx = GearCtx::new(
            "mini-chat",
            Uuid::nil(),
            Arc::new(FixedConfig::with_section("mini-chat", config(extra))),
            Arc::clone(&plugins.hub),
            CancellationToken::new(),
        )
        .with_db(DBProvider::new(db.db.clone()));
        let gear = MiniChatGear::default().with_provisioning_timeouts(ProvisioningTimeouts {
            s2s_exchange: Duration::from_millis(150),
            oagw_call: Duration::from_millis(150),
        });
        gear.init(&ctx).await.expect("init");
        if started {
            gear.start(CancellationToken::new()).await.expect("start");
        }
        let openapi = toolkit::api::OpenApiRegistryImpl::new();
        let router = gear
            .register_rest(&ctx, Router::new(), &openapi)
            .expect("routes");
        Self {
            pdp,
            oagw,
            audit: audit_plugin,
            policy: policy_plugin,
            db,
            gear,
            router,
            openapi,
        }
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<&Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let mut request = builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn stop(self) {
        self.gear
            .stop(CancellationToken::new())
            .await
            .expect("stop");
    }
}

fn operation_keys(world: &World) -> Vec<String> {
    let mut keys: Vec<String> = world
        .openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();
    keys.sort();
    keys
}

// ---- registration ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_gear_registers_the_chat_quota_streaming_and_turn_mutation_routes_next_to_the_models_routes()
 {
    let world = World::new(&json!({}), false).await;

    // The attachment routes have their own registration tests
    // (`gear_attachments_tests`, `routes_attachments_registration_tests`).
    let keys: Vec<String> = operation_keys(&world)
        .into_iter()
        .filter(|key| !key.contains("/attachments"))
        .collect();

    let mut expected = vec![
        "GET:/mini-chat/v1/models",
        "GET:/mini-chat/v1/models/{id}",
        "POST:/mini-chat/v1/chats",
        "GET:/mini-chat/v1/chats",
        "GET:/mini-chat/v1/chats/{id}",
        "PATCH:/mini-chat/v1/chats/{id}",
        "DELETE:/mini-chat/v1/chats/{id}",
        "GET:/mini-chat/v1/chats/{id}/messages",
        "PUT:/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
        "DELETE:/mini-chat/v1/chats/{id}/messages/{msg_id}/reaction",
        "GET:/mini-chat/v1/quota/status",
        "POST:/mini-chat/v1/chats/{id}/messages:stream",
        "GET:/mini-chat/v1/chats/{id}/turns/{request_id}",
        "POST:/mini-chat/v1/chats/{id}/turns/{request_id}/retry",
        "PATCH:/mini-chat/v1/chats/{id}/turns/{request_id}",
        "DELETE:/mini-chat/v1/chats/{id}/turns/{request_id}",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected);
    for key in keys {
        let spec = world
            .openapi
            .operation_specs
            .get(&key)
            .unwrap()
            .value()
            .clone();
        assert!(spec.authenticated, "{key}");
        assert_eq!(
            spec.license_requirement.unwrap().license_names,
            vec![BASE_LICENSE.to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_chat_routes_follow_the_configured_url_prefix() {
    let world = World::new(&json!({ "url_prefix": "/chat-api" }), false).await;

    let keys = operation_keys(&world);
    let (served, headers, body) = world
        .call("POST", "/chat-api/v1/chats", &user_ctx(), Some(&json!({})))
        .await;
    let (elsewhere, _, _) = world
        .call("POST", "/mini-chat/v1/chats", &user_ctx(), Some(&json!({})))
        .await;

    assert!(
        keys.contains(&"POST:/chat-api/v1/chats".to_owned()),
        "{keys:?}"
    );
    assert!(keys.iter().all(|k| !k.contains("/mini-chat/")), "{keys:?}");
    assert_eq!(served, StatusCode::CREATED, "{body}");
    assert_eq!(
        headers.get("location").unwrap().to_str().unwrap(),
        format!("/chat-api/v1/chats/{}", body["id"].as_str().unwrap())
    );
    assert_eq!(elsewhere, StatusCode::NOT_FOUND);
}

// ---- the lifecycle through the gear ----------------------------------------------------------------------------

#[tokio::test]
async fn a_chat_lives_through_create_rename_react_list_and_delete_behind_the_gear() {
    let world = World::new(&json!({}), true).await;
    let ctx = user_ctx();

    let (status, headers, created) = world
        .call(
            "POST",
            "/mini-chat/v1/chats",
            &ctx,
            Some(&json!({ "title": "  Plan  " })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["title"], "Plan");
    assert_eq!(
        created["model"], "gpt-4.1-mini",
        "the first enabled default entry"
    );
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    assert_eq!(
        headers.get("location").unwrap().to_str().unwrap(),
        format!("/mini-chat/v1/chats/{id}")
    );
    // A turn of the streaming feature is simulated with seeded rows.
    world
        .db
        .insert(
            "messages",
            message_row(msg_id(1), USER_TENANT, id)
                .set("role", "user")
                .set("request_id", request_id(1))
                .set("created_at", ts(1)),
        )
        .await
        .unwrap();
    world
        .db
        .insert(
            "messages",
            message_row(msg_id(2), USER_TENANT, id)
                .set("role", "assistant")
                .set("request_id", request_id(1))
                .set("model", "gpt-4.1-mini")
                .set("created_at", ts(2)),
        )
        .await
        .unwrap();

    let (status, _, got) = world
        .call("GET", &format!("/mini-chat/v1/chats/{id}"), &ctx, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["message_count"], 2);
    let (status, _, renamed) = world
        .call(
            "PATCH",
            &format!("/mini-chat/v1/chats/{id}"),
            &ctx,
            Some(&json!({ "title": "Renamed", "model": "model-a" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{renamed}");
    assert_eq!(renamed["title"], "Renamed");
    assert_eq!(renamed["model"], "gpt-4.1-mini");
    let reaction_uri = format!("/mini-chat/v1/chats/{id}/messages/{}/reaction", msg_id(2));
    let (status, _, reaction) = world
        .call(
            "PUT",
            &reaction_uri,
            &ctx,
            Some(&json!({ "reaction": "like" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{reaction}");
    let (status, _, messages) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{id}/messages"),
            &ctx,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{messages}");
    assert_eq!(messages["items"][0]["my_reaction"], Value::Null);
    assert_eq!(messages["items"][1]["my_reaction"], "like");
    let (status, _, list) = world.call("GET", "/mini-chat/v1/chats", &ctx, None).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["items"][0]["id"], id.to_string());
    assert_eq!(list["items"][0]["message_count"], 2);

    let (status, _, _) = world.call("DELETE", &reaction_uri, &ctx, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = world
        .call("DELETE", &format!("/mini-chat/v1/chats/{id}"), &ctx, None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, gone) = world
        .call("GET", &format!("/mini-chat/v1/chats/{id}"), &ctx, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{gone}");
    let (status, _, list) = world.call("GET", "/mini-chat/v1/chats", &ctx, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["items"], json!([]));
    world.stop().await;
}

#[tokio::test]
async fn deleting_a_chat_enqueues_one_message_on_the_chat_cleanup_queue_of_the_started_pipeline() {
    let world = World::new(&json!({}), true).await;
    let (_, _, created) = world
        .call("POST", "/mini-chat/v1/chats", &user_ctx(), Some(&json!({})))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, _, _) = world
        .call(
            "DELETE",
            &format!("/mini-chat/v1/chats/{id}"),
            &user_ctx(),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT);
    let count = world
        .db
        .query_i64(
            "SELECT COUNT(DISTINCT b.id) FROM mini_chat__outbox_body b \
             JOIN (SELECT partition_id, body_id FROM mini_chat__outbox_incoming \
                   UNION ALL SELECT partition_id, body_id FROM mini_chat__outbox_outgoing) m \
               ON m.body_id = b.id \
             JOIN mini_chat__outbox_partitions p ON p.id = m.partition_id \
             WHERE p.queue = 'mini-chat.chat_cleanup'",
        )
        .await;
    assert_eq!(count, 1);
    world.stop().await;
}

#[tokio::test]
async fn deleting_before_the_outbox_is_started_is_500_and_keeps_the_chat() {
    let world = World::new(&json!({}), false).await;
    let (_, _, created) = world
        .call("POST", "/mini-chat/v1/chats", &user_ctx(), Some(&json!({})))
        .await;
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, _, body) = world
        .call(
            "DELETE",
            &format!("/mini-chat/v1/chats/{id}"),
            &user_ctx(),
            None,
        )
        .await;
    let (after, _, _) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{id}"),
            &user_ctx(),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(after, StatusCode::OK);
}

// ---- isolation, PEP --------------------------------------------------------------------------------------------

#[tokio::test]
async fn chats_are_isolated_by_user_and_tenant_through_the_gear_router() {
    let world = World::new(&json!({}), false).await;
    let (_, _, created) = world
        .call(
            "POST",
            "/mini-chat/v1/chats",
            &user_ctx(),
            Some(&json!({ "title": "mine" })),
        )
        .await;
    let id = created["id"].as_str().unwrap().to_owned();
    let uri = format!("/mini-chat/v1/chats/{id}");

    for ctx in [other_user_ctx(), other_tenant_ctx()] {
        let (get, _, body) = world.call("GET", &uri, &ctx, None).await;
        let (patch, _, _) = world
            .call("PATCH", &uri, &ctx, Some(&json!({ "title": "x" })))
            .await;
        let (delete, _, _) = world.call("DELETE", &uri, &ctx, None).await;
        let (messages, _, _) = world
            .call("GET", &format!("{uri}/messages"), &ctx, None)
            .await;
        let (list, _, listed) = world.call("GET", "/mini-chat/v1/chats", &ctx, None).await;

        assert_eq!(get, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(patch, StatusCode::NOT_FOUND);
        assert_eq!(delete, StatusCode::NOT_FOUND);
        assert_eq!(messages, StatusCode::NOT_FOUND);
        assert_eq!(list, StatusCode::OK);
        assert_eq!(listed["items"], json!([]));
    }
    let (status, _, got) = world.call("GET", &uri, &user_ctx(), None).await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["title"], "mine");
}

#[tokio::test]
async fn the_chat_operations_use_the_authz_resolver_client_of_the_hub() {
    let world = World::new(&json!({}), false).await;

    world
        .call("POST", "/mini-chat/v1/chats", &user_ctx(), Some(&json!({})))
        .await;
    world
        .call("GET", "/mini-chat/v1/chats", &user_ctx(), None)
        .await;
    world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{}", chat_id(1)),
            &user_ctx(),
            None,
        )
        .await;

    let actions: Vec<String> = world
        .pdp
        .requests()
        .iter()
        .map(|r| r.action.name.clone())
        .collect();
    assert_eq!(actions, vec!["create", "list", "read"]);
    for request in world.pdp.requests() {
        assert_eq!(
            request.resource.resource_type,
            "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
        );
    }
}

// ---- the quota status through the gear ----------------------------------------------------------------------------

#[tokio::test]
async fn the_gear_serves_the_quota_status_over_its_own_policy_gateway_database_and_pep() {
    let world = World::new(&json!({}), false).await;
    let today = time::OffsetDateTime::now_utc().date();
    let month = today.replace_day(1).unwrap();
    for (period, start, spent, reserved) in [
        ("daily", today, 10_000_000_i64, 5_000_000_i64),
        ("monthly", month, 100_000_000, 0),
    ] {
        world
            .db
            .insert(
                "quota_usage",
                crate::infra::db::test_support::Row::default()
                    .set("id", Uuid::new_v4())
                    .set("tenant_id", USER_TENANT)
                    .set("user_id", USER)
                    .set("period_type", period)
                    .set("period_start", start.to_string())
                    .set("bucket", "total")
                    .set("spent_credits_micro", spent)
                    .set("reserved_credits_micro", reserved)
                    .set("updated_at", crate::infra::db::test_support::TS),
            )
            .await
            .unwrap();
    }

    let (status, _, body) = world
        .call("GET", "/mini-chat/v1/quota/status", &user_ctx(), None)
        .await;
    let (foreign_status, _, foreign) = world
        .call(
            "GET",
            "/mini-chat/v1/quota/status",
            &other_tenant_ctx(),
            None,
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["warning_threshold_pct"], 80);
    assert_eq!(body["tiers"][0]["tier"], "premium");
    assert_eq!(body["tiers"][1]["tier"], "total");
    if time::OffsetDateTime::now_utc().date() == today {
        let daily = &body["tiers"][1]["periods"][0];
        assert_eq!(daily["limit_credits_micro"], 60_000_000);
        assert_eq!(daily["used_credits_micro"], 15_000_000);
        assert_eq!(daily["remaining_percentage"], 75);
        let monthly = &body["tiers"][1]["periods"][1];
        assert_eq!(monthly["used_credits_micro"], 100_000_000);
        assert_eq!(body["tiers"][0]["periods"][0]["used_credits_micro"], 0);
        assert_eq!(foreign_status, StatusCode::OK, "{foreign}");
        assert_eq!(foreign["tiers"][1]["periods"][0]["used_credits_micro"], 0);
    }
}

#[tokio::test]
async fn the_quota_status_follows_the_configured_url_prefix() {
    let world = World::new(&json!({ "url_prefix": "/chat-api" }), false).await;

    let (served, _, body) = world
        .call("GET", "/chat-api/v1/quota/status", &user_ctx(), None)
        .await;
    let (elsewhere, _, _) = world
        .call("GET", "/mini-chat/v1/quota/status", &user_ctx(), None)
        .await;

    assert_eq!(served, StatusCode::OK, "{body}");
    assert_eq!(elsewhere, StatusCode::NOT_FOUND);
}

// ---- a turn through the gear ------------------------------------------------------------------------------------

fn openai_provider() -> Value {
    json!({ "providers": { "openai": {
        "kind": "openai_responses",
        "host": "api.openai.com",
        "storage_kind": "openai",
        "auth_plugin_type": APIKEY_PLUGIN,
        "auth_config": { "header": "Authorization", "prefix": "Bearer ", "secret_ref": "cred://x" }
    } } })
}

/// The raw body of a response (the stream must end).
async fn post_stream(
    world: &World,
    chat: Uuid,
    content: &str,
    request: Uuid,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/messages:stream"))
        .header("content-type", "application/json")
        .body(Body::from(
            json!({ "content": content, "request_id": request }).to_string(),
        ))
        .unwrap();
    request.extensions_mut().insert(user_ctx());
    let response = world.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = tokio::time::timeout(
        Duration::from_secs(20),
        to_bytes(response.into_body(), 4 << 20),
    )
    .await
    .expect("the stream ended")
    .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn frame_names(text: &str) -> Vec<String> {
    text.split("\n\n")
        .filter_map(|block| block.lines().find_map(|l| l.strip_prefix("event: ")))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_message_streams_through_the_gear_commits_the_turn_and_delivers_both_outbox_events() {
    use crate::test_doubles::ProxyReply;
    let world = World::new(&openai_provider(), true).await;
    let ctx = user_ctx();
    let (status, _, created) = world
        .call(
            "POST",
            "/mini-chat/v1/chats",
            &ctx,
            Some(&json!({ "title": "t" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let chat = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    world.oagw.push_proxy(ProxyReply::sse(&[
        (
            "response.output_text.delta",
            json!({ "type": "response.output_text.delta", "delta": "Hello" }),
        ),
        (
            "response.completed",
            json!({ "response": { "id": "resp_1", "usage": { "input_tokens": 12, "output_tokens": 3 } } }),
        ),
    ]));
    let request = request_id(77);
    let (status, headers, text) = post_stream(&world, chat, "Hi", request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(
        frame_names(&text),
        ["stream_started", "delta", "done"],
        "{text}"
    );

    // The provider was reached through OAGW at the provisioned alias.
    let proxied = world.oagw.proxied();
    assert_eq!(proxied.len(), 1);
    assert_eq!(proxied[0].uri, "/api.openai.com/v1/responses");
    assert_eq!(proxied[0].json()["model"], "provider-gpt-4.1-mini");

    // The turn is committed and readable through the other routes.
    let (status, _, turn) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{chat}/turns/{request}"),
            &ctx,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{turn}");
    assert_eq!(turn["state"], "done");
    let (_, _, page) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{chat}/messages"),
            &ctx,
            None,
        )
        .await;
    let roles: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["user", "assistant"]);
    assert_eq!(page["items"][1]["content"], "Hello");

    // The usage and the audit events reach the plugins through the real outbox
    // handlers.
    for _ in 0..400 {
        if !world.policy.published_events().is_empty() && !world.audit.emitted_events().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(world.policy.published_events().len(), 1, "one usage event");
    assert_eq!(world.audit.emitted_events().len(), 1, "one audit event");

    // The same request id replays without a provider call.
    let (status, _, replay) = post_stream(&world, chat, "ignored", request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(replay.contains("\"is_new_turn\":false"), "{replay}");
    assert_eq!(world.oagw.proxy_call_count(), 1);
    world.stop().await;
}

#[tokio::test]
async fn a_send_through_the_gear_to_a_chat_of_another_user_is_a_404_and_reaches_no_provider() {
    let world = World::new(&openai_provider(), true).await;
    let (_, _, created) = world
        .call("POST", "/mini-chat/v1/chats", &user_ctx(), Some(&json!({})))
        .await;
    let chat = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let mut request = Request::builder()
        .method("POST")
        .uri(format!("/mini-chat/v1/chats/{chat}/messages:stream"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "content": "Hi" }).to_string()))
        .unwrap();
    request.extensions_mut().insert(other_user_ctx());
    let response = world.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(world.oagw.proxy_call_count(), 0);
    world.stop().await;
}
