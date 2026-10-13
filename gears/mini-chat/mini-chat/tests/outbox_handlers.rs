#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Outbox handlers (S§10.1, D§5.6 "Shared Outbox Processing Model"): each
//! handler is called directly with a constructed `OutboxMessage`; the last
//! test starts the pipeline with the real handlers and drives it over REST.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::gts::PluginV1;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::AccessScope;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};
use uuid::Uuid;

use common::{
    FakePolicy, TestApp, attachment_by_id, attachment_row, chat_path, chat_row, create_chat,
    default_limits, push_hello, requests_matching, stream_path, tenant_scope, ts, vector_store_row,
    vector_store_rows, wait_until,
};
use mini_chat::infra::db::entity::{attachment, chat, chat_vector_store};
use mini_chat::infra::db::repos::{AttachmentRepo, ChatRepo, VectorStoreRepo};
use mini_chat::infra::gateways::audit_gateway::AuditGateway;
use mini_chat::infra::outbox::handlers::{
    AttachmentCleanupHandler, AuditCounts, AuditHandler, ChatCleanupHandler, UsageHandler,
};
use mini_chat::infra::outbox::payloads::{
    AttachmentCleanupEventType, AttachmentCleanupPayload, ChatCleanupPayload,
};
use mini_chat_sdk::{
    AuditPluginError, BillingOutcome, KillSwitches, MiniChatAuditEvent,
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, PublishError, RequesterType,
    SettlementMethod, TerminalState, TurnMutationAuditEvent, TurnMutationAuditEventType,
    UsageEvent,
};

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A message on its `attempts + 1`-th delivery.
fn msg(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn json_msg<T: serde::Serialize>(v: &T, attempts: i16) -> OutboxMessage {
    msg(serde_json::to_vec(v).unwrap(), attempts)
}

fn corrupt() -> OutboxMessage {
    msg(b"{\"not\": \"the payload\"".to_vec(), 0)
}

fn is_ok(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Ok)
}

fn is_retry(r: &MessageResult) -> bool {
    matches!(r, MessageResult::Retry)
}

fn reject_reason(r: &MessageResult) -> String {
    match r {
        MessageResult::Reject(reason) => reason.clone(),
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

fn usage_event() -> UsageEvent {
    UsageEvent {
        tenant_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        chat_id: Uuid::new_v4(),
        turn_id: Some(Uuid::new_v4()),
        request_id: Uuid::new_v4(),
        effective_model: "gpt-4.1".to_owned(),
        selected_model: "gpt-4.1".to_owned(),
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        usage: None,
        actual_credits_micro: 42,
        settlement_method: SettlementMethod::Actual,
        policy_version_applied: 1,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: ts(1_700_000_000),
        requester_type: RequesterType::User,
        dedupe_key: format!("dedupe-{}", Uuid::new_v4()),
        system_task_type: None,
    }
}

#[tokio::test]
async fn usage_published_once() {
    let policy = Arc::new(FakePolicy::new(
        Vec::new(),
        KillSwitches::default(),
        default_limits(),
    ));
    let handler = UsageHandler::new(policy.clone());
    let ev = usage_event();

    assert!(is_ok(&handler.handle(&json_msg(&ev, 0)).await));
    let published = policy.published_usage();
    assert_eq!(published.len(), 1);
    assert_eq!(
        serde_json::to_value(&published[0]).unwrap(),
        serde_json::to_value(&ev).unwrap(),
        "the event is published as enqueued"
    );

    policy.push_publish_error(PublishError::Transient("policy backend down".to_owned()));
    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));

    policy.push_publish_error(PublishError::Permanent("rejected".to_owned()));
    let reason = reject_reason(&handler.handle(&json_msg(&ev, 0)).await);
    assert!(reason.contains("rejected"), "{reason}");

    let reason = reject_reason(&handler.handle(&corrupt()).await);
    assert!(!reason.is_empty());

    assert_eq!(
        policy.published_usage().len(),
        1,
        "failed and corrupt deliveries publish nothing"
    );
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

const VENDOR: &str = "constructorfabric";

#[derive(Debug, Clone)]
enum PluginMode {
    Ok,
    Fail(AuditPluginError),
    Slow(Duration),
}

struct FakeAuditPlugin {
    mode: Mutex<PluginMode>,
    events: Mutex<Vec<MiniChatAuditEvent>>,
}

impl FakeAuditPlugin {
    fn new(mode: PluginMode) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            events: Mutex::new(Vec::new()),
        })
    }

    fn set(&self, mode: PluginMode) {
        *self.mode.lock().unwrap() = mode;
    }

    fn delivered(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

#[async_trait]
impl MiniChatAuditPluginClientV1 for FakeAuditPlugin {
    async fn emit(&self, event: MiniChatAuditEvent) -> Result<(), AuditPluginError> {
        let mode = self.mode.lock().unwrap().clone();
        match mode {
            PluginMode::Ok => {}
            PluginMode::Fail(e) => return Err(e),
            PluginMode::Slow(d) => tokio::time::sleep(d).await,
        }
        self.events.lock().unwrap().push(event);
        Ok(())
    }
}

fn audit_event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: TurnMutationAuditEventType::TurnDelete,
        actor_user_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: Some(Uuid::new_v4()),
        new_request_id: None,
        request_id: None,
        timestamp: ts(1_700_000_000),
    })
}

