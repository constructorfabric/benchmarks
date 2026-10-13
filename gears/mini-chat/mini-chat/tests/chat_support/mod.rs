//! S2 (chats, messages, reactions) harness on top of the shared S1 fakes.
//!
//! Kept apart from `common` so the S1 test binaries (which only declare
//! `mod common;`) compile independently of the S2 production items. Everything
//! here relies only on the items listed in `S2-test-interface.md`. Test files
//! declare both `mod common;` and `mod chat_support;`.

#![allow(
    dead_code,
    unused_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, header};
use mini_chat::api::rest::register_chat_routes;
use mini_chat::domain::chats::ChatsService;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::messages::MessagesService;
use mini_chat::domain::reactions::ReactionsService;
use mini_chat::infra::db::entity::{
    attachments, chats, message_attachments, message_reactions, messages,
};
use mini_chat::infra::db::migrations::gear_migrations;
pub use mini_chat::infra::db::timestamp::from_db;
use mini_chat::infra::db::timestamp::{to_db, to_db_opt};
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat::infra::outbox::ChatCleanupOutbox;
use mini_chat_sdk::ModelCatalogEntry;
use sea_orm::{ActiveValue::Set, EntityTrait};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{
    LeasedMessageHandler, MessageResult, Outbox, OutboxHandle, OutboxMessage, OutboxProfile,
    Partitions,
};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::common::{
    FakePdp, FakePolicyPlugin, PdpMode, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem,
    catalog_entry_json, ctx, ctx_a, send,
};

/// Second user of tenant A (`a_reviewer` of the black-box suite).
pub const USER_A2: Uuid = Uuid::from_u128(0x4444_4444_6a88_4768_9dfc_6bcd_5187_d9ed);

pub fn ctx_a2() -> SecurityContext {
    ctx(USER_A2, TENANT_A)
}

/// The tenant B user (`b` of the black-box suite).
pub fn ctx_b() -> SecurityContext {
    ctx(USER_B, TENANT_B)
}

pub const CHAT_PDP_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
pub const CHAT_CLEANUP_QUEUE: &str = "mini-chat.chat_cleanup";
pub const CHAT_CLEANUP_PARTITIONS: u16 = 4;
pub const DAY: i64 = 86_400;

/// Deterministic id of a seeded row (byte order == numeric order).
pub fn uid(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// `2025-01-01T00:00:00Z` plus `secs` seconds.
pub fn ts(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_735_689_600 + secs).expect("valid timestamp")
}

/// `2025-01-01T00:00:00Z` plus `secs` seconds and `micros` microseconds.
pub fn ts_micros(secs: i64, micros: i64) -> OffsetDateTime {
    ts(secs) + time::Duration::microseconds(micros)
}

/// Five instants within one second whose canonical text has different
/// fraction widths (none, 6, 3, 6, none digits), strictly ascending:
/// `:00`, `:00.422670`, `:00.500`, `:00.500001`, `:01`.
pub fn mixed_width_instants() -> [OffsetDateTime; 5] {
    [
        ts_micros(10, 0),
        ts_micros(10, 422_670),
        ts_micros(10, 500_000),
        ts_micros(10, 500_001),
        ts_micros(11, 0),
    ]
}

/// Parse a wire timestamp (RFC 3339).
pub fn parse_ts(value: &Value) -> OffsetDateTime {
    let raw = value
        .as_str()
        .unwrap_or_else(|| panic!("timestamp must be a string, got {value}"));
    OffsetDateTime::parse(raw, &Rfc3339)
        .unwrap_or_else(|e| panic!("`{raw}` is not an RFC 3339 timestamp: {e}"))
}

// ── request helpers with bodies ──────────────────────────────────────────────

