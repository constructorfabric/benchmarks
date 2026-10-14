//! Test doubles and the in-process REST harness of the chat-management feature: the five chat
//! routes registered over the real `Services`, a recording PDP, a fake model-policy gateway, an
//! in-memory `SQLite` database with the gear schema and the shared outbox pipeline, plus seeding
//! and raw inspection helpers that do not go through the code under test.
//!
//! If the author stage changes the constructors of `ChatsService` or `Services`, [`rig_with`]
//! is the only place to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    reason = "test support"
)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::PolicyEnforcer;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use axum::middleware::from_fn;
use mini_chat_sdk::{ModelTier, PolicySnapshot};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryResult};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit::api::operation_builder::OperationSpec;
use toolkit::api::{OpenApiRegistry, OpenApiRegistryImpl, canonical_error_middleware};
use toolkit_db::outbox::{OutboxHandle, Wake};
use toolkit_db::secure::{DbTx, SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use utoipa::openapi::RefOr;
use utoipa::openapi::schema::Schema;
use uuid::Uuid;

use super::{
    FakeGateway, Pdp, RecordingPdp, SUBJECT_USER_TYPE, TENANT_A, TENANT_B, TestDb, USER_A1,
    USER_A2, USER_B1, ctx_with, entry, raw_exec, raw_i64, snapshot, sqlite_db,
};
use crate::api::rest::register_routes;
use crate::config::OutboxConfig;
use crate::domain::authz::PepService;
use crate::domain::chats::{ChatCleanupMessage, ChatCleanupQueue, ChatsService};
use crate::domain::error::DomainError;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::services::Services;
use crate::infra::outbox::{OutboxEnqueuer, queue_specs, start_pipeline};
use crate::infra::storage::entity::{attachment, chat, message};
use crate::infra::storage::migrations::all_migrations;
use crate::infra::storage::repo_impl::{ChatRecord, ChatRepo, NewMessage};

/// `context.resource_type` of a chat not-found.
pub const CHAT_RESOURCE: &str = "gts.cf.core.mini_chat.chat.v1~";
/// `context.resource_type` of an OData query error.
pub const ODATA_RESOURCE: &str = "gts.cf.core.odata.query.v1~";

/// The text form of the timestamps written by raw seeding SQL.
const TS: &str = "2026-01-01 00:00:00+00:00";

// ── identities and requests ──────────────────────────────────────────────────

/// A caller of the given user and tenant, with the subject type and token scopes the gateway
/// would supply.
pub fn caller(user: Uuid, tenant: Uuid) -> SecurityContext {
    ctx_with(user, tenant, Some(SUBJECT_USER_TYPE), &["*"])
}

/// User 1 of tenant A.
pub fn a1() -> SecurityContext {
    caller(USER_A1, TENANT_A)
}

/// User 2 of tenant A.
pub fn a2() -> SecurityContext {
    caller(USER_A2, TENANT_A)
}

/// User 1 of tenant B.
pub fn b1() -> SecurityContext {
    caller(USER_B1, TENANT_B)
}

/// A request without a body.
pub fn request(method: &str, uri: &str, who: &SecurityContext) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-request-id", "req-42")
        .body(Body::empty())
        .unwrap();
    request.extensions_mut().insert(who.clone());
    request
}

/// A request with a body and the given `Content-Type` (`None` sends no content type).
pub fn request_with_body(
    method: &str,
    uri: &str,
    who: &SecurityContext,
    content_type: Option<&str>,
    body: &str,
) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-request-id", "req-42");
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    let mut request = builder.body(Body::from(body.to_owned())).unwrap();
    request.extensions_mut().insert(who.clone());
    request
}

/// A JSON request.
pub fn json_request(method: &str, uri: &str, who: &SecurityContext, body: &str) -> Request<Body> {
    request_with_body(method, uri, who, Some("application/json"), body)
}

/// A recorded HTTP reply.
pub struct Reply {
    pub status: StatusCode,
    pub content_type: String,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub text: String,
}

impl Reply {
    /// The body as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text)
            .unwrap_or_else(|e| panic!("body is not JSON ({e}): {:?}", self.text))
    }
}

// ── OpenAPI recorder ─────────────────────────────────────────────────────────

/// An `OpenApiRegistry` that records every registered operation.
pub struct RecordingOpenApi {
    pub inner: OpenApiRegistryImpl,
    pub ops: Mutex<Vec<OperationSpec>>,
}