fn set_registry(hub: &ClientHub, registry: MockTypesRegistryClient) {
    let api: Arc<dyn TypesRegistryClient> = Arc::new(registry);
    hub.register::<dyn TypesRegistryClient>(api);
}

/// Registry content with one audit plugin instance of [`VENDOR`].
fn audit_plugin_registration() -> (String, MockTypesRegistryClient) {
    let (id, payload) = PluginV1::<MiniChatAuditPluginSpecV1>::build_registration(
        "cf.test.outbox_audit.plugin.v1",
        VENDOR,
        100,
    )
    .unwrap();
    let id = id.to_string();
    let inst = make_test_instance(&id, payload);
    (id, MockTypesRegistryClient::new().with_instances([inst]))
}

fn register_plugin_client(hub: &ClientHub, id: &str, plugin: Arc<FakeAuditPlugin>) {
    let api: Arc<dyn MiniChatAuditPluginClientV1> = plugin;
    hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(id), api);
}

#[tokio::test]
async fn audit_semantics() {
    let hub = Arc::new(ClientHub::new());
    set_registry(&hub, MockTypesRegistryClient::new());
    let gateway = Arc::new(AuditGateway::new(Arc::clone(&hub), VENDOR));
    let handler = AuditHandler::new(gateway.clone());
    let ev = audit_event();

    // A corrupt payload is rejected before any plugin resolution.
    reject_reason(&handler.handle(&corrupt()).await);

    // No plugin registered: acknowledged and counted as dropped (not cached).
    assert!(is_ok(&handler.handle(&json_msg(&ev, 0)).await));
    assert!(is_ok(&handler.handle(&json_msg(&ev, 0)).await));
    assert_eq!(handler.counts().dropped, 2);

    // The instance resolves but its client is missing from ClientHub: Retry,
    // and the 120th attempt dead-letters the event.
    let (id, registry) = audit_plugin_registration();
    set_registry(&hub, registry);
    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));
    assert!(is_retry(&handler.handle(&json_msg(&ev, 118)).await));
    reject_reason(&handler.handle(&json_msg(&ev, 119)).await);

    // A plugin registered later is used.
    let plugin = FakeAuditPlugin::new(PluginMode::Ok);
    register_plugin_client(&hub, &id, plugin.clone());
    assert!(is_ok(&handler.handle(&json_msg(&ev, 0)).await));
    assert_eq!(plugin.delivered(), 1);

    // Transient plugin errors and plugin timeouts retry; permanent rejects.
    plugin.set(PluginMode::Fail(AuditPluginError::Transient(
        "busy".to_owned(),
    )));
    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));
    reject_reason(&handler.handle(&json_msg(&ev, 119)).await);
    plugin.set(PluginMode::Fail(AuditPluginError::PluginTimeout));
    assert!(is_retry(&handler.handle(&json_msg(&ev, 0)).await));
    plugin.set(PluginMode::Fail(AuditPluginError::Permanent(
        "bad".to_owned(),
    )));
    reject_reason(&handler.handle(&json_msg(&ev, 0)).await);

    // The plugin call is bounded by the handler timeout (30 s in production).
    plugin.set(PluginMode::Slow(Duration::from_secs(5)));
    let fast = AuditHandler::new(gateway).with_timeout(Duration::from_millis(50));
    assert!(is_retry(&fast.handle(&json_msg(&ev, 0)).await));

    assert_eq!(
        handler.counts(),
        AuditCounts {
            delivered: 1,
            dropped: 2,
            retry: 4,
            reject: 4,
        }
    );
}

