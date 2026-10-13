#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
//! Fixture of the chat CRUD tests (`cpt-cf-mini-chat-feature-chat-crud`): a
//! migrated `SQLite` database, the chat and message services over a fake PDP and
//! a fake model-policy plugin, the REST router registered like the gear does,
//! an optional running outbox pipeline, seeding helpers and raw-SQL readers.
//!
//! Services are built in [`ChatEnv::with`] only; a change of a service
//! constructor is made there.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::models::EvaluationRequest;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelCatalogEntry};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxHandle, OutboxMessage};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_chat_routes;
use crate::config::{OutboxConfig, ThreadSummaryWorkerConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::infra::db::repos::chats::SeaChatRepository;
use crate::infra::db::repos::messages::SeaMessageRepository;
use crate::infra::db::test_support::{Row, TestDb, attachment_row, chat_row, message_row};
use crate::infra::outbox::queues::{OutboxEnqueuer, OutboxHandlers, start_outbox};
use crate::test_doubles::{
    FakePdp, FakePolicyPlugin, PdpReply, PluginWorld, USER, USER_TENANT, entry, entry_json,
};

/// Default `url_prefix`.
pub const PREFIX: &str = "/mini-chat";
/// `context.resource_type` of a missing chat.
pub const CHAT_TYPE: &str = "gts.cf.core.mini_chat.chat.v1~";
/// `context.resource_type` of a missing message.
pub const MESSAGE_TYPE: &str = "gts.cf.core.mini_chat.message.v1~";
/// `context.resource_type` of every `OData` query error.
pub const ODATA_TYPE: &str = "gts.cf.core.odata.query.v1~";
/// PEP resource type of the chat operations.
pub const CHAT_PEP_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
/// Queue of the chat-cleanup messages with the default configuration.
pub const CHAT_CLEANUP_QUEUE: &str = "mini-chat.chat_cleanup";

/// What the outbox of the fixture looks like.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutboxMode {
    /// The pipeline is not started and the enqueuer is not bound.
    NotBound,
    /// The pipeline of the gear runs with the default configuration.
    Running,
    /// The pipeline runs, but the enqueuer targets a queue that is not
    /// registered, so every enqueue fails after the database writes.
    QueueMismatch,
}

/// Fixture options.
#[derive(Clone)]
pub struct Options {
    /// Answer of the fake PDP.
    pub pdp: PdpReply,
    /// Outbox mode.
    pub outbox: OutboxMode,
    /// `url_prefix` the routes are registered under.
    pub prefix: &'static str,
    /// Catalog served by the fake policy plugin.
    pub catalog: Vec<ModelCatalogEntry>,
    /// Whether the fake policy plugin is registered in types-registry.
    pub register_plugin: bool,
    /// `outbox.chat_cleanup_queue_name` (pipeline and enqueuer); `None` keeps
    /// the default.
    pub chat_cleanup_queue: Option<&'static str>,
    /// Use a file-backed `SQLite` database (WAL, pool of several connections,
    /// like the server) instead of the shared in-memory one.
    pub file_db: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            pdp: PdpReply::AllowScoped,
            outbox: OutboxMode::NotBound,
            prefix: PREFIX,
            catalog: default_catalog(),
            register_plugin: true,
            chat_cleanup_queue: None,
            file_db: false,
        }
    }
}

impl Options {
    /// Default options with a running outbox.
    pub fn with_outbox() -> Self {
        Self {
            outbox: OutboxMode::Running,
            ..Self::default()
        }
    }

    /// Default options with a running outbox on a file-backed database.
    pub fn with_file_db() -> Self {
        Self {
            file_db: true,
            ..Self::with_outbox()
        }
    }

    /// Default options with the given PDP answer.
    pub fn with_pdp(pdp: PdpReply) -> Self {
        Self {
            pdp,
            ..Self::default()
        }
    }
}

/// Catalog entry: `default` is `None` for an absent `preference`.
pub fn catalog_entry(id: &str, enabled: bool, default: Option<bool>) -> ModelCatalogEntry {
    let mut value = entry_json(id, enabled);
    value["preference"] = match default {
        Some(is_default) => json!({ "is_default": is_default, "sort_order": 1 }),
        None => Value::Null,
    };
    entry(value)
}