impl OpenApiRegistry for RecordingOpenApi {
    fn register_operation(&self, spec: &OperationSpec) {
        self.ops.lock().unwrap().push(spec.clone());
        self.inner.register_operation(spec);
    }

    fn ensure_schema_raw(&self, name: &str, schemas: Vec<(String, RefOr<Schema>)>) -> String {
        self.inner.ensure_schema_raw(name, schemas)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl RecordingOpenApi {
    /// The recorded operation of `method` and `path`.
    pub fn op(&self, method: &str, path: &str) -> Option<OperationSpec> {
        self.ops
            .lock()
            .unwrap()
            .iter()
            .find(|op| op.method.as_str() == method && op.path == path)
            .cloned()
    }
}

// ── chat-cleanup queue doubles ───────────────────────────────────────────────

/// What the chat-cleanup queue of the rig is.
pub enum Queue {
    /// The real transactional enqueuer over a running outbox pipeline.
    Real,
    /// The real enqueuer that was never bound to an outbox (every enqueue fails internally).
    Unbound,
    /// A double.
    Custom(Arc<dyn ChatCleanupQueue>),
}

/// A queue double whose enqueue always fails with the error built by `make`.
pub struct FailingQueue {
    make: fn() -> DomainError,
    pub calls: Mutex<Vec<ChatCleanupMessage>>,
}

impl FailingQueue {
    pub fn new(make: fn() -> DomainError) -> Arc<Self> {
        Arc::new(Self {
            make,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// The oversized-payload failure of the chat delete call site.
    pub fn payload_too_large() -> Arc<Self> {
        Self::new(|| DomainError::OutboxPayloadTooLarge {
            site: crate::domain::error::EnqueueSite::ChatDelete,
        })
    }

    /// A generic internal failure.
    pub fn internal() -> Arc<Self> {
        Self::new(|| DomainError::internal("forced enqueue failure with secret-queue-detail"))
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl ChatCleanupQueue for FailingQueue {
    async fn enqueue_chat_cleanup(
        &self,
        _tx: &DbTx<'_>,
        message: &ChatCleanupMessage,
    ) -> Result<Wake, DomainError> {
        self.calls.lock().unwrap().push(message.clone());
        Err((self.make)())
    }
}

// ── the rig ──────────────────────────────────────────────────────────────────

/// The model catalog of the rig: `gpt-standard` (enabled, first), `gpt-premium` (enabled, the
/// default flag), `gpt-hidden` (disabled).
pub fn catalog() -> PolicySnapshot {
    snapshot(vec![
        entry("gpt-standard", ModelTier::Standard, true, &json!({})),
        entry(
            "gpt-premium",
            ModelTier::Premium,
            true,
            &json!({ "preference": { "is_default": true, "sort_order": 1 } }),
        ),
        entry("gpt-hidden", ModelTier::Standard, false, &json!({})),
    ])
}

/// How to build a [`ChatRig`].
pub struct RigOptions {
    pub pdp: Pdp,
    pub gateway: Arc<FakeGateway>,
    pub prefix: &'static str,
    pub queue: Queue,
    pub outbox: OutboxConfig,
}

impl RigOptions {
    pub fn new(pdp: Pdp) -> Self {
        Self {
            pdp,
            gateway: FakeGateway::serving(catalog()),
            prefix: "/mini-chat",
            queue: Queue::Real,
            outbox: OutboxConfig::default(),
        }
    }
}

/// The in-process REST harness of the chat endpoints.
pub struct ChatRig {
    pub router: Router,
    pub openapi: Arc<RecordingOpenApi>,
    pub pdp: Arc<RecordingPdp>,
    pub gateway: Arc<FakeGateway>,
    pub db: TestDb,
    pub enqueuer: Arc<OutboxEnqueuer>,
    pub outbox: OutboxConfig,
    _pipeline: Option<OutboxHandle>,
}

/// A rig with the default catalog, the real queue and the given PDP behavior.
pub async fn rig(pdp: Pdp) -> ChatRig {
    rig_with(RigOptions::new(pdp)).await
}

/// A rig built from `options`.
pub async fn rig_with(options: RigOptions) -> ChatRig {
    let db = sqlite_db(all_migrations()).await;
    let specs = queue_specs(&options.outbox, 300);
    let enqueuer = Arc::new(OutboxEnqueuer::new(specs.clone()));
    let pipeline = if matches!(options.queue, Queue::Real) {
        let handle = start_pipeline(db.db.clone(), &specs)
            .await
            .expect("pipeline starts");
        enqueuer.bind(handle.outbox());
        Some(handle)
    } else {
        None
    };
    let cleanup: Arc<dyn ChatCleanupQueue> = match options.queue {
        Queue::Custom(queue) => queue,
        Queue::Real | Queue::Unbound => enqueuer.clone(),
    };
    let pdp = RecordingPdp::new(options.pdp);
    let pep = Arc::new(PepService::new(PolicyEnforcer::new(pdp.clone())));
    let gateway: Arc<dyn ModelPolicyGateway> = options.gateway.clone();
    let chats = ChatsService::new(
        Arc::clone(&pep),
        Arc::clone(&gateway),
        Arc::clone(&db.provider),
        cleanup,
    );
    let services = Arc::new(Services::new(pep, gateway).with_chats(chats));
    let openapi = Arc::new(RecordingOpenApi {
        inner: OpenApiRegistryImpl::new(),
        ops: Mutex::new(Vec::new()),
    });
    let router = register_routes(Router::new(), openapi.as_ref(), options.prefix, services)
        .layer(from_fn(canonical_error_middleware));
    ChatRig {
        router,
        openapi,
        pdp,
        gateway: options.gateway,
        db,
        enqueuer,
        outbox: options.outbox,
        _pipeline: pipeline,
    }
}

impl ChatRig {
    /// Send one request through the router.
    pub async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };
        let content_type = header("content-type").unwrap_or_default();
        let location = header("location");
        let retry_after = header("retry-after");
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        Reply {
            status,
            content_type,
            location,
            retry_after,
            text: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    /// `GET uri` as `who`.
    pub async fn get(&self, uri: &str, who: &SecurityContext) -> Reply {
        self.send(request("GET", uri, who)).await
    }

    /// `DELETE uri` as `who`.
    pub async fn delete(&self, uri: &str, who: &SecurityContext) -> Reply {
        self.send(request("DELETE", uri, who)).await
    }

    /// `POST uri` with a JSON body as `who`.
    pub async fn post_json(&self, uri: &str, who: &SecurityContext, body: &str) -> Reply {
        self.send(json_request("POST", uri, who, body)).await
    }

    /// `PATCH uri` with a JSON body as `who`.
    pub async fn patch_json(&self, uri: &str, who: &SecurityContext, body: &str) -> Reply {
        self.send(json_request("PATCH", uri, who, body)).await
    }

    /// Seed a live chat of `who` (the repository insert, not the endpoint) with `updated_at`
    /// `at(0)`.
    pub async fn seed(
        &self,
        who: &SecurityContext,
        title: Option<&str>,
        model: &str,
    ) -> ChatRecord {
        self.seed_at(who, Uuid::new_v4(), title, model, at(0)).await
    }

    /// Seed a live chat of `who` with a chosen id and `created_at = updated_at = when`.
    pub async fn seed_at(
        &self,
        who: &SecurityContext,
        id: Uuid,
        title: Option<&str>,
        model: &str,
        when: OffsetDateTime,
    ) -> ChatRecord {
        seed_chat(&self.db, who, id, title, model, when).await
    }

    /// The chat-cleanup rows of the outbox.
    pub async fn outbox_rows(&self) -> Vec<OutboxRow> {
        outbox_rows(&self.db).await
    }

    /// The rows of the chat-cleanup queue of the configured name.
    pub async fn cleanup_rows(&self) -> Vec<OutboxRow> {
        let queue = self.outbox.chat_cleanup_queue_name.clone();
        self.outbox_rows()
            .await
            .into_iter()
            .filter(|row| row.queue == queue)
            .collect()
    }
}

// ── time ─────────────────────────────────────────────────────────────────────

/// A fixed instant `secs` seconds after 2023-11-14T22:13:20Z, far in the past, so rows written
/// by the code under test (`now`) are always newer.
pub fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

/// `at(secs)` in the RFC 3339 form an OData `$filter` literal takes.
pub fn at_rfc3339(secs: i64) -> String {
    at(secs)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// Parse an RFC 3339 timestamp of a response body.
pub fn parse_ts(value: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(
        value
            .as_str()
            .unwrap_or_else(|| panic!("not a string: {value}")),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap_or_else(|e| panic!("not RFC 3339 ({e}): {value}"))
}

// ── seeding ──────────────────────────────────────────────────────────────────

/// The owner-scoped access scope of a caller.
pub fn owner_scope(who: &SecurityContext) -> AccessScope {
    AccessScope::for_tenant(who.subject_tenant_id()).ensure_owner(who.subject_id())
}

/// Insert a live chat directly through the Secure ORM, with explicit timestamps.
pub async fn seed_chat(
    db: &TestDb,
    who: &SecurityContext,
    id: Uuid,
    title: Option<&str>,
    model: &str,
    when: OffsetDateTime,
) -> ChatRecord {
    let conn = db.provider.conn().unwrap();
    let row = chat::ActiveModel {
        id: sea_orm::Set(id),
        tenant_id: sea_orm::Set(who.subject_tenant_id()),
        user_id: sea_orm::Set(who.subject_id()),
        model: sea_orm::Set(model.to_owned()),
        title: sea_orm::Set(title.map(ToOwned::to_owned)),
        is_temporary: sea_orm::Set(false),
        created_at: sea_orm::Set(when),
        updated_at: sea_orm::Set(when),
        deleted_at: sea_orm::Set(None),
    };
    let inserted = secure_insert::<chat::Entity>(row, &owner_scope(who), &conn)
        .await
        .expect("seed chat");
    ChatRecord {
        id: inserted.id,
        tenant_id: inserted.tenant_id,
        user_id: inserted.user_id,
        model: inserted.model,
        title: inserted.title,
        created_at: inserted.created_at,
        updated_at: inserted.updated_at,
    }
}

/// Insert a message of `role` into a seeded chat (the existing repository insert).
pub async fn seed_message(db: &TestDb, chat: &ChatRecord, role: &str) -> Uuid {
    let id = Uuid::new_v4();
    ChatRepo::add_message(
        &db.provider.conn().unwrap(),
        chat,
        NewMessage {
            id,
            request_id: None,
            role: role.to_owned(),
            content: "text".to_owned(),
        },
    )
    .await
    .expect("seed message");
    id
}

/// Insert a user message into a seeded chat inside the caller's transaction.
pub async fn seed_message_in(tx: &DbTx<'_>, chat: &ChatRecord) -> Result<(), DomainError> {
    ChatRepo::add_message(
        tx,
        chat,
        NewMessage {
            id: Uuid::new_v4(),
            request_id: None,
            role: "user".to_owned(),
            content: "hello".to_owned(),
        },
    )
    .await
    .map(|_| ())
}

/// Insert an attachment of `kind` (`document` or `image`) with the given `cleanup_status`.
pub async fn seed_attachment(
    db: &TestDb,
    chat: &ChatRecord,
    kind: &str,
    cleanup_status: Option<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    let status = cleanup_status.map_or("NULL".to_owned(), |s| format!("'{s}'"));
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             status, attachment_kind, cleanup_status, created_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'f.bin', 'ready', '{kind}', {status}, '{TS}', '{TS}')",
            blob(id),
            blob(chat.tenant_id),
            blob(chat.id),
            blob(chat.user_id),
        ),
    )
    .await
    .expect("seed attachment");
    id
}

/// Insert a running turn of the chat; returns its id.
pub async fn seed_running_turn(db: &TestDb, chat: &ChatRecord) -> Uuid {
    let id = Uuid::new_v4();
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO chat_turns (id, tenant_id, chat_id, request_id, requester_type, \
             requester_user_id, state, started_at, updated_at) \
             VALUES ({}, {}, {}, {}, 'user', {}, 'running', '{TS}', '{TS}')",
            blob(id),
            blob(chat.tenant_id),
            blob(chat.id),
            blob(Uuid::new_v4()),
            blob(chat.user_id),
        ),
    )
    .await
    .expect("seed turn");
    id
}

/// Insert a reaction of the chat owner on `message_id`.
pub async fn seed_reaction(db: &TestDb, chat: &ChatRecord, message_id: Uuid) {
    raw_exec(
        &db.raw,
        &format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, \
             created_at) VALUES ({}, {}, {}, {}, 'like', '{TS}')",
            blob(Uuid::new_v4()),
            blob(message_id),
            blob(chat.user_id),
            blob(chat.tenant_id),
        ),
    )
    .await
    .expect("seed reaction");
}

/// The SQL blob literal of a UUID (SQLite stores UUIDs as 16-byte blobs).
pub fn blob(id: Uuid) -> String {
    format!("x'{}'", id.simple())
}

// ── direct inspection (no code under test) ───────────────────────────────────

/// The row of `chat_id`, whatever its state, or `None`.
pub async fn chat_row(db: &TestDb, chat_id: Uuid) -> Option<chat::Model> {
    let conn = db.provider.conn().unwrap();
    chat::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(chat::Column::Id.eq(chat_id)))
        .one(&conn)
        .await
        .unwrap()
}

/// Every attachment row of the chat (including soft-deleted ones), ordered by id.
pub async fn attachment_rows(db: &TestDb, chat_id: Uuid) -> Vec<attachment::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = attachment::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(attachment::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| row.id);
    rows
}