// ---------------------------------------------------------------------------
// Cleanup fixtures
// ---------------------------------------------------------------------------

/// Chat of a new tenant/user, soft-deleted when `deleted`.
async fn insert_chat(app: &TestApp, deleted: bool) -> chat::Model {
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let mut row = chat_row(tenant, user);
    if deleted {
        row.deleted_at = Some(ts(1_700_000_100));
    }
    let conn = app.db.conn().unwrap();
    ChatRepo
        .insert(&conn, &tenant_scope(tenant, user), row)
        .await
        .unwrap()
}

/// Ready document of `chat` stored at the default `openai` provider, with
/// provider cleanup pending.
async fn insert_attachment(
    app: &TestApp,
    chat: &chat::Model,
    file_id: Option<&str>,
) -> attachment::Model {
    let mut row = attachment_row(chat);
    "openai".clone_into(&mut row.storage_backend);
    "ready".clone_into(&mut row.status);
    row.provider_file_id = file_id.map(ToOwned::to_owned);
    row.cleanup_status = Some("pending".to_owned());
    let conn = app.db.conn().unwrap();
    AttachmentRepo
        .insert(&conn, &AccessScope::for_tenant(chat.tenant_id), row)
        .await
        .unwrap()
}

async fn insert_vector_store(
    app: &TestApp,
    chat: &chat::Model,
    vs: &str,
) -> chat_vector_store::Model {
    let mut row = vector_store_row(chat);
    row.vector_store_id = Some(vs.to_owned());
    "openai".clone_into(&mut row.provider);
    let conn = app.db.conn().unwrap();
    VectorStoreRepo
        .insert(&conn, &AccessScope::for_tenant(chat.tenant_id), row)
        .await
        .unwrap()
}

fn attachment_payload(a: &attachment::Model) -> AttachmentCleanupPayload {
    AttachmentCleanupPayload {
        event_type: AttachmentCleanupEventType::AttachmentDeleted,
        tenant_id: a.tenant_id,
        chat_id: a.chat_id,
        attachment_id: a.id,
        provider_file_id: a.provider_file_id.clone(),
        vector_store_id: None,
        storage_backend: a.storage_backend.clone(),
        attachment_kind: a.attachment_kind.clone(),
        deleted_at: ts(1_700_000_200),
        secondary_ref: None,
    }
}

fn chat_payload(c: &chat::Model) -> ChatCleanupPayload {
    ChatCleanupPayload::new(c.tenant_id, c.id, ts(1_700_000_100))
}

fn delete_index(app: &TestApp, needle: &str) -> usize {
    app.oagw
        .requests()
        .iter()
        .position(|r| r.method == "DELETE" && r.uri.contains(needle))
        .unwrap_or_else(|| panic!("no DELETE {needle}: {:?}", app.oagw.requests()))
}

// ---------------------------------------------------------------------------
// Attachment cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attachment_cleanup_deletes_provider_file() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, false).await;
    let att = insert_attachment(&app, &chat, Some("file-cleanup0001aaaa")).await;
    app.oagw.push_json_for(
        "DELETE",
        "/v1/files/file-cleanup0001aaaa",
        200,
        json!({"id": "file-cleanup0001aaaa", "deleted": true}),
    );
    let handler = AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler
        .handle(&json_msg(&attachment_payload(&att), 0))
        .await;

    assert!(is_ok(&res), "{res:?}");
    let deletes = requests_matching(&app, "DELETE", "/v1/files/file-cleanup0001aaaa");
    assert_eq!(deletes.len(), 1);
    assert_eq!(
        deletes[0].uri,
        "/api.openai.com/v1/files/file-cleanup0001aaaa"
    );
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
    assert!(row.cleanup_updated_at.is_some());

    // Corrupt payloads are dead-lettered.
    reject_reason(&handler.handle(&corrupt()).await);
}

#[tokio::test]
async fn attachment_cleanup_404_is_success() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, false).await;
    let att = insert_attachment(&app, &chat, Some("file-gone00000001")).await;
    app.oagw.push_json_for(
        "DELETE",
        "/v1/files/file-gone00000001",
        404,
        json!({"error": {"message": "No such file"}}),
    );
    let handler = AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler
        .handle(&json_msg(&attachment_payload(&att), 0))
        .await;

    assert!(is_ok(&res), "{res:?}");
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
    assert_eq!(row.cleanup_attempts, 0);
}

