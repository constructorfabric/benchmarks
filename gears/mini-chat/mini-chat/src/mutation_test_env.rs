#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(
    clippy::format_push_string,
    clippy::needless_pass_by_value,
    clippy::struct_excessive_bools
)]
//! Fixture of the turn-mutation persistence and end-to-end tests
//! (`cpt-cf-mini-chat-feature-turn-mutations`): the streaming fixture (real
//! stores over a running outbox, the real OAGW provider client over the fake
//! gateway, the stream service and its router) extended with the real
//! mutation store, the mutation service, the three mutation routes, seeding of
//! whole turns (turn row, user and assistant message, attachment links) and
//! readers of the rows a mutation changes.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::future::BoxFuture;
use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent};
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

use crate::api::rest::routes::register_turn_mutation_routes;
use crate::chat_test_env::{Reply, blob, ts};
use crate::config::OutboxConfig;
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::mutations::ports::{
    DeletePlan, MutationMetrics, MutationRepository, ReplacementPlan,
};
use crate::domain::service::mutations::service::{MutationDeps, MutationService};
use crate::domain::service::mutations::test_support::RecordingMutationMetrics;
use crate::domain::service::mutations::types::{LatestTurn, MutationOp, TargetMessage, TargetTurn};
use crate::domain::service::stream::ports::{MessageBoundary, TurnRepository};
use crate::infra::db::repos::chats::SeaChatRepository;
use crate::infra::db::repos::mutations::SeaMutationRepository;
use crate::infra::db::test_support::{Row, turn_row};
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::stream_test_env::{AUDIT_QUEUE, OpenStream, Options, PREFIX, StreamEnv};
use crate::test_doubles::{USER, USER_TENANT, user_ctx};

/// The `context.resource_type` of a missing turn.
pub const TURN_TYPE: &str = "gts.cf.core.mini_chat.turn.v1~";

/// The scope of the owner of the default chat (what the PEP compiles to).
pub fn owner_scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT).ensure_owner(USER)
}

/// A turn to seed: the `chat_turns` row with its user message, its assistant
/// message (when `answer` is set) and its attachment links.
#[derive(Clone)]
pub struct SeedTurn {
    /// Number of the turn: ids, request id and message ids derive from it.
    pub n: u128,
    /// `completed`, `failed`, `cancelled` or `running`.
    pub state: &'static str,
    /// Seconds after the fixture epoch: `started_at` and the user message time.
    pub secs: i64,
    /// Content of the user message.
    pub question: String,
    /// Content of the assistant message; `None` seeds no assistant message.
    pub answer: Option<String>,
    /// `web_search_enabled`.
    pub web_search: bool,
    /// `requester_user_id`.
    pub requester: Option<Uuid>,
    /// Seeded with the six preflight columns.
    pub reserved: bool,
    /// Attachments linked to the user message.
    pub attachments: Vec<Uuid>,
    /// Seeded soft-deleted.
    pub deleted: bool,
    /// `chat_turns.error_code`.
    pub error_code: Option<&'static str>,
    /// Seeded user message `is_compressed`.
    pub compressed: bool,
}

impl SeedTurn {
    /// A completed turn with a question and an answer.
    pub fn completed(n: u128, secs: i64) -> Self {
        Self {
            n,
            state: "completed",
            secs,
            question: format!("question {n}"),
            answer: Some(format!("answer {n}")),
            web_search: false,
            requester: Some(USER),
            reserved: true,
            attachments: Vec::new(),
            deleted: false,
            error_code: None,
            compressed: false,
        }
    }

    /// A failed turn: one message (the question), no assistant message.
    pub fn failed(n: u128, secs: i64) -> Self {
        Self {
            state: "failed",
            answer: None,
            error_code: Some("provider_error"),
            ..Self::completed(n, secs)
        }
    }

    /// A cancelled turn: one message.
    pub fn cancelled(n: u128, secs: i64) -> Self {
        Self {
            state: "cancelled",
            answer: None,
            ..Self::completed(n, secs)
        }
    }

