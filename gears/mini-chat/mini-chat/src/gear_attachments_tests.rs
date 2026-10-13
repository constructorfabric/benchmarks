#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The attachments feature as wired by the gear
//! (`cpt-cf-mini-chat-feature-attachments`, `cpt-cf-mini-chat-dod-attachments-rest-routes`,
//! `-cleanup-handler`, `-upload-reaper`): the routes registered by `register_rest`
//! under `url_prefix`, the upload through the gear's own PEP, database and OAGW
//! client, the cleanup handler on the started outbox pipeline, and the reaper
//! spawned on the gear's background registry.

use std::sync::Arc;
use std::time::Duration;

use authn_resolver_sdk::AuthNResolverClient;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
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
use crate::attachment_test_env::{BOUNDARY, file_part, multipart_body, multipart_content_type};
use crate::chat_test_env::{blob, chat_id, ts};
use crate::domain::service::quota::test_support::design_limits;
use crate::infra::db::test_support::{Row, TestDb, attachment_row, chat_row};
use crate::infra::llm::oagw_provisioning::ProvisioningTimeouts;
use crate::infra::test_support::FixedConfig;
use crate::infra::workers::upload_reaper::UPLOAD_REAPER_TASK;
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, EmitBehavior, FakeAuditPlugin, FakeAuthn, FakeOagw, FakePdp,
    FakePolicyPlugin, POLICY_VERSION, PdpReply, PluginWorld, ProxyReply, S2S_SUBJECT, USER,
    USER_TENANT, entry, entry_json, user_ctx,
};

const BASE_LICENSE: &str = "gts.cf.core.lic.feat.v1~cf.core.global.base.v1";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