#[tokio::test]
async fn attachment_cleanup_without_provider_file_is_done() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, false).await;
    let att = insert_attachment(&app, &chat, None).await;
    let handler = AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler
        .handle(&json_msg(&attachment_payload(&att), 0))
        .await;

    assert!(is_ok(&res), "{res:?}");
    assert!(app.oagw.requests().is_empty());
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("done"));
}

#[tokio::test]
async fn attachment_cleanup_failure_counts_and_terminal_failed() {
    let app = TestApp::builder()
        .config(|c| c.cleanup_worker.max_attempts = 2)
        .build()
        .await;
    let chat = insert_chat(&app, false).await;
    let att = insert_attachment(&app, &chat, Some("file-flaky0000001")).await;
    let handler = AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup));
    let payload = attachment_payload(&att);

    app.oagw.push_json_for(
        "DELETE",
        "/v1/files/file-flaky0000001",
        500,
        json!({"error": {"message": "upstream exploded"}}),
    );
    assert!(is_retry(&handler.handle(&json_msg(&payload, 0)).await));
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    assert_eq!(row.cleanup_attempts, 1);
    assert!(row.last_cleanup_error.is_some());
    assert!(row.cleanup_updated_at.is_some());

    app.oagw.push_json_for(
        "DELETE",
        "/v1/files/file-flaky0000001",
        503,
        json!({"error": {"message": "still exploding"}}),
    );
    let reason = reject_reason(&handler.handle(&json_msg(&payload, 1)).await);
    assert!(reason.contains("max attempts"), "{reason}");
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 2);
    assert!(row.last_cleanup_error.is_some());
    assert_eq!(
        requests_matching(&app, "DELETE", "/v1/files/file-flaky0000001").len(),
        2
    );
}

#[tokio::test]
async fn attachment_cleanup_skips_deleted_chat() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, true).await;
    let att = insert_attachment(&app, &chat, Some("file-chatowned0001")).await;
    let handler = AttachmentCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler
        .handle(&json_msg(&attachment_payload(&att), 0))
        .await;

    assert!(is_ok(&res), "{res:?}");
    assert!(
        app.oagw.requests().is_empty(),
        "chat cleanup owns the files"
    );
    let row = attachment_by_id(&app, att.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
}

// ---------------------------------------------------------------------------
// Chat cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_cleanup_deletes_files_then_vector_store() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, true).await;
    let a1 = insert_attachment(&app, &chat, Some("file-chatone000001")).await;
    let a2 = insert_attachment(&app, &chat, Some("file-chattwo000002")).await;
    insert_vector_store(&app, &chat, "vs_chatstore00000001").await;
    for f in ["file-chatone000001", "file-chattwo000002"] {
        app.oagw.push_json_for(
            "DELETE",
            &format!("/v1/files/{f}"),
            200,
            json!({"deleted": true}),
        );
    }
    app.oagw.push_json_for(
        "DELETE",
        "/v1/vector_stores/vs_chatstore00000001",
        200,
        json!({"deleted": true}),
    );
    let handler = ChatCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler.handle(&json_msg(&chat_payload(&chat), 0)).await;

    assert!(is_ok(&res), "{res:?}");
    let vs = delete_index(&app, "/v1/vector_stores/vs_chatstore00000001");
    assert!(delete_index(&app, "/v1/files/file-chatone000001") < vs);
    assert!(delete_index(&app, "/v1/files/file-chattwo000002") < vs);
    assert_eq!(app.oagw.requests().len(), 3);
    assert!(vector_store_rows(&app).await.is_empty(), "row removed");
    for a in [a1.id, a2.id] {
        assert_eq!(
            attachment_by_id(&app, a).await.cleanup_status.as_deref(),
            Some("done")
        );
    }

    // Redelivery is idempotent: nothing left to delete.
    assert!(is_ok(
        &handler.handle(&json_msg(&chat_payload(&chat), 1)).await
    ));
    assert_eq!(app.oagw.requests().len(), 3);
}