/// Send a request with an optional `Content-Type` and a raw body.
pub async fn send_body(
    router: &Router,
    method: &str,
    uri: &str,
    ctx: Option<&SecurityContext>,
    content_type: Option<&str>,
    body: &str,
) -> Resp {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let mut req = builder.body(Body::from(body.to_owned())).expect("request");
    if let Some(ctx) = ctx {
        req.extensions_mut().insert(ctx.clone());
    }
    let resp = router
        .clone()
        .oneshot(req)
        .await
        .expect("router is infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    Resp {
        status,
        headers,
        raw: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub async fn send_json(
    router: &Router,
    method: &str,
    uri: &str,
    ctx: &SecurityContext,
    body: &Value,
) -> Resp {
    send_body(
        router,
        method,
        uri,
        Some(ctx),
        Some("application/json"),
        &body.to_string(),
    )
    .await
}

// ── problem assertions ───────────────────────────────────────────────────────

fn list_of<'a>(problem: &'a Value, key: &str) -> &'a Vec<Value> {
    problem["context"][key]
        .as_array()
        .unwrap_or_else(|| panic!("problem has no context.{key}[]: {problem}"))
}

/// `context.field_violations[]` contains `{field, reason}`.
pub fn assert_field_violation(problem: &Value, field: &str, reason: &str) {
    let list = list_of(problem, "field_violations");
    assert!(
        list.iter()
            .any(|v| v["field"] == json!(field) && v["reason"] == json!(reason)),
        "expected field_violations[{field}].reason = {reason}, got {list:?}"
    );
}

/// `context.field_violations[]` contains a violation with `reason` (any field).
pub fn assert_violation_reason(problem: &Value, reason: &str) {
    let list = list_of(problem, "field_violations");
    assert!(
        list.iter().any(|v| v["reason"] == json!(reason)),
        "expected a field violation with reason {reason}, got {list:?}"
    );
}

/// `context.violations[]` contains `{subject, type}` (failed_precondition).
pub fn assert_precondition(problem: &Value, subject: &str, kind: &str) {
    let list = list_of(problem, "violations");
    assert!(
        list.iter()
            .any(|v| v["subject"] == json!(subject) && v["type"] == json!(kind)),
        "expected violations[{{subject: {subject}, type: {kind}}}], got {list:?}"
    );
}

/// 404 `not_found` problem for the given resource kind (`chat`, `message`).
pub fn assert_not_found(resp: &Resp, kind: &str) -> Value {
    let problem = assert_problem(resp, 404, "not_found");
    assert_eq!(
        problem["context"]["resource_type"],
        json!(format!("gts.cf.core.mini_chat.{kind}.v1~")),
        "404 names the missing resource type: {}",
        resp.raw
    );
    problem
}

/// 400 `invalid_argument` raised by the OData layer: `context.resource_type`
/// is the OData query type and one violation carries `{field, reason}`.
pub fn assert_odata_problem(resp: &Resp, field: &str, reason: &str) -> Value {
    let problem = assert_problem(resp, 400, "invalid_argument");
    assert_eq!(
        problem["context"]["resource_type"],
        json!("gts.cf.core.odata.query.v1~"),
        "OData errors are reported against the OData query type: {}",
        resp.raw
    );
    assert_field_violation(&problem, field, reason);
    problem
}

/// 400 `invalid_argument` with `field_violations[field].reason` for a chat/message field.
pub fn assert_invalid_argument(resp: &Resp, field: &str, reason: &str) -> Value {
    let problem = assert_problem(resp, 400, "invalid_argument");
    assert_field_violation(&problem, field, reason);
    problem
}

// ── catalog helpers ──────────────────────────────────────────────────────────

/// A catalog entry with an explicit `preference.is_default`.
pub fn model_entry(id: &str, tier: &str, enabled: bool, is_default: bool) -> ModelCatalogEntry {
    let mut value = catalog_entry_json(id, tier, enabled);
    value["preference"]["is_default"] = json!(is_default);
    serde_json::from_value(value).expect("catalog entry")
}

/// `gpt-standard` (enabled), `gpt-premium` (enabled, the default), `gpt-off` (disabled).
pub fn default_chat_models() -> Vec<ModelCatalogEntry> {
    vec![
        model_entry("gpt-standard", "Standard", true, false),
        model_entry("gpt-premium", "Premium", true, true),
        model_entry("gpt-off", "Standard", false, false),
    ]
}