/// Catalog of the fixture: `model-off` (disabled default), `model-a` (enabled,
/// no preference) and `model-b` (enabled, not default). The default model is
/// therefore `model-a`.
pub fn default_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry("model-off", false, Some(true)),
        catalog_entry("model-a", true, None),
        catalog_entry("model-b", true, Some(false)),
    ]
}

/// A started outbox handler that acknowledges everything (usage and audit).
struct AckHandler;

#[async_trait]
impl LeasedMessageHandler for AckHandler {
    async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
        MessageResult::Ok
    }
}

/// One HTTP reply.
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub text: String,
    pub json: Value,
}

impl Reply {
    /// A response header as text.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// `context.field_violations` of an `invalid_argument` problem.
    pub fn field_violations(&self) -> Vec<Value> {
        self.json["context"]["field_violations"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// `(field, reason)` of the first field violation.
    pub fn first_violation(&self) -> (String, String) {
        let violations = self.field_violations();
        let first = violations
            .first()
            .unwrap_or_else(|| panic!("no field violation in: {}", self.text));
        (
            first["field"].as_str().unwrap_or_default().to_owned(),
            first["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    /// `context.resource_type`.
    pub fn resource_type(&self) -> &str {
        self.json["context"]["resource_type"]
            .as_str()
            .unwrap_or_default()
    }

    /// The canonical category in `type` (for example `not_found`).
    pub fn category(&self) -> &str {
        self.json["type"].as_str().unwrap_or_default()
    }

    /// Asserts a 400 `OData` `invalid_argument` with `reason` and the `OData`
    /// resource type.
    pub fn assert_odata_error(&self, reason: &str) {
        assert_eq!(self.status, StatusCode::BAD_REQUEST, "{}", self.text);
        assert!(
            self.category().contains("invalid_argument"),
            "{}",
            self.text
        );
        assert_eq!(self.resource_type(), ODATA_TYPE, "{}", self.text);
        let reasons: Vec<String> = self
            .field_violations()
            .iter()
            .map(|v| v["reason"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(
            reasons.iter().any(|r| r == reason),
            "expected reason {reason}, got {reasons:?}: {}",
            self.text
        );
    }

    /// Asserts a 404 `not_found` of `resource_type`.
    pub fn assert_not_found(&self, resource_type: &str) {
        assert_eq!(self.status, StatusCode::NOT_FOUND, "{}", self.text);
        assert!(self.category().contains("not_found"), "{}", self.text);
        assert_eq!(self.resource_type(), resource_type, "{}", self.text);
    }

    /// Asserts a 400 `invalid_argument` whose first violation is
    /// `(field, reason)`.
    pub fn assert_invalid(&self, field: &str, reason: &str) {
        assert_eq!(self.status, StatusCode::BAD_REQUEST, "{}", self.text);
        assert!(
            self.category().contains("invalid_argument"),
            "{}",
            self.text
        );
        assert_eq!(
            self.first_violation(),
            (field.to_owned(), reason.to_owned()),
            "{}",
            self.text
        );
    }
}

/// One row of the chat-cleanup queue as stored by the outbox.
pub struct OutboxRow {
    pub queue: String,
    pub partition: i64,
    pub payload: Vec<u8>,
}

impl OutboxRow {
    /// The payload as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.payload).expect("the payload is JSON")
    }
}

/// The fixture.
pub struct ChatEnv {
    pub db: TestDb,
    pub pdp: Arc<FakePdp>,
    pub plugin: Arc<FakePolicyPlugin>,
    pub openapi: OpenApiRegistryImpl,
    pub router: Router,
    pub enqueuer: Arc<OutboxEnqueuer>,
    pub outbox_config: OutboxConfig,
    outbox: Option<OutboxHandle>,
}

impl ChatEnv {
    /// Default options (scoped PDP, outbox not bound).
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    /// Default options with the outbox pipeline running.
    pub async fn running_outbox() -> Self {
        Self::with(Options::with_outbox()).await
    }

    /// Builds the fixture.
    pub async fn with(options: Options) -> Self {
        let db = if options.file_db {
            TestDb::migrated_file().await
        } else {
            TestDb::migrated().await
        };
        let world = PluginWorld::new();
        let plugin = FakePolicyPlugin::with_catalog(options.catalog.clone());
        if options.register_plugin {
            world
                .register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
        }
        let pdp = FakePdp::new(options.pdp);
        let authz = Arc::new(AuthzService::new(
            Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>
        ));
        let gateway = Arc::new(
            world
                .policy_gateway()
                .with_call_timeout(Duration::from_millis(200)),
        );
        let provider = Arc::new(DBProvider::<DomainError>::new(db.db.clone()));

        let mut outbox_config = OutboxConfig {
            num_partitions: 4,
            ..OutboxConfig::default()
        };
        if let Some(queue) = options.chat_cleanup_queue {
            queue.clone_into(&mut outbox_config.chat_cleanup_queue_name);
        }
        let mut enqueuer_config = outbox_config.clone();
        if options.outbox == OutboxMode::QueueMismatch {
            "mini-chat.not_registered".clone_into(&mut enqueuer_config.chat_cleanup_queue_name);
        }
        let enqueuer = Arc::new(OutboxEnqueuer::new(&enqueuer_config));
        let outbox = if options.outbox == OutboxMode::NotBound {
            None
        } else {
            let handle = start_outbox(
                &db.db,
                &outbox_config,
                &ThreadSummaryWorkerConfig::default(),
                OutboxHandlers {
                    usage: Arc::new(AckHandler),
                    audit: Arc::new(AckHandler),
                    attachment_cleanup: None,
                    chat_cleanup: None,
                    thread_summary: None,
                },
            )
            .await
            .expect("the outbox pipeline starts");
            assert!(enqueuer.bind(handle.outbox()));
            Some(handle)
        };

        let chats = Arc::new(ChatsService::new(
            Arc::clone(&authz),
            gateway as Arc<dyn PolicyCatalog>,
            Arc::new(SeaChatRepository::new(
                Arc::clone(&provider),
                Arc::clone(&enqueuer),
            )),
        ));
        let messages = Arc::new(MessagesService::new(
            authz,
            Arc::new(SeaMessageRepository::new(provider)),
        ));
        let openapi = OpenApiRegistryImpl::new();
        let router = register_chat_routes(Router::new(), &openapi, chats, messages, options.prefix);
        Self {
            db,
            pdp,
            plugin,
            openapi,
            router,
            enqueuer,
            outbox_config,
            outbox,
        }
    }

    // ---- requests -------------------------------------------------------------------------

    /// Sends a request as `ctx`; `content_type` and `body` as given.
    pub async fn send_raw(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        content_type: Option<&str>,
        body: Option<String>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let mut request = builder
            .body(body.map_or_else(Body::empty, Body::from))
            .unwrap();
        request.extensions_mut().insert(ctx.clone());
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 4 << 20).await.unwrap();
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply {
            status,
            headers,
            text,
            json,
        }
    }

    /// Sends a JSON body as `ctx`.
    pub async fn send_json(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: &Value,
    ) -> Reply {
        self.send_raw(
            method,
            uri,
            ctx,
            Some("application/json"),
            Some(body.to_string()),
        )
        .await
    }

    /// `GET` as the default user.
    pub async fn get(&self, uri: &str) -> Reply {
        self.send_raw("GET", uri, &crate::test_doubles::user_ctx(), None, None)
            .await
    }

    /// `GET` as `ctx`.
    pub async fn get_as(&self, ctx: &SecurityContext, uri: &str) -> Reply {
        self.send_raw("GET", uri, ctx, None, None).await
    }

    /// `DELETE` as the default user.
    pub async fn delete(&self, uri: &str) -> Reply {
        self.send_raw("DELETE", uri, &crate::test_doubles::user_ctx(), None, None)
            .await
    }

    /// `POST /v1/chats` as the default user.
    pub async fn post_chat(&self, body: &Value) -> Reply {
        self.send_json(
            "POST",
            "/mini-chat/v1/chats",
            &crate::test_doubles::user_ctx(),
            body,
        )
        .await
    }

    /// `PATCH /v1/chats/{id}` as the default user.
    pub async fn patch_chat(&self, id: Uuid, body: &Value) -> Reply {
        self.send_json(
            "PATCH",
            &chat_uri(id),
            &crate::test_doubles::user_ctx(),
            body,
        )
        .await
    }

    /// `PUT .../reaction` as the default user.
    pub async fn put_reaction(&self, chat: Uuid, message: Uuid, reaction: &str) -> Reply {
        self.send_json(
            "PUT",
            &reaction_uri(chat, message),
            &crate::test_doubles::user_ctx(),
            &json!({ "reaction": reaction }),
        )
        .await
    }

    /// `DELETE .../reaction` as the default user.
    pub async fn delete_reaction(&self, chat: Uuid, message: Uuid) -> Reply {
        self.delete(&reaction_uri(chat, message)).await
    }

    /// `GET /v1/chats/{id}/messages{query}` as the default user (`query`
    /// starts with `?` or is empty).
    pub async fn list_messages(&self, chat: Uuid, query: &str) -> Reply {
        self.get(&format!("{}/messages{query}", chat_uri(chat)))
            .await
    }

    /// `GET /v1/chats{query}` as the default user.
    pub async fn list_chats(&self, query: &str) -> Reply {
        self.get(&format!("/mini-chat/v1/chats{query}")).await
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// Inserts a chat of the default user and tenant; `updated` is seconds
    /// after [`BASE_UNIX`].
    pub async fn seed_chat(&self, n: u128, title: Option<&str>, updated: i64) -> Uuid {
        self.seed_chat_of(n, USER_TENANT, USER, title, updated)
            .await
    }

    /// Inserts a chat of `(tenant, user)`.
    pub async fn seed_chat_of(
        &self,
        n: u128,
        tenant: Uuid,
        user: Uuid,
        title: Option<&str>,
        updated: i64,
    ) -> Uuid {
        let id = chat_id(n);
        let mut row = chat_row(id, tenant, user)
            .set("created_at", ts(0))
            .set("updated_at", ts(updated));
        row = match title {
            Some(title) => row.set("title", title),
            None => row.null("title"),
        };
        self.db.insert("chats", row).await.expect("seed chat");
        id
    }

    /// Inserts a message of the default tenant with a unique `request_id`;
    /// `created` is seconds after [`BASE_UNIX`].
    pub async fn seed_message(&self, chat: Uuid, n: u128, role: &str, created: i64) -> Uuid {
        self.seed_message_row(
            chat,
            n,
            message_row(msg_id(n), USER_TENANT, chat)
                .set("role", role)
                .set("request_id", request_id(n))
                .set("created_at", ts(created)),
        )
        .await
    }

    /// Inserts a prepared message row (see [`message_row`]); returns its id.
    pub async fn seed_message_row(&self, _chat: Uuid, n: u128, row: Row) -> Uuid {
        self.db.insert("messages", row).await.expect("seed message");
        msg_id(n)
    }

    /// Soft-deletes a message.
    pub async fn soft_delete_message(&self, id: Uuid) {
        self.db.soft_delete("messages", id).await;
    }

    /// Inserts an attachment of the default tenant; returns its id.
    pub async fn seed_attachment(
        &self,
        chat: Uuid,
        n: u128,
        kind: &str,
        status: &str,
        cleanup_status: Option<&str>,
    ) -> Uuid {
        let id = att_id(n);
        let mut row = attachment_row(id, USER_TENANT, chat)
            .set("filename", format!("file-{n}.bin"))
            .set("attachment_kind", kind)
            .set("status", status)
            .set("created_at", ts(0))
            .set("updated_at", ts(0));
        row = match cleanup_status {
            Some(value) => row.set("cleanup_status", value),
            None => row.null("cleanup_status"),
        };
        self.db
            .insert("attachments", row)
            .await
            .expect("seed attachment");
        id
    }

    /// Links an attachment to a message; `created` is seconds after
    /// [`BASE_UNIX`].
    pub async fn link_attachment(&self, chat: Uuid, message: Uuid, attachment: Uuid, created: i64) {
        self.db
            .insert(
                "message_attachments",
                Row::default()
                    .set("tenant_id", USER_TENANT)
                    .set("chat_id", chat)
                    .set("message_id", message)
                    .set("attachment_id", attachment)
                    .set("created_at", ts(created)),
            )
            .await
            .expect("link attachment");
    }

    /// Inserts a reaction row of `user` on `message`.
    pub async fn seed_reaction(&self, n: u128, message: Uuid, user: Uuid, reaction: &str) -> Uuid {
        let id = Uuid::from_u128(0x7000 + n);
        self.db
            .insert(
                "message_reactions",
                Row::default()
                    .set("id", id)
                    .set("message_id", message)
                    .set("user_id", user)
                    .set("tenant_id", USER_TENANT)
                    .set("reaction", reaction)
                    .set("created_at", ts(5)),
            )
            .await
            .expect("seed reaction");
        id
    }

    // ---- raw readers -----------------------------------------------------------------------

    /// First column of the first row of `sql` as text (`NULL` is `"NULL"`).
    pub async fn text(&self, sql: &str) -> String {
        self.db.query_string(sql).await
    }

    /// The stored `deleted_at` text of chat `id`, `None` when NULL.
    pub async fn chat_deleted_at(&self, id: Uuid) -> Option<String> {
        let text = self
            .text(&format!(
                "SELECT quote(deleted_at) FROM chats WHERE id = {}",
                blob(id)
            ))
            .await;
        (text != "NULL").then(|| text.trim_matches('\'').to_owned())
    }

    /// The stored `updated_at` text of chat `id`.
    pub async fn chat_updated_at(&self, id: Uuid) -> String {
        self.text(&format!(
            "SELECT updated_at FROM chats WHERE id = {}",
            blob(id)
        ))
        .await
    }

    /// Every column of chat `id` as one string (changes whenever the row does).
    pub async fn chat_snapshot(&self, id: Uuid) -> String {
        self.text(&format!(
            "SELECT quote(id)||quote(tenant_id)||quote(user_id)||quote(model)||quote(title)\
             ||quote(is_temporary)||quote(created_at)||quote(updated_at)||quote(deleted_at) \
             FROM chats WHERE id = {}",
            blob(id)
        ))
        .await
    }

    /// Every row and column of `table`, ordered, as one string.
    pub async fn dump(&self, table: &str) -> String {
        let names = self
            .db
            .raw
            .query_all_raw(Statement::from_string(
                DbBackend::Sqlite,
                format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"),
            ))
            .await
            .expect("table info");
        let columns: Vec<String> = names
            .iter()
            .map(|row| format!("quote({})", row.try_get::<String>("", "name").unwrap()))
            .collect();
        let expr = columns.join("||'|'||");
        self.text(&format!(
            "SELECT COALESCE(group_concat(r, char(10)), '') \
             FROM (SELECT {expr} AS r FROM {table} ORDER BY r)"
        ))
        .await
    }

    /// Rows of the chat-cleanup queue of the outbox (default queue name),
    /// whether or not the sequencer has moved them already.
    pub async fn chat_cleanup_rows(&self) -> Vec<OutboxRow> {
        self.outbox_rows(CHAT_CLEANUP_QUEUE).await
    }

    /// Rows of `queue` of the outbox.
    pub async fn outbox_rows(&self, queue: &str) -> Vec<OutboxRow> {
        let sql = format!(
            "SELECT DISTINCT b.id AS body_id, p.queue AS queue, p.partition AS part, b.payload AS payload \
             FROM mini_chat__outbox_body b \
             JOIN (SELECT partition_id, body_id FROM mini_chat__outbox_incoming \
                   UNION ALL SELECT partition_id, body_id FROM mini_chat__outbox_outgoing) m \
               ON m.body_id = b.id \
             JOIN mini_chat__outbox_partitions p ON p.id = m.partition_id \
             WHERE p.queue = '{queue}' ORDER BY b.id"
        );
        let mut last_error = None;
        for _ in 0..20 {
            match self
                .db
                .raw
                .query_all_raw(Statement::from_string(DbBackend::Sqlite, sql.clone()))
                .await
            {
                Ok(rows) => {
                    return rows
                        .iter()
                        .map(|row| OutboxRow {
                            queue: row.try_get("", "queue").unwrap(),
                            partition: row.try_get("", "part").unwrap(),
                            payload: row.try_get("", "payload").unwrap(),
                        })
                        .collect();
                }
                Err(error) => {
                    last_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        }
        panic!("cannot read the outbox: {last_error:?}");
    }

    /// Number of rows of every outbox queue together.
    pub async fn outbox_message_count(&self) -> i64 {
        self.db.count("mini_chat__outbox_body").await
    }

    /// The only PDP request seen.
    pub fn only_pdp_request(&self) -> EvaluationRequest {
        let requests = self.pdp.requests();
        assert_eq!(requests.len(), 1, "exactly one PDP call: {requests:?}");
        requests.into_iter().next().unwrap()
    }
}

// ---- ids and time -------------------------------------------------------------------------------

/// Chat id `n`.
pub fn chat_id(n: u128) -> Uuid {
    Uuid::from_u128(0xC000_0000 + n)
}

/// Message id `n`.
pub fn msg_id(n: u128) -> Uuid {
    Uuid::from_u128(0xD000_0000 + n)
}

/// Request id of message `n`.
pub fn request_id(n: u128) -> Uuid {
    Uuid::from_u128(0xE000_0000 + n)
}

/// Attachment id `n`.
pub fn att_id(n: u128) -> Uuid {
    Uuid::from_u128(0xF000_0000 + n)
}

/// `X'..'` literal of a UUID as the driver stores it (16-byte blob).
pub fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

/// 2024-01-01T00:00:00Z as a unix timestamp: every seeded time is after it by
/// a few seconds and well before the test clock.
pub const BASE_UNIX: i64 = 1_704_067_200;

/// The text the driver stores for `BASE_UNIX + secs`: RFC 3339 with `Z` and no
/// fractional digits for a whole second, exactly what the application writes
/// and binds for such a value, so that seeded timestamps compare and sort
/// consistently with bound cursor and filter values as `SQLite` text.
pub fn ts(secs: i64) -> String {
    let at = time::OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second()
    )
}

/// RFC 3339 form of `BASE_UNIX + secs` for use in `$filter` literals.
pub fn rfc3339(secs: i64) -> String {
    let at = time::OffsetDateTime::from_unix_timestamp(BASE_UNIX + secs).unwrap();
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// Parses a stored or serialized timestamp (RFC 3339, or the driver format
/// `YYYY-MM-DD HH:MM:SS[.f]+00:00`).
pub fn parse_ts(text: &str) -> chrono::DateTime<chrono::Utc> {
    let text = text.trim_matches('\'');
    chrono::DateTime::parse_from_rfc3339(text)
        .or_else(|_| chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%:z"))
        .or_else(|_| chrono::DateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .unwrap_or_else(|error| panic!("unparsable timestamp {text:?}: {error}"))
        .with_timezone(&chrono::Utc)
}

/// `{url_prefix}/v1/chats/{id}` under the default prefix.
pub fn chat_uri(id: Uuid) -> String {
    format!("/mini-chat/v1/chats/{id}")
}

/// `.../messages/{msg}/reaction` under the default prefix.
pub fn reaction_uri(chat: Uuid, message: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/messages/{message}/reaction")
}

/// Ids of `items[*].id` of a page reply.
pub fn item_ids(reply: &Reply) -> Vec<String> {
    reply.json["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items in: {}", reply.text))
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_owned())
        .collect()
}

/// `next_cursor` of a page reply, if any.
pub fn next_cursor(reply: &Reply) -> Option<String> {
    reply.json["page_info"]["next_cursor"]
        .as_str()
        .map(str::to_owned)
}

/// Percent-encodes a query value.
pub fn enc(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            write!(out, "%{byte:02X}").unwrap();
        }
    }
    out
}