/// Every message row of the chat (including soft-deleted ones), ordered by id.
pub async fn message_rows(db: &TestDb, chat_id: Uuid) -> Vec<message::Model> {
    let conn = db.provider.conn().unwrap();
    let mut rows = message::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)))
        .all(&conn)
        .await
        .unwrap();
    rows.sort_by_key(|row| row.id);
    rows
}

/// Soft-delete one message row directly.
pub async fn soft_delete_message(db: &TestDb, message_id: Uuid) {
    raw_exec(
        &db.raw,
        &format!(
            "UPDATE messages SET deleted_at = '{TS}' WHERE id = {}",
            blob(message_id)
        ),
    )
    .await
    .expect("soft-delete message");
}

/// The live (not soft-deleted) row counts of the children of a chat that the delete transaction
/// must leave alone: `(messages, turns, reactions)`, plus the rows of those tables that have a
/// `deleted_at` set.
pub async fn children_counts(db: &TestDb, chat_id: Uuid) -> (i64, i64, i64, i64) {
    let id = blob(chat_id);
    let messages = raw_i64(
        &db.raw,
        &format!("SELECT COUNT(*) FROM messages WHERE chat_id = {id} AND deleted_at IS NULL"),
    )
    .await;
    let turns = raw_i64(
        &db.raw,
        &format!("SELECT COUNT(*) FROM chat_turns WHERE chat_id = {id} AND deleted_at IS NULL"),
    )
    .await;
    let reactions = raw_i64(
        &db.raw,
        &format!(
            "SELECT COUNT(*) FROM message_reactions WHERE message_id IN \
             (SELECT id FROM messages WHERE chat_id = {id})"
        ),
    )
    .await;
    let marked_deleted = raw_i64(
        &db.raw,
        &format!(
            "SELECT (SELECT COUNT(*) FROM messages WHERE chat_id = {id} AND deleted_at IS NOT NULL) \
             + (SELECT COUNT(*) FROM chat_turns WHERE chat_id = {id} AND deleted_at IS NOT NULL) \
             + (SELECT COUNT(*) FROM attachments WHERE chat_id = {id} AND deleted_at IS NOT NULL)"
        ),
    )
    .await;
    (messages, turns, reactions, marked_deleted)
}