    /// A running turn (no assistant message, no `completed_at`).
    pub fn running(n: u128, secs: i64) -> Self {
        Self {
            state: "running",
            answer: None,
            ..Self::completed(n, secs)
        }
    }

    pub fn question(mut self, text: &str) -> Self {
        text.clone_into(&mut self.question);
        self
    }

    pub fn web_search(mut self, enabled: bool) -> Self {
        self.web_search = enabled;
        self
    }

    pub fn requester(mut self, user: Option<Uuid>) -> Self {
        self.requester = user;
        self
    }

    pub fn attached(mut self, attachments: &[Uuid]) -> Self {
        self.attachments = attachments.to_vec();
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted = true;
        self
    }

    pub fn compressed(mut self) -> Self {
        self.compressed = true;
        self
    }

    pub fn turn_id(&self) -> Uuid {
        Uuid::from_u128(0xA100_0000 + self.n)
    }

    pub fn request_id(&self) -> Uuid {
        Uuid::from_u128(0xE200_0000 + self.n)
    }

    pub fn user_message_id(&self) -> Uuid {
        Uuid::from_u128(0xD200_0000 + self.n * 2)
    }

    pub fn assistant_message_id(&self) -> Uuid {
        Uuid::from_u128(0xD200_0000 + self.n * 2 + 1)
    }
}

/// What [`MutationEnv::seed_turn`] returns.
#[derive(Debug, Clone, Copy)]
pub struct Seeded {
    pub turn_id: Uuid,
    pub request_id: Uuid,
    pub user_message: Uuid,
    pub assistant_message: Uuid,
}

/// One live or soft-deleted message row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub role: String,
    pub content: String,
    pub deleted: bool,
    pub compressed: bool,
}

/// A hook awaited before the mutation transaction opens (after the preview and
/// the other pre-commit reads).
pub type BeforeTx = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// A mutation store that delegates to another and can run a hook right before
/// a transaction opens: the window in which a concurrent request wins.
pub struct HookedMutations {
    inner: Arc<SeaMutationRepository>,
    pub before_replace: Mutex<Option<BeforeTx>>,
    pub before_delete: Mutex<Option<BeforeTx>>,
    /// Awaited after the replacement transaction committed.
    pub after_replace: Mutex<Option<BeforeTx>>,
}

#[async_trait]
impl MutationRepository for HookedMutations {
    async fn find_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetTurn>, DomainError> {
        self.inner.find_target(scope, chat_id, request_id).await
    }

    async fn latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<LatestTurn>, DomainError> {
        self.inner.latest_turn(scope, chat_id).await
    }

    async fn user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetMessage>, DomainError> {
        self.inner.user_message(scope, chat_id, request_id).await
    }

    async fn carried_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        self.inner
            .carried_attachment_ids(scope, chat_id, user_message_id)
            .await
    }

    async fn history_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        new_request_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        self.inner
            .history_boundary(scope, chat_id, new_request_id)
            .await
    }

    async fn replace_turn(&self, plan: &ReplacementPlan) -> Result<(), DomainError> {
        let hook = self.before_replace.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook().await;
        }
        let committed = self.inner.replace_turn(plan).await;
        if committed.is_ok() {
            let hook = self.after_replace.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook().await;
            }
        }
        committed
    }

    async fn delete_turn(&self, plan: &DeletePlan) -> Result<(), DomainError> {
        let hook = self.before_delete.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook().await;
        }
        self.inner.delete_turn(plan).await
    }

    async fn mark_setup_failed(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        self.inner
            .mark_setup_failed(tenant_id, turn_id, error_code, at)
            .await
    }
}

/// The fixture.
pub struct MutationEnv {
    pub stream: StreamEnv,
    pub repo: Arc<SeaMutationRepository>,
    pub hooked: Arc<HookedMutations>,
    pub service: Arc<MutationService>,
    pub mutation_metrics: Arc<RecordingMutationMetrics>,
    pub router: Router,
}