#[tokio::test]
async fn chat_cleanup_failed_attachment_does_not_block_vector_store() {
    let app = TestApp::builder()
        .config(|c| c.cleanup_worker.max_attempts = 1)
        .build()
        .await;
    let chat = insert_chat(&app, true).await;
    let a1 = insert_attachment(&app, &chat, Some("file-broken0000001")).await;
    insert_vector_store(&app, &chat, "vs_afterfail0000001").await;
    app.oagw.push_json_for(
        "DELETE",
        "/v1/files/file-broken0000001",
        500,
        json!({"error": {"message": "nope"}}),
    );
    app.oagw.push_json_for(
        "DELETE",
        "/v1/vector_stores/vs_afterfail0000001",
        200,
        json!({"deleted": true}),
    );
    let handler = ChatCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let res = handler.handle(&json_msg(&chat_payload(&chat), 0)).await;

    assert!(is_ok(&res), "{res:?}");
    let row = attachment_by_id(&app, a1.id).await;
    assert_eq!(row.cleanup_status.as_deref(), Some("failed"));
    assert_eq!(row.cleanup_attempts, 1);
    assert!(vector_store_rows(&app).await.is_empty());
}

#[tokio::test]
async fn chat_cleanup_rejects_active_chat() {
    let app = TestApp::builder().build().await;
    let chat = insert_chat(&app, false).await;
    let att = insert_attachment(&app, &chat, Some("file-activechat001")).await;
    let handler = ChatCleanupHandler::new(Arc::clone(&app.services.cleanup));

    let reason = reject_reason(&handler.handle(&json_msg(&chat_payload(&chat), 0)).await);

    assert!(reason.contains("not soft-deleted"), "{reason}");
    assert!(app.oagw.requests().is_empty());
    assert_eq!(
        attachment_by_id(&app, att.id)
            .await
            .cleanup_status
            .as_deref(),
        Some("pending")
    );
    reject_reason(&handler.handle(&corrupt()).await);
}

#[tokio::test]
async fn vector_store_delete_failure_retries_then_rejects_at_max() {
    let app = TestApp::builder()
        .config(|c| c.cleanup_worker.max_attempts = 3)
        .build()
        .await;
    let chat = insert_chat(&app, true).await;
    insert_vector_store(&app, &chat, "vs_stubborn00000001").await;
    let handler = ChatCleanupHandler::new(Arc::clone(&app.services.cleanup));
    let payload = chat_payload(&chat);
    let fail = || {
        app.oagw.push_json_for(
            "DELETE",
            "/v1/vector_stores/vs_stubborn00000001",
            500,
            json!({"error": {"message": "vector store backend down"}}),
        );
    };

    fail();
    assert!(is_retry(&handler.handle(&json_msg(&payload, 0)).await));
    fail();
    assert!(is_retry(&handler.handle(&json_msg(&payload, 1)).await));
    assert_eq!(vector_store_rows(&app).await.len(), 1, "row kept");

    fail();
    let reason = reject_reason(&handler.handle(&json_msg(&payload, 2)).await);
    assert_eq!(reason, "vector store delete: max attempts (3) reached");
    assert_eq!(
        vector_store_rows(&app).await.len(),
        1,
        "row kept for a dead-letter replay"
    );
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

#[tokio::test]
async fn pipeline_end_to_end() {
    let app = TestApp::builder().real_handlers().build().await;
    let (user, tenant) = (Uuid::new_v4(), Uuid::new_v4());
    let client = app.as_user(user, tenant);
    let chat_id = create_chat(&client, "s1").await;

    // One completed turn: its usage event is published once, its audit
    // event delivered once.
    push_hello(&app);
    let resp = client
        .post_json(&stream_path(chat_id), &json!({"content": "Hello?"}))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    wait_until(3, || async {
        app.policy.published_usage().len() == 1 && app.audit.events().len() == 1
    })
    .await;

    // A chat with a document and a vector store, deleted over REST.
    let chat = ChatRepo
        .find_by_id(
            &app.db.conn().unwrap(),
            &tenant_scope(tenant, user),
            chat_id,
        )
        .await
        .unwrap()
        .unwrap();
    let att = insert_attachment(&app, &chat, Some("file-pipeline00001")).await;
    insert_vector_store(&app, &chat, "vs_pipeline00000001").await;
    let resp = client.delete(&chat_path(chat_id)).await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "{}", resp.text());

    wait_until(3, || async {
        !requests_matching(&app, "DELETE", "/v1/vector_stores/vs_pipeline00000001").is_empty()
    })
    .await;
    assert!(
        delete_index(&app, "/v1/files/file-pipeline00001")
            < delete_index(&app, "/v1/vector_stores/vs_pipeline00000001")
    );
    wait_until(3, || async { vector_store_rows(&app).await.is_empty() }).await;
    assert_eq!(
        attachment_by_id(&app, att.id)
            .await
            .cleanup_status
            .as_deref(),
        Some("done")
    );
    assert_eq!(
        app.policy.published_usage().len(),
        1,
        "published exactly once"
    );
}