/// One message of the shared outbox as stored.
#[derive(Debug, Clone)]
pub struct OutboxRow {
    pub queue: String,
    pub partition: i64,
    pub payload_type: String,
    pub payload: Value,
}

/// Every message of the shared outbox, in enqueue order.
pub async fn outbox_rows(db: &TestDb) -> Vec<OutboxRow> {
    use sea_orm::ConnectionTrait;
    let statement = sea_orm::Statement::from_string(
        db.raw.get_database_backend(),
        "SELECT p.queue, p.partition, b.payload_type, b.payload \
         FROM toolkit_outbox_body b \
         JOIN (SELECT partition_id, body_id FROM toolkit_outbox_incoming \
               UNION ALL SELECT partition_id, body_id FROM toolkit_outbox_outgoing) m \
           ON m.body_id = b.id \
         JOIN toolkit_outbox_partitions p ON p.id = m.partition_id \
         ORDER BY b.id",
    );
    let rows: Vec<QueryResult> = db.raw.query_all_raw(statement).await.expect("outbox query");
    rows.into_iter()
        .map(|row| {
            let payload: Vec<u8> = row.try_get_by_index(3).expect("payload");
            OutboxRow {
                queue: row.try_get_by_index(0).expect("queue"),
                partition: row.try_get_by_index(1).expect("partition"),
                payload_type: row.try_get_by_index(2).expect("payload_type"),
                payload: serde_json::from_slice(&payload)
                    .unwrap_or_else(|e| panic!("payload is not JSON ({e})")),
            }
        })
        .collect()
}