// ── seed rows ────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ChatSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub model: String,
    pub title: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

impl ChatSeed {
    pub fn new(tenant_id: Uuid, user_id: Uuid) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id,
            user_id,
            model: "gpt-premium".to_owned(),
            title: None,
            created_at: ts(0),
            updated_at: ts(0),
            deleted_at: None,
        }
    }

    /// A chat owned by `USER_A` of `TENANT_A`.
    pub fn of_a() -> Self {
        Self::new(TENANT_A, USER_A)
    }

    pub fn with_id(mut self, n: u128) -> Self {
        self.id = uid(n);
        self
    }

    pub fn titled(mut self, title: &str) -> Self {
        self.title = Some(title.to_owned());
        self
    }

    pub fn with_model(mut self, model: &str) -> Self {
        model.clone_into(&mut self.model);
        self
    }

    /// Sets `created_at` and `updated_at` (seconds after `2025-01-01T00:00:00Z`).
    pub fn at(mut self, secs: i64) -> Self {
        self.created_at = ts(secs);
        self.updated_at = ts(secs);
        self
    }

    pub fn updated_at(mut self, secs: i64) -> Self {
        self.updated_at = ts(secs);
        self
    }

    /// Sets `created_at` and `updated_at` to an exact instant.
    pub fn at_time(mut self, when: OffsetDateTime) -> Self {
        self.created_at = when;
        self.updated_at = when;
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted_at = Some(ts(1));
        self
    }
}

#[derive(Clone)]
pub struct MessageSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Option<Uuid>,
    pub role: String,
    pub content: String,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub created_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}

impl MessageSeed {
    pub fn new(chat_id: Uuid, role: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            chat_id,
            request_id: Some(Uuid::new_v4()),
            role: role.to_owned(),
            content: format!("{role} message"),
            model: None,
            input_tokens: 0,
            output_tokens: 0,
            created_at: ts(0),
            deleted_at: None,
        }
    }

    pub fn user(chat_id: Uuid) -> Self {
        Self::new(chat_id, "user")
    }

    pub fn assistant(chat_id: Uuid) -> Self {
        Self::new(chat_id, "assistant")
    }

    pub fn system(chat_id: Uuid) -> Self {
        Self::new(chat_id, "system")
    }

    pub fn with_id(mut self, n: u128) -> Self {
        self.id = uid(n);
        self
    }

    pub fn tenant(mut self, tenant_id: Uuid) -> Self {
        self.tenant_id = tenant_id;
        self
    }

    pub fn content(mut self, content: &str) -> Self {
        content.clone_into(&mut self.content);
        self
    }

    pub fn request(mut self, request_id: Option<Uuid>) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn at(mut self, secs: i64) -> Self {
        self.created_at = ts(secs);
        self
    }

    /// Sets `created_at` to an exact instant.
    pub fn at_time(mut self, when: OffsetDateTime) -> Self {
        self.created_at = when;
        self
    }

    pub fn model(mut self, model: &str) -> Self {
        self.model = Some(model.to_owned());
        self
    }

    pub fn tokens(mut self, input: i64, output: i64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted_at = Some(ts(1));
        self
    }
}

#[derive(Clone)]
pub struct AttachmentSeed {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub filename: String,
    pub kind: String,
    pub status: String,
    pub cleanup_status: Option<String>,
    pub thumbnail: Option<Vec<u8>>,
    pub deleted_at: Option<OffsetDateTime>,
}