impl Deref for MutationEnv {
    type Target = StreamEnv;

    fn deref(&self) -> &StreamEnv {
        &self.stream
    }
}

impl MutationEnv {
    pub async fn new() -> Self {
        Self::with(Options::default()).await
    }

    /// File-backed WAL database with a pool (real concurrency).
    pub async fn file() -> Self {
        Self::with(Options {
            file_db: true,
            ..Options::default()
        })
        .await
    }

    pub async fn with(options: Options) -> Self {
        Self::build(options, false).await
    }

    /// The audit enqueue of the mutation transactions fails after the other
    /// statements ran (its queue is not registered).
    pub async fn with_failing_mutation_audit() -> Self {
        Self::build(Options::default(), true).await
    }

    async fn build(options: Options, audit_fails: bool) -> Self {
        let stream = StreamEnv::with(options).await;
        let enqueuer = if audit_fails {
            let mismatched = Arc::new(OutboxEnqueuer::new(&OutboxConfig {
                audit_queue_name: "mini-chat.not_registered".to_owned(),
                num_partitions: 4,
                ..OutboxConfig::default()
            }));
            assert!(mismatched.bind(&stream.quota.enqueuer.outbox().expect("the outbox runs")));
            mismatched
        } else {
            Arc::clone(&stream.quota.enqueuer)
        };
        let repo = Arc::new(SeaMutationRepository::new(
            Arc::clone(&stream.quota.provider),
            enqueuer,
        ));
        let hooked = Arc::new(HookedMutations {
            inner: Arc::clone(&repo),
            before_replace: Mutex::new(None),
            before_delete: Mutex::new(None),
            after_replace: Mutex::new(None),
        });
        let authz = Arc::new(AuthzService::new(
            Arc::clone(&stream.quota.pdp) as Arc<dyn AuthZResolverApi>
        ));
        let mutation_metrics = RecordingMutationMetrics::new();
        let service = Arc::new(MutationService::new(MutationDeps {
            authz,
            chats: Arc::new(SeaChatRepository::new(
                Arc::clone(&stream.quota.provider),
                Arc::clone(&stream.quota.enqueuer),
            )) as Arc<dyn ChatRepository>,
            policy: Arc::clone(&stream.quota.policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&stream.quota.service),
            turns: Arc::clone(&stream.turns) as Arc<dyn TurnRepository>,
            mutations: Arc::clone(&hooked) as Arc<dyn MutationRepository>,
            stream: Arc::clone(&stream.service),
            metrics: Arc::clone(&mutation_metrics) as Arc<dyn MutationMetrics>,
            clock: stream.clock_fn(),
        }));
        let router = register_turn_mutation_routes(
            stream.router.clone(),
            &stream.openapi,
            Arc::clone(&service),
            PREFIX,
        );
        Self {
            stream,
            repo,
            hooked,
            service,
            mutation_metrics,
            router,
        }
    }

    // ---- requests -------------------------------------------------------------------------

    pub fn retry_uri(chat: Uuid, request: Uuid) -> String {
        format!("{PREFIX}/v1/chats/{chat}/turns/{request}/retry")
    }

    pub fn item_uri(chat: Uuid, request: Uuid) -> String {
        StreamEnv::turn_uri(chat, request)
    }

    fn build_request(
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<(&str, String)>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        let payload = match body {
            Some((content_type, text)) => {
                builder = builder.header("content-type", content_type);
                Body::from(text)
            }
            None => Body::empty(),
        };
        let mut request = builder.body(payload).unwrap();
        request.extensions_mut().insert(ctx.clone());
        request
    }