/// The number of outbox bodies, whatever the queue.
pub async fn outbox_body_count(db: &TestDb) -> i64 {
    raw_i64(&db.raw, "SELECT COUNT(*) FROM toolkit_outbox_body").await
}

/// Make every UPDATE of `attachments` fail (a database failure in the marking step).
pub async fn fail_attachment_updates(db: &TestDb) {
    raw_exec(
        &db.raw,
        "CREATE TRIGGER fail_attachment_update BEFORE UPDATE ON attachments \
         BEGIN SELECT RAISE(ABORT, 'forced attachment failure'); END",
    )
    .await
    .expect("create trigger");
}

/// Make every INSERT into the outbox body table fail (a database failure in the enqueue step).
pub async fn fail_outbox_inserts(db: &TestDb) {
    raw_exec(
        &db.raw,
        "CREATE TRIGGER fail_outbox_insert BEFORE INSERT ON toolkit_outbox_body \
         BEGIN SELECT RAISE(ABORT, 'forced outbox failure'); END",
    )
    .await
    .expect("create trigger");
}

// ── response assertions ──────────────────────────────────────────────────────

/// Assert a Problem reply: status, `application/problem+json`, the category in `type`, the
/// contract fields and no top-level `code`. Returns the body.
pub fn assert_problem(reply: &Reply, status: u16, category: &str) -> Value {
    assert_eq!(reply.status.as_u16(), status, "{}", reply.text);
    assert_eq!(
        reply.content_type, "application/problem+json",
        "{}",
        reply.text
    );
    let body = reply.json();
    for key in [
        "type", "title", "status", "detail", "instance", "trace_id", "context",
    ] {
        assert!(body.get(key).is_some(), "missing `{key}` in {body}");
    }
    assert!(body.get("code").is_none(), "top-level code in {body}");
    assert_eq!(body["status"], status);
    assert!(
        body["type"].as_str().unwrap().contains(category),
        "type {} is not {category}",
        body["type"]
    );
    body
}