impl AttachmentSeed {
    /// A ready document of `TENANT_A`.
    pub fn document(chat_id: Uuid, filename: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            chat_id,
            filename: filename.to_owned(),
            kind: "document".to_owned(),
            status: "ready".to_owned(),
            cleanup_status: None,
            thumbnail: None,
            deleted_at: None,
        }
    }

    /// A ready image with a stored WebP thumbnail of `THUMBNAIL_BYTES`.
    pub fn image(chat_id: Uuid, filename: &str) -> Self {
        Self {
            kind: "image".to_owned(),
            thumbnail: Some(THUMBNAIL_BYTES.to_vec()),
            ..Self::document(chat_id, filename)
        }
    }

    pub fn with_id(mut self, n: u128) -> Self {
        self.id = uid(n);
        self
    }

    pub fn status(mut self, status: &str) -> Self {
        status.clone_into(&mut self.status);
        self
    }

    pub fn cleanup(mut self, cleanup_status: &str) -> Self {
        self.cleanup_status = Some(cleanup_status.to_owned());
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted_at = Some(ts(1));
        self
    }
}

/// Raw bytes stored as `attachments.img_thumbnail` by `AttachmentSeed::image`.
pub const THUMBNAIL_BYTES: &[u8] = b"RIFF\x00\x00\x00\x00WEBPVP8 thumbnail-bytes";
pub const THUMBNAIL_WIDTH: i32 = 64;
pub const THUMBNAIL_HEIGHT: i32 = 48;

// ── recording chat-cleanup handler ───────────────────────────────────────────

/// Leased handler registered on the chat-cleanup queue of the harness; it
/// records every delivered payload (JSON) and acknowledges it.
#[derive(Default)]
pub struct CleanupRecorder {
    delivered: Mutex<Vec<Value>>,
}

impl CleanupRecorder {
    pub fn received(&self) -> Vec<Value> {
        self.delivered.lock().unwrap().clone()
    }

    /// Wait until at least `n` payloads were delivered (15 s budget).
    pub async fn wait_for(&self, n: usize) -> Vec<Value> {
        for _ in 0..300 {
            let got = self.received();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} chat-cleanup message(s) from the outbox, got {:?}",
            self.received()
        );
    }

    /// Give the outbox time to deliver, then assert nothing was enqueued.
    pub async fn assert_none(&self) {
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            self.received().is_empty(),
            "no chat-cleanup message may be enqueued, got {:?}",
            self.received()
        );
    }
}

struct RecorderHandler(Arc<CleanupRecorder>);

#[async_trait]
impl LeasedMessageHandler for RecorderHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: Value = serde_json::from_slice(&msg.payload)
            .unwrap_or_else(|e| panic!("chat-cleanup payload must be JSON: {e}"));
        self.0.delivered.lock().unwrap().push(payload);
        MessageResult::Ok
    }
}

// ── the harness ──────────────────────────────────────────────────────────────

/// SQLite database file in the temp dir, removed on drop.
pub struct TempDbFile {
    path: PathBuf,
}

impl TempDbFile {
    pub fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!("mini-chat-s2-{}.db", Uuid::new_v4().simple())),
        }
    }

    pub fn dsn(&self) -> String {
        format!(
            "sqlite://{}?mode=rwc&WAL=true&busy_timeout=5000",
            self.path.display()
        )
    }
}

impl Drop for TempDbFile {
    fn drop(&mut self) {
        let base = self.path.display().to_string();
        for suffix in ["", "-wal", "-shm"] {
            std::fs::remove_file(format!("{base}{suffix}")).unwrap_or_default();
        }
    }
}

pub struct ChatHarnessOptions {
    pub models: Vec<ModelCatalogEntry>,
    pub pdp: Arc<FakePdp>,
    pub prefix: String,
    /// `false`: the outbox runs but the chat-cleanup queue is not registered,
    /// so every enqueue fails (`QueueNotRegistered`).
    pub register_cleanup_queue: bool,
}

impl Default for ChatHarnessOptions {
    fn default() -> Self {
        Self {
            models: default_chat_models(),
            pdp: FakePdp::allow(),
            prefix: "/mini-chat".to_owned(),
            register_cleanup_queue: true,
        }
    }
}