    /// Sends a request and reads the whole response (a stream must end).
    pub async fn call(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<(&str, String)>,
    ) -> Reply {
        let request = Self::build_request(method, uri, ctx, body);
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = tokio::time::timeout(
            Duration::from_secs(20),
            to_bytes(response.into_body(), 64 << 20),
        )
        .await
        .expect("the response did not end")
        .unwrap();
        Reply {
            status,
            headers,
            text: String::from_utf8_lossy(&bytes).into_owned(),
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// Sends a request and returns the open response, to be read frame by frame.
    pub async fn open(
        &self,
        method: &str,
        uri: &str,
        ctx: &SecurityContext,
        body: Option<(&str, String)>,
    ) -> OpenStream {
        let request = Self::build_request(method, uri, ctx, body);
        let response = tokio::time::timeout(
            Duration::from_secs(10),
            self.router.clone().oneshot(request),
        )
        .await
        .expect("the response head did not arrive")
        .unwrap();
        OpenStream::from_response(response)
    }

    fn edit_body(content: &str) -> (&'static str, String) {
        (
            "application/json",
            json!({ "content": content }).to_string(),
        )
    }

    pub async fn retry(&self, request: Uuid) -> Reply {
        self.call(
            "POST",
            &Self::retry_uri(self.chat, request),
            &user_ctx(),
            None,
        )
        .await
    }

    pub async fn edit(&self, request: Uuid, content: &str) -> Reply {
        self.call(
            "PATCH",
            &Self::item_uri(self.chat, request),
            &user_ctx(),
            Some(Self::edit_body(content)),
        )
        .await
    }

    pub async fn delete(&self, request: Uuid) -> Reply {
        self.call(
            "DELETE",
            &Self::item_uri(self.chat, request),
            &user_ctx(),
            None,
        )
        .await
    }

    pub async fn open_retry(&self, request: Uuid) -> OpenStream {
        self.open(
            "POST",
            &Self::retry_uri(self.chat, request),
            &user_ctx(),
            None,
        )
        .await
    }

    pub async fn open_edit(&self, request: Uuid, content: &str) -> OpenStream {
        self.open(
            "PATCH",
            &Self::item_uri(self.chat, request),
            &user_ctx(),
            Some(Self::edit_body(content)),
        )
        .await
    }

    // ---- seeding ---------------------------------------------------------------------------

    /// Seeds a whole turn of the default chat.
    pub async fn seed_turn(&self, seed: &SeedTurn) -> Seeded {
        self.seed_turn_in(self.chat, seed).await
    }

    pub async fn seed_turn_in(&self, chat: Uuid, seed: &SeedTurn) -> Seeded {
        let db = &self.quota.db;
        let mut row = turn_row(
            seed.turn_id(),
            USER_TENANT,
            chat,
            seed.request_id(),
            seed.state,
        )
        .set("started_at", ts(seed.secs))
        .set("updated_at", ts(seed.secs + 1))
        .set("web_search_enabled", seed.web_search);
        row = match seed.requester {
            Some(user) => row.set("requester_user_id", user),
            None => row.null("requester_user_id"),
        };
        if seed.state != "running" {
            row = row.set("completed_at", ts(seed.secs + 1));
        }
        if seed.reserved {
            row = row
                .set("reserve_tokens", 1500_i64)
                .set("max_output_tokens_applied", 500_i32)
                .set("reserved_credits_micro", 1_500_000_i64)
                .set("policy_version_applied", 7_i64)
                .set("effective_model", "p")
                .set("minimal_generation_floor_applied", 50_i32);
        }
        if seed.answer.is_some() {
            row = row.set("assistant_message_id", seed.assistant_message_id());
        }
        if let Some(code) = seed.error_code {
            row = row.set("error_code", code);
        }
        if seed.deleted {
            row = row.set("deleted_at", ts(seed.secs + 2));
        }
        db.insert("chat_turns", row).await.unwrap();
        db.insert(
            "messages",
            crate::infra::db::test_support::message_row(seed.user_message_id(), USER_TENANT, chat)
                .set("role", "user")
                .set("content", seed.question.clone())
                .set("request_id", seed.request_id())
                .set("created_at", ts(seed.secs))
                .set("is_compressed", seed.compressed)
                .set_deleted(seed.deleted, ts(seed.secs + 2)),
        )
        .await
        .unwrap();
        if let Some(answer) = &seed.answer {
            db.insert(
                "messages",
                crate::infra::db::test_support::message_row(
                    seed.assistant_message_id(),
                    USER_TENANT,
                    chat,
                )
                .set("role", "assistant")
                .set("content", answer.clone())
                .set("request_id", seed.request_id())
                .set("created_at", ts(seed.secs + 1))
                .set("model", "p")
                .set("is_compressed", seed.compressed)
                .set_deleted(seed.deleted, ts(seed.secs + 2)),
            )
            .await
            .unwrap();
        }
        for attachment in &seed.attachments {
            db.insert(
                "message_attachments",
                Row::default()
                    .set("tenant_id", USER_TENANT)
                    .set("chat_id", chat)
                    .set("message_id", seed.user_message_id())
                    .set("attachment_id", *attachment)
                    .set("created_at", ts(seed.secs)),
            )
            .await
            .unwrap();
        }
        Seeded {
            turn_id: seed.turn_id(),
            request_id: seed.request_id(),
            user_message: seed.user_message_id(),
            assistant_message: seed.assistant_message_id(),
        }
    }

    /// A user or assistant message of `request` without a turn row (the new
    /// user message of a replacement, for the history boundary tests).
    pub async fn seed_message_for(&self, request: Uuid, role: &str, content: &str, secs: i64) {
        self.quota
            .db
            .insert(
                "messages",
                crate::infra::db::test_support::message_row(Uuid::new_v4(), USER_TENANT, self.chat)
                    .set("role", role)
                    .set("content", content)
                    .set("request_id", request)
                    .set("created_at", ts(secs)),
            )
            .await
            .unwrap();
    }

    /// A summary whose frontier is `up_to` (a message of `secs`).
    pub async fn seed_summary_at(&self, up_to: Uuid, up_to_secs: i64) {
        self.seed_summary(self.chat, "the summary text", up_to, up_to_secs, 42)
            .await;
    }

    /// Soft-deletes an attachment (as the attachment delete does).
    pub async fn soft_delete_attachment(&self, attachment: Uuid) {
        self.quota.db.soft_delete("attachments", attachment).await;
    }

    // ---- plans for repository-level tests --------------------------------------------------

    /// The replacement plan of `old` with fresh ids and the clock time.
    pub fn replacement_plan(
        &self,
        old: &SeedTurn,
        op: MutationOp,
        content: &str,
    ) -> ReplacementPlan {
        let new_request_id = Uuid::new_v4();
        let now = self.now();
        ReplacementPlan {
            op,
            tenant_id: USER_TENANT,
            chat_id: self.chat,
            user_id: USER,
            target_turn_id: old.turn_id(),
            target_request_id: old.request_id(),
            target_started_at: OffsetDateTime::from_unix_timestamp(
                crate::chat_test_env::BASE_UNIX + old.secs,
            )
            .unwrap(),
            target_user_message: MessageBoundary {
                created_at: OffsetDateTime::from_unix_timestamp(
                    crate::chat_test_env::BASE_UNIX + old.secs,
                )
                .unwrap(),
                id: old.user_message_id(),
            },
            new_turn_id: Uuid::new_v4(),
            new_request_id,
            new_user_message_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
            content: content.to_owned(),
            web_search_enabled: old.web_search,
            now,
            audit: match op {
                MutationOp::Edit => TurnMutationAuditEvent::TurnEdit {
                    tenant_id: USER_TENANT,
                    actor_user_id: USER,
                    chat_id: self.chat,
                    original_request_id: old.request_id(),
                    new_request_id,
                    timestamp: now,
                },
                _ => TurnMutationAuditEvent::TurnRetry {
                    tenant_id: USER_TENANT,
                    actor_user_id: USER,
                    chat_id: self.chat,
                    original_request_id: old.request_id(),
                    new_request_id,
                    timestamp: now,
                },
            },
        }
    }

    /// The delete plan of `old` with the clock time.
    pub fn delete_plan(&self, old: &SeedTurn) -> DeletePlan {
        let now = self.now();
        DeletePlan {
            tenant_id: USER_TENANT,
            chat_id: self.chat,
            user_id: USER,
            target_turn_id: old.turn_id(),
            target_request_id: old.request_id(),
            target_started_at: OffsetDateTime::from_unix_timestamp(
                crate::chat_test_env::BASE_UNIX + old.secs,
            )
            .unwrap(),
            target_user_message: MessageBoundary {
                created_at: OffsetDateTime::from_unix_timestamp(
                    crate::chat_test_env::BASE_UNIX + old.secs,
                )
                .unwrap(),
                id: old.user_message_id(),
            },
            now,
            audit: TurnMutationAuditEvent::TurnDelete {
                tenant_id: USER_TENANT,
                actor_user_id: USER,
                chat_id: self.chat,
                request_id: old.request_id(),
                timestamp: now,
            },
        }
    }

    // ---- readers ---------------------------------------------------------------------------

    /// Runs a raw statement (a test arranging the world between two calls).
    pub async fn exec(&self, sql: &str) {
        use sea_orm::ConnectionTrait;
        self.quota
            .db
            .raw
            .execute_raw(sea_orm::Statement::from_string(
                sea_orm::DbBackend::Sqlite,
                sql.to_owned(),
            ))
            .await
            .expect("the statement runs");
    }

    /// `quote(column)` of a message by id.
    pub async fn message_col(&self, message: Uuid, column: &str) -> String {
        self.sql(&format!(
            "SELECT quote({column}) FROM messages WHERE id = {}",
            blob(message)
        ))
        .await
    }

    /// `deleted_at` of a turn (by request id); `None` when live.
    pub async fn turn_deleted_at(&self, request: Uuid) -> Option<String> {
        self.turn_text(request, "deleted_at").await
    }

    /// `replaced_by_request_id` of a turn as a UUID.
    pub async fn replaced_by_request(&self, request: Uuid) -> Option<Uuid> {
        let hex = self
            .sql(&format!(
                "SELECT COALESCE(hex(replaced_by_request_id), '') FROM chat_turns WHERE request_id = {}",
                blob(request)
            ))
            .await;
        (!hex.is_empty()).then(|| Uuid::parse_str(&hex).unwrap())
    }

    /// Every message of `request` (live and soft-deleted), role order.
    pub async fn messages_of(&self, request: Uuid) -> Vec<MessageRow> {
        let joined = self
            .sql(&format!(
                "SELECT COALESCE(group_concat(role || char(31) || content || char(31) || \
                 (deleted_at IS NOT NULL) || char(31) || is_compressed, char(30)), '') \
                 FROM (SELECT role, content, deleted_at, is_compressed FROM messages \
                       WHERE request_id = {} ORDER BY role DESC)",
                blob(request)
            ))
            .await;
        if joined.is_empty() {
            return Vec::new();
        }
        joined
            .split('\u{1e}')
            .map(|row| {
                let mut parts = row.split('\u{1f}');
                MessageRow {
                    role: parts.next().unwrap().to_owned(),
                    content: parts.next().unwrap().to_owned(),
                    deleted: parts.next().unwrap() == "1",
                    compressed: parts.next().unwrap() == "1",
                }
            })
            .collect()
    }

    /// The id of the live user message of `request`.
    pub async fn user_message_of(&self, request: Uuid) -> Uuid {
        let hex = self
            .sql(&format!(
                "SELECT hex(id) FROM messages WHERE request_id = {} AND role = 'user' AND deleted_at IS NULL",
                blob(request)
            ))
            .await;
        Uuid::parse_str(&hex).unwrap()
    }

    /// The attachment ids linked to `message`, ascending.
    pub async fn links_of(&self, message: Uuid) -> Vec<Uuid> {
        let joined = self
            .sql(&format!(
                "SELECT COALESCE(group_concat(h, ','), '') FROM (SELECT hex(attachment_id) AS h \
                 FROM message_attachments WHERE message_id = {} ORDER BY attachment_id)",
                blob(message)
            ))
            .await;
        if joined.is_empty() {
            return Vec::new();
        }
        joined
            .split(',')
            .map(|h| Uuid::parse_str(h).unwrap())
            .collect()
    }

    pub async fn summary_count(&self) -> i64 {
        self.count("thread_summaries").await
    }

    /// Messages of the default chat with `is_compressed` set (deleted included).
    pub async fn compressed_count(&self) -> i64 {
        self.quota
            .db
            .query_i64(&format!(
                "SELECT COUNT(*) FROM messages WHERE chat_id = {} AND is_compressed = 1",
                blob(self.chat)
            ))
            .await
    }

    pub async fn chat_updated_at(&self) -> String {
        self.sql(&format!(
            "SELECT updated_at FROM chats WHERE id = {}",
            blob(self.chat)
        ))
        .await
    }

    /// Request ids of the live turns of the default chat, oldest first.
    pub async fn live_turn_requests(&self) -> Vec<Uuid> {
        let joined = self
            .sql(&format!(
                "SELECT COALESCE(group_concat(h, ','), '') FROM (SELECT hex(request_id) AS h \
                 FROM chat_turns WHERE chat_id = {} AND deleted_at IS NULL \
                 ORDER BY started_at, id)",
                blob(self.chat)
            ))
            .await;
        if joined.is_empty() {
            return Vec::new();
        }
        joined
            .split(',')
            .map(|h| Uuid::parse_str(h).unwrap())
            .collect()
    }

    /// The mutation audit events in the audit queue, in enqueue order.
    pub async fn mutation_events(&self) -> Vec<TurnMutationAuditEvent> {
        self.quota
            .outbox_rows(AUDIT_QUEUE)
            .await
            .iter()
            .filter_map(|row| {
                match mini_chat_sdk::decode_audit_event(&row.payload).expect("an audit event") {
                    MiniChatAuditEvent::Mutation(event) => Some(event),
                    MiniChatAuditEvent::Turn(_) => None,
                }
            })
            .collect()
    }

    /// The turn (finalization) audit events in the audit queue, in enqueue order.
    pub async fn turn_audit_events(&self) -> Vec<mini_chat_sdk::TurnAuditEvent> {
        self.quota
            .outbox_rows(AUDIT_QUEUE)
            .await
            .iter()
            .filter_map(|row| {
                match mini_chat_sdk::decode_audit_event(&row.payload).expect("an audit event") {
                    MiniChatAuditEvent::Turn(event) => Some(event),
                    MiniChatAuditEvent::Mutation(_) => None,
                }
            })
            .collect()
    }

    /// The raw payloads of the audit queue.
    pub async fn audit_payloads(&self) -> Vec<Value> {
        self.quota
            .outbox_rows(AUDIT_QUEUE)
            .await
            .iter()
            .map(crate::chat_test_env::OutboxRow::json)
            .collect()
    }

    /// Every table a mutation writes (and the outbox), as one string.
    pub async fn world(&self) -> String {
        self.snapshot().await
    }

    /// The `request_id` of the `stream_started` frame of a streamed reply.
    pub fn started_request_id(frames: &[crate::stream_test_env::Frame]) -> Uuid {
        let started = frames
            .iter()
            .find(|frame| frame.name == "stream_started")
            .expect("a stream_started frame");
        Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap()
    }
}

/// Soft-delete columns of a seeded message row.
trait SetDeleted {
    fn set_deleted(self, deleted: bool, at: String) -> Self;
}

impl SetDeleted for Row {
    fn set_deleted(self, deleted: bool, at: String) -> Self {
        if deleted {
            self.set("deleted_at", at)
        } else {
            self
        }
    }
}

/// `StatusCode` shorthand of the tests.
pub const OK: StatusCode = StatusCode::OK;