struct World {
    oagw: Arc<FakeOagw>,
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
    async fn new(extra: &Value, started: bool) -> Self {
        let plugins = PluginWorld::new();
        let mut model = entry_json("model-a", true);
        model["provider_id"] = json!("primary");
        let policy_plugin = FakePolicyPlugin::with_catalog(vec![entry(model)]);
        policy_plugin.set_limits(Some(design_limits(POLICY_VERSION)));
        plugins.register_policy(
            Arc::clone(&policy_plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>
        );
        let audit_plugin = FakeAuditPlugin::new(EmitBehavior::Ok);
        plugins.register_audit(audit_plugin as Arc<dyn MiniChatAuditPluginClientV1>);
        let pdp = FakePdp::new(PdpReply::AllowScoped);
        plugins
            .hub
            .register::<dyn AuthZResolverApi>(pdp as Arc<dyn AuthZResolverApi>);
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
            oagw,
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
        content_type: Option<&str>,
        body: Body,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let mut request = builder.body(body).unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn chat(&self) -> Uuid {
        let id = chat_id(1);
        self.db
            .insert(
                "chats",
                chat_row(id, USER_TENANT, USER).set("model", "model-a"),
            )
            .await
            .unwrap();
        id
    }

    async fn seed_attachment(&self, chat: Uuid, n: u128, edit: impl FnOnce(Row) -> Row) -> Uuid {
        let id = Uuid::from_u128(0xF100_0000 + n);
        let row = edit(
            attachment_row(id, USER_TENANT, chat)
                .set("uploaded_by_user_id", USER)
                .set("provider_file_id", format!("file-prov{n}"))
                .set("created_at", ts(0))
                .set("updated_at", ts(0)),
        );
        self.db.insert("attachments", row).await.unwrap();
        id
    }

    async fn att_text(&self, id: Uuid, column: &str) -> String {
        self.db
            .query_string(&format!(
                "SELECT COALESCE({column}, 'NULL') FROM attachments WHERE id = {}",
                blob(id)
            ))
            .await
    }

    async fn stop(self) {
        self.gear
            .stop(CancellationToken::new())
            .await
            .expect("stop");
    }
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

// ---- registration -----------------------------------------------------------------------------------

#[tokio::test]
async fn the_gear_registers_the_three_attachment_routes_behind_the_license_gate() {
    let world = World::new(&json!({}), false).await;

    let keys = keys(&world);

    for key in [
        "POST:/mini-chat/v1/chats/{id}/attachments",
        "GET:/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
        "DELETE:/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
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
        assert_eq!(
            spec.license_requirement.unwrap().license_names,
            vec![BASE_LICENSE.to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_attachment_routes_are_served_by_the_wired_service() {
    let world = World::new(&json!({}), false).await;
    let chat = world.chat().await;
    let ctx = user_ctx();

    let (get, get_body) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{chat}/attachments/{}", Uuid::new_v4()),
            &ctx,
            None,
            Body::empty(),
        )
        .await;
    let (delete, delete_body) = world
        .call(
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/attachments/{}", Uuid::new_v4()),
            &ctx,
            None,
            Body::empty(),
        )
        .await;
    let (post, post_body) = world
        .call(
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/attachments"),
            &ctx,
            Some("application/json"),
            Body::from("{}"),
        )
        .await;

    assert_eq!(get, StatusCode::NOT_FOUND, "{get_body}");
    assert_eq!(
        get_body["context"]["resource_type"],
        "gts.cf.core.mini_chat.attachment.v1~"
    );
    assert_eq!(delete, StatusCode::NOT_FOUND, "{delete_body}");
    assert_eq!(
        delete_body["context"]["resource_type"],
        "gts.cf.core.mini_chat.attachment.v1~"
    );
    assert_eq!(post, StatusCode::BAD_REQUEST, "{post_body}");
    assert_eq!(
        post_body["context"]["field_violations"][0]["reason"],
        "BOUNDARY_REQUIRED"
    );
}

#[tokio::test]
async fn the_attachment_routes_follow_the_configured_url_prefix() {
    let world = World::new(&json!({ "url_prefix": "/chat-api" }), false).await;
    let chat = world.chat().await;

    let (served, body) = world
        .call(
            "GET",
            &format!("/chat-api/v1/chats/{chat}/attachments/{}", Uuid::new_v4()),
            &user_ctx(),
            None,
            Body::empty(),
        )
        .await;
    let (elsewhere, _) = world
        .call(
            "GET",
            &format!("/mini-chat/v1/chats/{chat}/attachments/{}", Uuid::new_v4()),
            &user_ctx(),
            None,
            Body::empty(),
        )
        .await;

    assert_eq!(served, StatusCode::NOT_FOUND);
    assert_eq!(
        body["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~",
        "{body}"
    );
    assert_eq!(elsewhere, StatusCode::NOT_FOUND);
    assert!(keys(&world).iter().all(|k| !k.contains("/mini-chat/")));
}

// ---- an upload through the gear ----------------------------------------------------------------------------

#[tokio::test]
async fn a_spreadsheet_is_uploaded_through_the_gear_to_the_provider_entry() {
    let world = World::new(&json!({}), true).await;
    let chat = world.chat().await;
    world.oagw.on_path("POST", "/api.openai.com/v1/files", |_| {
        ProxyReply::json(200, &json!({ "id": "file-gear-1" }))
    });
    let body = multipart_body(BOUNDARY, &[file_part("sheet.xlsx", XLSX, b"PK cells")]);

    let (status, json) = world
        .call(
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/attachments"),
            &user_ctx(),
            Some(&multipart_content_type(BOUNDARY)),
            Body::from(body),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json["status"], "ready");
    assert_eq!(json["kind"], "document");
    let calls = world.oagw.requests_to("POST", "/api.openai.com/v1/files");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].subject, USER,
        "the request context, not the S2S one"
    );
    world.stop().await;
}

// ---- the cleanup handler on the started pipeline ---------------------------------------------------------------------

#[tokio::test]
async fn a_deleted_attachment_is_cleaned_up_by_the_started_gear_with_the_s2s_context() {
    let world = World::new(&json!({}), true).await;
    let chat = world.chat().await;
    let id = world
        .seed_attachment(chat, 1, |row| row.set("storage_backend", "primary"))
        .await;
    world
        .oagw
        .on_path("DELETE", "/api.openai.com/v1/files/*", |_| {
            ProxyReply::json(200, &json!({ "deleted": true }))
        });

    let (status, body) = world
        .call(
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/attachments/{id}"),
            &user_ctx(),
            None,
            Body::empty(),
        )
        .await;

    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    wait_until("the provider delete", || {
        !world
            .oagw
            .requests_to("DELETE", "/api.openai.com/v1/files/file-prov1")
            .is_empty()
    })
    .await;
    let delete = &world
        .oagw
        .requests_to("DELETE", "/api.openai.com/v1/files/file-prov1")[0];
    assert_eq!(delete.subject, S2S_SUBJECT);
    let mut done = false;
    for _ in 0..200 {
        if world.att_text(id, "cleanup_status").await == "done" {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(done, "the handler marks the row done");
    world.stop().await;
}

// ---- the reaper ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_started_gear_runs_the_upload_reaper_when_it_is_enabled() {
    let world = World::new(&json!({}), true).await;

    let running = world.gear.running_background_tasks();

    assert!(running.contains(&UPLOAD_REAPER_TASK), "{running:?}");
    world.gear.stop(CancellationToken::new()).await.unwrap();
    assert!(
        !world
            .gear
            .running_background_tasks()
            .contains(&UPLOAD_REAPER_TASK),
        "the reaper stops with the gear"
    );
}

#[tokio::test]
async fn a_disabled_reaper_is_not_spawned() {
    let enabled = World::new(&json!({}), true).await;
    assert!(
        enabled
            .gear
            .running_background_tasks()
            .contains(&UPLOAD_REAPER_TASK),
        "the default spawns it"
    );
    enabled.stop().await;
    let world = World::new(&json!({ "upload_reaper": { "enabled": false } }), true).await;

    let running = world.gear.running_background_tasks();

    assert!(!running.contains(&UPLOAD_REAPER_TASK), "{running:?}");
    world.stop().await;
}

#[tokio::test]
async fn the_started_gear_reaps_an_abandoned_upload() {
    let world = World::new(
        &json!({ "upload_reaper": { "enabled": true, "scan_interval_secs": 1, "stale_after_secs": 60 } }),
        true,
    )
    .await;
    let chat = world.chat().await;
    let stale = world
        .seed_attachment(chat, 1, |row| row.set("status", "uploaded"))
        .await;
    let fresh_stamp = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let fresh = world
        .seed_attachment(chat, 2, |row| {
            row.set("status", "uploaded").set("updated_at", fresh_stamp)
        })
        .await;

    let mut reaped = false;
    for _ in 0..300 {
        if world.att_text(stale, "status").await == "failed" {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert!(reaped, "the reaper scanned within a few intervals");
    assert_eq!(
        world.att_text(stale, "error_code").await,
        "upload_abandoned"
    );
    assert_eq!(world.att_text(stale, "cleanup_status").await, "pending");
    assert_eq!(
        world.att_text(fresh, "status").await,
        "uploaded",
        "a fresh row is not stale"
    );
    world.stop().await;
}
