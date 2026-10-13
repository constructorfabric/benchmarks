#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The turn mutations as wired by the gear
//! (`cpt-cf-mini-chat-feature-turn-mutations`,
//! `cpt-cf-mini-chat-dod-turn-mutations-rest-routes`): the three routes
//! registered by `register_rest` under `url_prefix` behind the license gate, a
//! delete through the gear's own PEP, database and outbox, and the mutation
//! audit event reaching the audit plugin through the started pipeline.

use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use mini_chat_sdk::{
    MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    TurnMutationAuditEvent,
};
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
use crate::chat_test_env::{blob, chat_id, request_id, ts};
use crate::domain::service::quota::test_support::design_limits;
use crate::infra::db::test_support::{TestDb, chat_row, message_row, turn_row};
use crate::infra::llm::oagw_provisioning::ProvisioningTimeouts;
use crate::infra::test_support::FixedConfig;
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, EmitBehavior, FakeAuditPlugin, FakeAuthn, FakeOagw, FakePdp,
    FakePolicyPlugin, POLICY_VERSION, PdpReply, PluginWorld, USER, USER_TENANT, entry, entry_json,
    user_ctx,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";

struct World {
    db: TestDb,
    gear: MiniChatGear,
    router: Router,
    openapi: toolkit::api::OpenApiRegistryImpl,
    audit: Arc<FakeAuditPlugin>,
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
    async fn new(extra: &Value, started: bool) -> Self {
        let plugins = PluginWorld::new();
        let mut model = entry_json("model-a", true);
        model["provider_id"] = json!("primary");
        let policy_plugin = FakePolicyPlugin::with_catalog(vec![entry(model)]);
        policy_plugin.set_limits(Some(design_limits(POLICY_VERSION)));
        plugins.register_policy(
            Arc::clone(&policy_plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>
        );
        let audit = FakeAuditPlugin::new(EmitBehavior::Ok);
        plugins.register_audit(Arc::clone(&audit) as Arc<dyn MiniChatAuditPluginClientV1>);
        plugins.hub.register::<dyn AuthZResolverApi>(
            FakePdp::new(PdpReply::AllowScoped) as Arc<dyn AuthZResolverApi>
        );
        plugins.hub.register::<dyn ServiceGatewayClientV1>(
            FakeOagw::new() as Arc<dyn ServiceGatewayClientV1>
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
            db,
            gear,
            router,
            openapi,
            audit,
        }
    }

    async fn call(&self, method: &str, uri: &str, ctx: &SecurityContext) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// A chat with a completed turn (question and answer); returns the chat and
    /// the request id.
    async fn chat_with_turn(&self) -> (Uuid, Uuid) {
        let chat = chat_id(1);
        self.db
            .insert(
                "chats",
                chat_row(chat, USER_TENANT, USER).set("model", "model-a"),
            )
            .await
            .unwrap();
        let request = request_id(1);
        let assistant = Uuid::from_u128(0xD000_0002);
        self.db
            .insert(
                "chat_turns",
                turn_row(
                    Uuid::from_u128(0xA000_0001),
                    USER_TENANT,
                    chat,
                    request,
                    "completed",
                )
                .set("requester_user_id", USER)
                .set("assistant_message_id", assistant)
                .set("started_at", ts(10)),
            )
            .await
            .unwrap();
        for (id, role, secs) in [
            (Uuid::from_u128(0xD000_0001), "user", 10),
            (assistant, "assistant", 11),
        ] {
            self.db
                .insert(
                    "messages",
                    message_row(id, USER_TENANT, chat)
                        .set("role", role)
                        .set("request_id", request)
                        .set("created_at", ts(secs)),
                )
                .await
                .unwrap();
        }
        (chat, request)
    }
}

fn keys(world: &World) -> Vec<String> {
    let mut keys: Vec<String> = world
        .openapi
        .operation_specs
        .iter()
        .map(|e| e.key().clone())
        .collect();
    keys.sort();
    keys
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

// ---- registration -----------------------------------------------------------------------------------

#[tokio::test]
async fn the_gear_registers_the_three_turn_mutation_routes_behind_the_license_gate() {
    let world = World::new(&json!({}), false).await;

    let keys = keys(&world);

    for (key, operation) in [
        (
            "POST:/mini-chat/v1/chats/{id}/turns/{request_id}/retry",
            "mini_chat.retry_turn",
        ),
        (
            "PATCH:/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.edit_turn",
        ),
        (
            "DELETE:/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.delete_turn",
        ),
    ] {
        assert!(keys.contains(&key.to_owned()), "{key} in {keys:?}");
        let spec = world
            .openapi
            .operation_specs
            .get(key)
            .unwrap()
            .value()
            .clone();
        assert!(spec.authenticated, "{key}");
        assert_eq!(spec.operation_id.as_deref(), Some(operation));
        assert_eq!(
            spec.license_requirement.unwrap().license_names,
            vec![BASE_LICENSE.to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_turn_mutation_routes_follow_the_configured_url_prefix() {
    let world = World::new(&json!({ "url_prefix": "/chat-api" }), false).await;

    let keys = keys(&world);

    assert!(
        keys.contains(&"DELETE:/chat-api/v1/chats/{id}/turns/{request_id}".to_owned()),
        "{keys:?}"
    );
    assert!(
        keys.iter()
            .filter(|key| key.contains("/turns/"))
            .all(|key| !key.contains("/mini-chat/")),
        "{keys:?}"
    );
}

// ---- the wired service ------------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_turn_is_served_as_a_404_of_the_turn_type_by_the_wired_service() {
    let world = World::new(&json!({}), false).await;
    let (chat, _) = world.chat_with_turn().await;

    let (status, body) = world
        .call(
            "DELETE",
            &format!(
                "/mini-chat/v1/chats/{chat}/turns/{}",
                Uuid::from_u128(0xDEAD)
            ),
            &user_ctx(),
        )
        .await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.turn.v1~"
    );
}

#[tokio::test]
async fn a_delete_through_the_gear_soft_deletes_the_turn_and_the_audit_event_reaches_the_plugin() {
    let world = World::new(&json!({}), true).await;
    let (chat, request) = world.chat_with_turn().await;

    let (status, body) = world
        .call(
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/turns/{request}"),
            &user_ctx(),
        )
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_ne!(
        world
            .db
            .query_string(&format!(
                "SELECT COALESCE(deleted_at, 'NULL') FROM chat_turns WHERE request_id = {}",
                blob(request)
            ))
            .await,
        "NULL"
    );
    wait_until("the mutation audit event", || {
        !world.audit.emitted_events().is_empty()
    })
    .await;
    let events = world.audit.emitted_events();
    match events.as_slice() {
        [
            MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
                tenant_id,
                actor_user_id,
                chat_id: audited_chat,
                request_id: audited_request,
                ..
            }),
        ] => {
            assert_eq!(*tenant_id, USER_TENANT);
            assert_eq!(*actor_user_id, USER);
            assert_eq!(*audited_chat, chat);
            assert_eq!(*audited_request, request);
        }
        other => panic!("one turn_delete event expected, got {other:?}"),
    }
    world
        .gear
        .stop(CancellationToken::new())
        .await
        .expect("stop");
}

#[tokio::test]
async fn a_second_delete_through_the_gear_is_a_409_not_the_latest_turn() {
    let world = World::new(&json!({}), true).await;
    let (chat, request) = world.chat_with_turn().await;
    let uri = format!("/mini-chat/v1/chats/{chat}/turns/{request}");
    world.call("DELETE", &uri, &user_ctx()).await;

    let (status, body) = world.call("DELETE", &uri, &user_ctx()).await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["context"]["reason"], "NOT_LATEST_TURN");
    world
        .gear
        .stop(CancellationToken::new())
        .await
        .expect("stop");
}