/// The S2 REST surface built in-process on a migrated SQLite file database with
/// a running outbox (recording handler on the chat-cleanup queue).
pub struct ChatHarness {
    pub router: Router,
    pub db: Arc<DBProvider<DomainError>>,
    pub pdp: Arc<FakePdp>,
    pub plugin: Arc<FakePolicyPlugin>,
    pub cleanup: Arc<CleanupRecorder>,
    pub openapi: Arc<OpenApiRegistryImpl>,
    pub prefix: String,
    _outbox: OutboxHandle,
    _db_file: TempDbFile,
}

pub async fn chat_harness() -> ChatHarness {
    chat_harness_with(ChatHarnessOptions::default()).await
}

pub async fn chat_harness_with(opts: ChatHarnessOptions) -> ChatHarness {
    let db_file = TempDbFile::new();
    let db = connect_db(
        &db_file.dsn(),
        ConnectOpts {
            max_conns: Some(5),
            ..Default::default()
        },
    )
    .await
    .expect("connect sqlite file");
    run_migrations_for_testing(&db, gear_migrations())
        .await
        .expect("gear migrations apply");
    let provider = Arc::new(DBProvider::<DomainError>::new(db.clone()));

    let recorder = Arc::new(CleanupRecorder::default());
    let queue = if opts.register_cleanup_queue {
        CHAT_CLEANUP_QUEUE
    } else {
        "mini-chat.unrelated"
    };
    let outbox = Outbox::builder(db.clone())
        .profile(OutboxProfile::low_latency())
        .queue(
            queue,
            Partitions::of(if opts.register_cleanup_queue {
                CHAT_CLEANUP_PARTITIONS
            } else {
                1
            }),
        )
        .leased(RecorderHandler(recorder.clone()))
        .start()
        .await
        .expect("outbox starts");

    let plugin = FakePolicyPlugin::new(opts.models);
    let gateway = Arc::new(ModelPolicyGateway::from_client(plugin.clone()));
    let enforcer = opts.pdp.enforcer();
    let cleanup_port = Arc::new(ChatCleanupOutbox::new(
        outbox.outbox().clone(),
        CHAT_CLEANUP_QUEUE,
        CHAT_CLEANUP_PARTITIONS,
    ));
    let chats_service = Arc::new(ChatsService::new(
        provider.clone(),
        gateway,
        cleanup_port,
        enforcer.clone(),
    ));
    let messages_service = Arc::new(MessagesService::new(provider.clone(), enforcer.clone()));
    let reactions_service = Arc::new(ReactionsService::new(provider.clone(), enforcer));
    let openapi = Arc::new(OpenApiRegistryImpl::new());
    let router = register_chat_routes(
        Router::new(),
        openapi.as_ref(),
        chats_service,
        messages_service,
        reactions_service,
        &opts.prefix,
    );

    ChatHarness {
        router,
        db: provider,
        pdp: opts.pdp,
        plugin,
        cleanup: recorder,
        openapi,
        prefix: opts.prefix,
        _outbox: outbox,
        _db_file: db_file,
    }
}

impl ChatHarness {
    pub fn chats_uri(&self) -> String {
        format!("{}/v1/chats", self.prefix)
    }

    pub fn chat_uri(&self, id: Uuid) -> String {
        format!("{}/v1/chats/{id}", self.prefix)
    }