/// The `context.field_violations` entry of `field`.
pub fn violation<'a>(problem: &'a Value, field: &str) -> &'a Value {
    problem["context"]["field_violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no field_violations in {problem}"))
        .iter()
        .find(|v| v["field"] == field)
        .unwrap_or_else(|| panic!("no violation for `{field}` in {problem}"))
}

/// Assert a 400 `invalid_argument` with `field_violations[field].reason == reason` on the
/// chat resource type.
pub fn assert_invalid_argument(reply: &Reply, status: u16, field: &str, reason: &str) -> Value {
    let body = assert_problem(reply, status, "invalid_argument");
    assert_eq!(violation(&body, field)["reason"], reason, "{body}");
    body
}

/// Assert a 400 OData query error: `context.resource_type` is the OData query type and a
/// violation with `reason` (on `field`) exists.
pub fn assert_odata_error(reply: &Reply, field: &str, reason: &str) -> Value {
    let body = assert_problem(reply, 400, "invalid_argument");
    assert_eq!(body["context"]["resource_type"], ODATA_RESOURCE, "{body}");
    assert_eq!(violation(&body, field)["reason"], reason, "{body}");
    body
}

/// Assert a 404 `not_found` of the chat resource type.
pub fn assert_chat_not_found(reply: &Reply) -> Value {
    let body = assert_problem(reply, 404, "not_found");
    assert_eq!(body["context"]["resource_type"], CHAT_RESOURCE, "{body}");
    body
}

/// The field set of a `ChatDetail` body.
pub fn detail_keys(titled: bool) -> BTreeSet<&'static str> {
    let mut keys = BTreeSet::from([
        "id",
        "model",
        "is_temporary",
        "message_count",
        "created_at",
        "updated_at",
    ]);
    if titled {
        keys.insert("title");
    }
    keys
}

/// The keys of a JSON object.
pub fn keys(value: &Value) -> BTreeSet<&str> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .map(String::as_str)
        .collect()
}

/// The ids of the items of a list body, in order.
pub fn item_ids(body: &Value) -> Vec<Uuid> {
    body["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items in {body}"))
        .iter()
        .map(|item| Uuid::parse_str(item["id"].as_str().unwrap()).unwrap())
        .collect()
}

/// The URI of the chat collection.
pub const CHATS: &str = "/mini-chat/v1/chats";

/// The URI of one chat.
pub fn chat_uri(id: Uuid) -> String {
    format!("{CHATS}/{id}")
}