    pub fn messages_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages", self.prefix)
    }

    pub fn reaction_uri(&self, chat: Uuid, message: Uuid) -> String {
        format!(
            "{}/v1/chats/{chat}/messages/{message}/reaction",
            self.prefix
        )
    }

    pub async fn get(&self, uri: &str, ctx: &SecurityContext) -> Resp {
        send(&self.router, "GET", uri, Some(ctx)).await
    }

    pub async fn delete(&self, uri: &str, ctx: &SecurityContext) -> Resp {
        send(&self.router, "DELETE", uri, Some(ctx)).await
    }

    pub async fn post_json(&self, uri: &str, ctx: &SecurityContext, body: &Value) -> Resp {
        send_json(&self.router, "POST", uri, ctx, body).await
    }

    pub async fn patch_json(&self, uri: &str, ctx: &SecurityContext, body: &Value) -> Resp {
        send_json(&self.router, "PATCH", uri, ctx, body).await
    }

    pub async fn put_json(&self, uri: &str, ctx: &SecurityContext, body: &Value) -> Resp {
        send_json(&self.router, "PUT", uri, ctx, body).await
    }

    /// Any method with an explicit content type and raw body.
    pub async fn raw(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        content_type: Option<&str>,
        body: &str,
    ) -> Resp {
        send_body(&self.router, method, uri, Some(ctx), content_type, body).await
    }

    /// `POST /v1/chats` as `ctx`, asserting 201, returning the body.
    pub async fn create_chat(&self, ctx: &SecurityContext, body: &Value) -> Value {
        let resp = self.post_json(&self.chats_uri(), ctx, body).await;
        assert_eq!(resp.status.as_u16(), 201, "create failed: {}", resp.raw);
        resp.json()
    }

    pub async fn list_chats(&self, query: &str, ctx: &SecurityContext) -> Resp {
        let uri = if query.is_empty() {
            self.chats_uri()
        } else {
            format!("{}?{query}", self.chats_uri())
        };
        self.get(&uri, ctx).await
    }

    pub async fn list_messages(&self, chat: Uuid, query: &str, ctx: &SecurityContext) -> Resp {
        let uri = if query.is_empty() {
            self.messages_uri(chat)
        } else {
            format!("{}?{query}", self.messages_uri(chat))
        };
        self.get(&uri, ctx).await
    }

    // ── seeding (direct DB writes; sending messages belongs to a later slice) ──

    pub async fn seed_chat(&self, seed: ChatSeed) -> Uuid {
        let conn = self.db.conn().expect("db connection");
        secure_insert::<chats::Entity>(
            chats::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(seed.tenant_id),
                user_id: Set(seed.user_id),
                model: Set(seed.model),
                // Untitled chats are stored as the empty title (never NULL), like the repository does.
                title: Set(Some(seed.title.unwrap_or_default())),
                is_temporary: Set(false),
                created_at: Set(to_db(seed.created_at)),
                updated_at: Set(to_db(seed.updated_at)),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed chat");
        seed.id
    }

    pub async fn seed_message(&self, seed: MessageSeed) -> Uuid {
        let conn = self.db.conn().expect("db connection");
        secure_insert::<messages::Entity>(
            messages::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(seed.tenant_id),
                chat_id: Set(seed.chat_id),
                request_id: Set(seed.request_id),
                role: Set(seed.role),
                content: Set(seed.content),
                content_type: Set("text".to_owned()),
                token_estimate: Set(0),
                provider_response_id: Set(None),
                request_kind: Set("chat".to_owned()),
                features_used: Set(json!([])),
                input_tokens: Set(seed.input_tokens),
                output_tokens: Set(seed.output_tokens),
                cache_read_input_tokens: Set(0),
                cache_write_input_tokens: Set(0),
                reasoning_tokens: Set(0),
                model: Set(seed.model),
                is_compressed: Set(false),
                created_at: Set(to_db(seed.created_at)),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed message");
        seed.id
    }

    pub async fn seed_attachment(&self, seed: AttachmentSeed) -> Uuid {
        let conn = self.db.conn().expect("db connection");
        let is_image = seed.kind == "image";
        secure_insert::<attachments::Entity>(
            attachments::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(seed.tenant_id),
                chat_id: Set(seed.chat_id),
                uploaded_by_user_id: Set(USER_A),
                filename: Set(seed.filename),
                content_type: Set(Some(if is_image {
                    "image/png".to_owned()
                } else {
                    "application/pdf".to_owned()
                })),
                size_bytes: Set(Some(1234)),
                storage_backend: Set("openai".to_owned()),
                provider_file_id: Set(Some(format!("file-{}", seed.id.simple()))),
                status: Set(seed.status),
                error_code: Set(None),
                attachment_kind: Set(seed.kind),
                for_file_search: Set(!is_image),
                for_code_interpreter: Set(false),
                doc_summary: Set(None),
                img_thumbnail_width: Set(seed.thumbnail.as_ref().map(|_| THUMBNAIL_WIDTH)),
                img_thumbnail_height: Set(seed.thumbnail.as_ref().map(|_| THUMBNAIL_HEIGHT)),
                img_thumbnail: Set(seed.thumbnail),
                summary_model: Set(None),
                summary_updated_at: Set(None),
                cleanup_status: Set(seed.cleanup_status),
                cleanup_attempts: Set(0),
                last_cleanup_error: Set(None),
                cleanup_updated_at: Set(None),
                created_at: Set(to_db(ts(0))),
                updated_at: Set(to_db(ts(0))),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
                secondary_file_id: Set(None),
                secondary_status: Set("not_attempted".to_owned()),
                secondary_provider_kind: Set(None),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed attachment");
        seed.id
    }

    /// Link `attachment` to `message` (`message_attachments` row).
    pub async fn link_attachment(&self, chat: Uuid, message: Uuid, attachment: Uuid) {
        let conn = self.db.conn().expect("db connection");
        secure_insert::<message_attachments::Entity>(
            message_attachments::ActiveModel {
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                message_id: Set(message),
                attachment_id: Set(attachment),
                created_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("link attachment");
    }

    /// Insert a reaction row directly (`reaction` is `like` / `dislike`).
    pub async fn seed_reaction(&self, tenant: Uuid, user: Uuid, message: Uuid, reaction: &str) {
        let conn = self.db.conn().expect("db connection");
        secure_insert::<message_reactions::Entity>(
            message_reactions::ActiveModel {
                id: Set(Uuid::new_v4()),
                message_id: Set(message),
                user_id: Set(user),
                tenant_id: Set(tenant),
                reaction: Set(reaction.to_owned()),
                created_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed reaction");
    }

    // ── reading rows back (unscoped; the tests assert what the API did) ───────

    pub async fn all_chats(&self) -> Vec<chats::Model> {
        let conn = self.db.conn().expect("db connection");
        chats::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read chats")
    }

    pub async fn chat_row(&self, id: Uuid) -> Option<chats::Model> {
        self.all_chats().await.into_iter().find(|c| c.id == id)
    }

    pub async fn all_messages(&self) -> Vec<messages::Model> {
        let conn = self.db.conn().expect("db connection");
        messages::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read messages")
    }

    pub async fn all_attachments(&self) -> Vec<attachments::Model> {
        let conn = self.db.conn().expect("db connection");
        attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read attachments")
    }

    pub async fn attachment_row(&self, id: Uuid) -> attachments::Model {
        self.all_attachments()
            .await
            .into_iter()
            .find(|a| a.id == id)
            .expect("attachment row exists")
    }

    pub async fn all_reactions(&self) -> Vec<message_reactions::Model> {
        let conn = self.db.conn().expect("db connection");
        message_reactions::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read reactions")
    }

    pub async fn reactions_of(&self, message: Uuid) -> Vec<message_reactions::Model> {
        self.all_reactions()
            .await
            .into_iter()
            .filter(|r| r.message_id == message)
            .collect()
    }
}

// ── response helpers ─────────────────────────────────────────────────────────

/// `items` of a page body.
pub fn page_items(page: &Value) -> Vec<Value> {
    page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("page has no `items` array: {page}"))
        .clone()
}

/// `id` of every item of a page body, in order.
pub fn page_ids(page: &Value) -> Vec<String> {
    page_items(page)
        .iter()
        .map(|i| i["id"].as_str().expect("item id").to_owned())
        .collect()
}

pub fn ids_of(uuids: &[Uuid]) -> Vec<String> {
    uuids.iter().map(Uuid::to_string).collect()
}

/// `page_info.next_cursor` / `prev_cursor` (absent and `null` both mean none).
pub fn cursor_of(page: &Value, which: &str) -> Option<String> {
    page["page_info"][which].as_str().map(str::to_owned)
}

pub fn key_set(value: &Value) -> std::collections::BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object, got {value}"))
        .keys()
        .cloned()
        .collect()
}

pub fn keys(names: &[&str]) -> std::collections::BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}
