//! S7 (thread summary) harness on top of the S1 fakes, the S2 chat harness, the S3 stream pieces,
//! the S4 quota pieces and the S5 seeds.
//!
//! Kept apart from `common`, `chat_support`, `stream_support`, `quota_support`, `mutation_support`
//! and `attachment_support` so the S1 - S6 test binaries (which never declare `mod summary_support;`)
//! compile independently of the S7 production items. Everything here relies only on the items
//! listed in `S7-test-interface.md`. Test files declare `mod common; mod chat_support;
//! mod stream_support; mod quota_support; mod mutation_support; mod summary_support;`.
//!
//! * [`SummaryOagw`] is the `ServiceGatewayClientV1` the gear talks to: it hands every provider
//!   request whose `model` is the provider-side name of the summary model to a second scripted
//!   [`FakeOagw`] (`h.summaries`) and every other request to the chat provider (`h.oagw`), so the
//!   two kinds of provider traffic are scripted and recorded apart and never race for a FIFO reply.
//! * [`SummaryHarness`] wires the real `StreamService` (usage, audit and thread-summary ports),
//!   `TurnsService`, `TurnMutationService` and a [`ThreadSummaryHandler`] over the S2 database, with
//!   one running outbox serving the usage queue, the audit queue and the thread-summary queue (the
//!   last one runs the real handler, a recording stand-in, or is not registered at all).
//! * [`TaskSpec`] builds the JSON payload of a thread-summary message (DESIGN 3.2 "Durable
//!   scheduling"); [`SummaryHarness::deliver`] hands it to the real handler like the outbox does.

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
    clippy::needless_pass_by_value,
    clippy::integer_division,
    clippy::struct_excessive_bools,
    clippy::cast_possible_truncation
)]

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body as AxumBody;
use axum::http::{Request, header};
use mini_chat::api::rest::{register_stream_routes, register_turn_mutation_routes};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::streaming::StreamService;
use mini_chat::domain::turn_mutations::TurnMutationService;
use mini_chat::domain::turns::TurnsService;
use mini_chat::infra::db::entity::{chat_turns, chats, messages, quota_usage, thread_summaries};
use mini_chat::infra::db::timestamp::{to_db, to_db_opt};
use mini_chat::infra::llm::LlmProviders;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat::infra::outbox::{
    AuditOutbox, AuditOutboxHandler, ThreadSummaryHandler, ThreadSummaryOutbox, UsageOutbox,
    UsageOutboxHandler, thread_summary_lease,
};
use mini_chat_sdk::{
    BillingOutcome, MiniChatAuditPluginClientV1, MiniChatModelPolicyPluginClientV1,
    ModelPolicyPluginError, PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent,
    UserLimits,
};
use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::body::Body;
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{
    DeadLetterFilter, DeadLetterScope, LeasedMessageHandler, MessageResult, Outbox, OutboxHandle,
    OutboxMessage, OutboxProfile, Partitions,
};
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::chat_support::{
    ChatHarness, ChatHarnessOptions, ChatSeed, MessageSeed, assert_not_found, from_db, page_items,
    ts, ts_micros, uid,
};
pub use crate::common::{
    FakePdp, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem, ctx, ctx_a,
};
pub use crate::mutation_support::{
    ALL_OPS, EDIT_TEXT, Op, Pair, PairSeed, TurnRowSeed, assert_not_latest,
};
pub use crate::quota_support::{
    LimitsSpec, PREMIUM, QuotaPolicy, RecordingAudit, STANDARD, audit_gateway_for, quota_models,
    ref_estimate,
};
pub use crate::stream_support::{
    FakeOagw, LiveSse, Opts, Piece, ProviderReply, RawStream, RecordedProviderRequest, Sent, ev,
    eventually, hex, merge, model_entries, provider_model, stream_config, stream_model_json,
};
pub use mini_chat::infra::outbox::THREAD_SUMMARY_PAYLOAD_TYPE;

// ── constants ────────────────────────────────────────────────────────────────

/// Catalog id of the summary model of the harness (`thread_summary_worker.summary_model_id`).
pub const SUMMARY_MODEL: &str = "gpt-summary";
/// The built-in summary model: used when `summary_model_id` is empty (DESIGN B.9.4).
pub const DEFAULT_SUMMARY_MODEL: &str = "gpt-4.1-mini";
/// `max_output_tokens` of the summary model entry (the chat models keep 4096).
pub const SUMMARY_MAX_OUTPUT: u32 = 2048;
/// `max_input_tokens` of the chat model `gpt-premium` in [`summary_models`]: the effective input
/// budget of its turns (`min(2000, 128000 - 4096)`), so the proactive threshold is 1600 tokens and
/// the truncation budget 1900 tokens.
pub const CHAT_MAX_INPUT: u32 = 2000;
/// Queue of the thread-summary messages (`outbox.thread_summary_queue_name` default).
pub const SUMMARY_QUEUE: &str = "mini-chat.thread_summary";
/// `system_task_type` of the work item (DESIGN 3.2 "System Task Attribution Rules").
pub const TASK_TYPE: &str = "thread_summary_update";
/// The platform default subject id: the system identity of the summary request (DESIGN 4
/// "Provider Request Metadata").
pub const SYSTEM_SUBJECT: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
/// Delivered to the model before the summary text (DESIGN B.5.5).
pub const PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";
/// First words of the built-in system prompt of the summary request (DESIGN B.5.5).
pub const DEFAULT_SUMMARY_PROMPT_START: &str = "You are a conversation summarizer.";
/// Opening of the summary request without an existing summary (DESIGN B.5.5).
pub const OPENING_FRESH: &str = "Summarize the following conversation:";
/// Opening of the summary request with an existing summary (DESIGN B.5.5).
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.";
/// Last sentence of the summary request (DESIGN B.5.5).
pub const CLOSING: &str = "Respond with an <analysis> block followed by a <summary> block.";
/// The text a default summary answer carries inside its `<summary>` block.
pub const DEFAULT_SUMMARY_TEXT: &str = "Summary of the earlier conversation.";

// ── catalog ──────────────────────────────────────────────────────────────────

/// The summary model entry: a Standard model with `max_output_tokens` 2048 and an empty
/// `thread_summary_prompt` (so the configured / built-in prompt applies).
pub fn summary_model_json(id: &str) -> Value {
    let mut model = stream_model_json(id, "Standard");
    model["max_output_tokens"] = json!(SUMMARY_MAX_OUTPUT);
    model["preference"] = json!({"is_default": false, "sort_order": 9});
    model
}

/// `gpt-premium` (chat model, `max_input_tokens` = [`CHAT_MAX_INPUT`]), `gpt-standard`, the
/// disabled `gpt-off` and the summary model [`SUMMARY_MODEL`].
pub fn summary_models() -> Vec<Value> {
    let mut models = quota_models();
    patch_model(
        &mut models,
        PREMIUM,
        json!({"max_input_tokens": CHAT_MAX_INPUT}),
    );
    models.push(summary_model_json(SUMMARY_MODEL));
    models
}

/// [`summary_models`] with `max_input_tokens` of the chat model set to `max_input`.
pub fn summary_models_with_input(max_input: u32) -> Vec<Value> {
    let mut models = summary_models();
    patch_model(&mut models, PREMIUM, json!({"max_input_tokens": max_input}));
    models
}

/// Merge `patch` into the catalog entry `id`.
pub fn patch_model(models: &mut [Value], id: &str, patch: Value) {
    let model = models
        .iter_mut()
        .find(|m| m["id"] == json!(id))
        .unwrap_or_else(|| panic!("no model {id} in the catalog"));
    merge(model, &patch);
}

/// Estimated tokens of an item of `bytes` bytes with the budgets of the stream-test catalog
/// (4 bytes per token, overhead 100, margin 10 %): DESIGN 5.5.4, independent of the production code.
pub fn est_bytes(bytes: usize) -> u64 {
    let base = u64::try_from(bytes).unwrap().div_ceil(4) + 100;
    (base * 110).div_ceil(100)
}

/// `label` followed by `x` up to exactly `bytes` bytes.
pub fn filler(label: &str, bytes: usize) -> String {
    assert!(bytes >= label.len(), "label {label} longer than {bytes}");
    format!("{label}{}", "x".repeat(bytes - label.len()))
}

// ── scripted summary answers ─────────────────────────────────────────────────

/// A summary answer in the requested format: an `<analysis>` block and the `<summary>` block
/// holding `summary`, with the usage `(12, 5)`. The summary call is non-streaming, so this is
/// the JSON body of a plain Responses API answer.
pub fn summary_reply(summary: &str) -> ProviderReply {
    summary_reply_with(summary, &Opts::default())
}

/// [`summary_reply`] with explicit usage. Options that shape a stream (pauses, inserted pieces,
/// a missing terminal event) make the answer a `text/event-stream` one in three deltas (the
/// adapter accepts both, the response content type decides).
pub fn summary_reply_with(summary: &str, opts: &Opts) -> ProviderReply {
    if opts.inserts.is_empty() && !opts.no_terminal {
        return ProviderReply::json_text(
            &format!(
                "<analysis>reviewing the exchange</analysis>\n<summary>\n{summary}\n</summary>"
            ),
            opts,
        );
    }
    ProviderReply::text_with(
        &[
            "<analysis>reviewing the exchange</analysis>\n<summary>\n",
            summary,
            "\n</summary>",
        ],
        opts,
    )
}

/// An answer streaming exactly `text` (no markup added), usage `(12, 5)`.
pub fn plain_reply(text: &str) -> ProviderReply {
    ProviderReply::text_with(&[text], &Opts::default())
}

// ── views of a recorded summary request ─────────────────────────────────────

pub trait SummaryRequestView {
    /// The text of the single user input item (the summary prompt).
    fn prompt(&self) -> String;
    /// `metadata` object of the request body.
    fn metadata(&self) -> Value;
    /// `true` when `metadata.request_type` is `summary`.
    fn is_summary(&self) -> bool;
}

impl SummaryRequestView for RecordedProviderRequest {
    fn prompt(&self) -> String {
        let items = self.conversation();
        assert_eq!(
            items.len(),
            1,
            "the summary request has exactly one conversation item: {items:?}"
        );
        assert_eq!(items[0].0, "user", "the summary prompt is a user item");
        items[0].1.clone()
    }

    fn metadata(&self) -> Value {
        self.body["metadata"].clone()
    }

    fn is_summary(&self) -> bool {
        self.body["metadata"]["request_type"] == json!("summary")
    }
}

/// The `User: ...` / `Assistant: ...` entries of a summary prompt: every paragraph (blank-line
/// separated) that starts with one of the two prefixes.
pub fn prompt_entries(prompt: &str) -> Vec<String> {
    prompt
        .split("\n\n")
        .map(str::trim)
        .filter(|p| p.starts_with("User: ") || p.starts_with("Assistant: "))
        .map(str::to_owned)
        .collect()
}

// ── task payload ─────────────────────────────────────────────────────────────

/// An inclusive summary frontier `(created_at, message_id)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frontier {
    pub created_at: OffsetDateTime,
    pub message_id: Uuid,
}

impl Frontier {
    pub fn of(message: &messages::Model) -> Self {
        Self {
            created_at: from_db(message.created_at),
            message_id: message.id,
        }
    }

    pub fn at(created_at: OffsetDateTime, message_id: Uuid) -> Self {
        Self {
            created_at,
            message_id,
        }
    }
}

fn rfc3339(when: OffsetDateTime) -> String {
    when.format(&Rfc3339).expect("rfc3339")
}

/// One thread-summary work item (the JSON payload of the outbox message).
#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub tenant: Uuid,
    pub chat: Uuid,
    pub system_request_id: Uuid,
    pub base: Option<Frontier>,
    pub target: Frontier,
    pub task_type: String,
}

impl TaskSpec {
    /// A task of `chat` (tenant A) that summarizes everything up to `target` with no base.
    pub fn new(chat: Uuid, target: Frontier) -> Self {
        Self {
            tenant: TENANT_A,
            chat,
            system_request_id: Uuid::new_v4(),
            base: None,
            target,
            task_type: TASK_TYPE.to_owned(),
        }
    }

    pub fn based_on(mut self, base: Frontier) -> Self {
        self.base = Some(base);
        self
    }

    pub fn tenant(mut self, tenant: Uuid) -> Self {
        self.tenant = tenant;
        self
    }

    pub fn request(mut self, id: Uuid) -> Self {
        self.system_request_id = id;
        self
    }

    /// The payload exactly as DESIGN 3.2 lists it; absent base frontier fields are JSON `null`.
    pub fn json(&self) -> Value {
        json!({
            "tenant_id": self.tenant,
            "chat_id": self.chat,
            "system_request_id": self.system_request_id,
            "base_frontier_created_at": self.base.map(|f| rfc3339(f.created_at)),
            "base_frontier_message_id": self.base.map(|f| f.message_id),
            "frozen_target_created_at": rfc3339(self.target.created_at),
            "frozen_target_message_id": self.target.message_id,
            "system_task_type": self.task_type,
        })
    }
}

/// Hand `payload` to `handler` as the `attempts`-th retry.
pub async fn handle_with(
    handler: &ThreadSummaryHandler,
    payload: &Value,
    attempts: i16,
) -> MessageResult {
    handler
        .handle(&outbox_message(payload.to_string().into_bytes(), attempts))
        .await
}

/// An outbox message as the shared outbox hands it to a leased handler.
pub fn outbox_message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: THREAD_SUMMARY_PAYLOAD_TYPE.to_owned(),
        created_at: DateTimeUtc::default(),
        attempts,
    }
}

// ── result assertions ────────────────────────────────────────────────────────

pub fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

pub fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

pub fn is_reject(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Reject(_))
}

// ── the scripted providers ───────────────────────────────────────────────────

/// Routes `proxy_request` by the `model` of the JSON body: the provider-side name of the summary
/// model goes to `summaries`, everything else (and every provisioning call) to `chat`.
pub struct SummaryOagw {
    pub chat: Arc<FakeOagw>,
    pub summaries: Arc<FakeOagw>,
    summary_provider_models: Mutex<Vec<String>>,
}

impl SummaryOagw {
    pub fn new(chat: Arc<FakeOagw>, summaries: Arc<FakeOagw>) -> Arc<Self> {
        Arc::new(Self {
            chat,
            summaries,
            summary_provider_models: Mutex::new(vec![
                provider_model(SUMMARY_MODEL),
                provider_model(DEFAULT_SUMMARY_MODEL),
            ]),
        })
    }

    /// Also route the provider-side name of the catalog model `id` to the summary provider.
    pub fn route_model(&self, id: &str) {
        self.summary_provider_models
            .lock()
            .unwrap()
            .push(provider_model(id));
    }

    fn is_summary_model(&self, model: &str) -> bool {
        self.summary_provider_models
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == model)
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for SummaryOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.chat.create_upstream(ctx, req).await
    }

    async fn get_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.chat.get_upstream(ctx, id).await
    }

    async fn list_upstreams(
        &self,
        ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        self.chat.list_upstreams(ctx, query).await
    }

    async fn update_upstream(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.chat.update_upstream(ctx, id, req).await
    }

    async fn delete_upstream(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.chat.delete_upstream(ctx, id).await
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.chat.create_route(ctx, req).await
    }

    async fn get_route(&self, ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.chat.get_route(ctx, id).await
    }

    async fn list_routes(
        &self,
        ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        self.chat.list_routes(ctx, upstream_id, query).await
    }

    async fn update_route(
        &self,
        ctx: SecurityContext,
        id: Uuid,
        req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.chat.update_route(ctx, id, req).await
    }

    async fn delete_route(&self, ctx: SecurityContext, id: Uuid) -> Result<(), CanonicalError> {
        self.chat.delete_route(ctx, id).await
    }

    async fn resolve_proxy_target(
        &self,
        ctx: SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        self.chat
            .resolve_proxy_target(ctx, alias, method, path)
            .await
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let raw = body
            .into_bytes()
            .await
            .map_err(|e| CanonicalError::internal(format!("summary router body: {e}")).create())?;
        let model = serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|v| v["model"].as_str().map(str::to_owned))
            .unwrap_or_default();
        let target = if self.is_summary_model(&model) {
            &self.summaries
        } else {
            &self.chat
        };
        target
            .proxy_request(ctx, http::Request::from_parts(parts, Body::from(raw)))
            .await
    }
}

// ── the policy plugin behind a switch ────────────────────────────────────────

/// The S4 policy plugin; `fail_with` makes `get_policy_snapshot` / `get_current_policy_version`
/// answer an internal error (the model-policy plugin failure of the summary handler).
pub struct SummaryPolicy {
    pub inner: Arc<QuotaPolicy>,
    failure: Mutex<Option<String>>,
}

impl SummaryPolicy {
    pub fn new(inner: Arc<QuotaPolicy>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            failure: Mutex::new(None),
        })
    }

    pub fn fail_with(&self, message: &str) {
        *self.failure.lock().unwrap() = Some(message.to_owned());
    }

    pub fn recover(&self) {
        *self.failure.lock().unwrap() = None;
    }

    fn check(&self) -> Result<(), ModelPolicyPluginError> {
        match self.failure.lock().unwrap().clone() {
            Some(message) => Err(ModelPolicyPluginError::Internal(message)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for SummaryPolicy {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        self.check()?;
        self.inner.get_current_policy_version(user_id).await
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.check()?;
        self.inner
            .get_policy_snapshot(user_id, policy_version)
            .await
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.check()?;
        self.inner.get_user_limits(user_id, policy_version).await
    }

    async fn publish_usage(&self, payload: UsageEvent) -> Result<(), PublishError> {
        self.inner.publish_usage(payload).await
    }
}

// ── recording stand-in for the thread-summary queue ──────────────────────────

/// One message the recording stand-in received.
#[derive(Clone, Debug)]
pub struct DeliveredTask {
    pub payload: Value,
    pub payload_type: String,
    pub partition_id: i64,
    /// `chat_turns.state` of every turn of the task's chat at the instant of the delivery: the
    /// message is only visible after the transaction that made the causing turn durable.
    pub turn_states: Vec<String>,
}

/// Messages delivered to the recording thread-summary handler.
#[derive(Default)]
pub struct TaskLog {
    delivered: Mutex<Vec<DeliveredTask>>,
}

impl TaskLog {
    pub fn received(&self) -> Vec<DeliveredTask> {
        self.delivered.lock().unwrap().clone()
    }

    pub fn payloads(&self) -> Vec<Value> {
        self.received().into_iter().map(|d| d.payload).collect()
    }

    /// Wait (15 s) until at least `n` messages arrived.
    pub async fn wait_for(&self, n: usize) -> Vec<DeliveredTask> {
        for _ in 0..300 {
            let got = self.received();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} thread-summary message(s), got {:?}",
            self.received()
        );
    }

    /// Give the outbox time to deliver, then assert exactly `n` messages arrived.
    pub async fn assert_count_stays(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            self.received().len(),
            n,
            "unexpected thread-summary messages: {:?}",
            self.payloads()
        );
    }
}

struct RecordingSummary {
    log: Arc<TaskLog>,
    db: Arc<DBProvider<DomainError>>,
}

#[async_trait]
impl LeasedMessageHandler for RecordingSummary {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: Value = serde_json::from_slice(&msg.payload)
            .unwrap_or_else(|e| panic!("thread-summary payload must be JSON: {e}"));
        let chat = payload["chat_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
            .unwrap_or_default();
        let conn = self.db.conn().expect("db connection");
        let turn_states = chat_turns::Entity::find()
            .filter(chat_turns::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
            .into_iter()
            .map(|t| t.state)
            .collect();
        self.log.delivered.lock().unwrap().push(DeliveredTask {
            payload,
            payload_type: msg.payload_type.clone(),
            partition_id: msg.partition_id,
            turn_states,
        });
        MessageResult::Ok
    }
}

/// Who serves the thread-summary queue of the harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SummaryQueue {
    /// The real handler (it calls the scripted summary provider and commits the summary).
    Real,
    /// A stand-in that records the message and acknowledges it.
    Recording,
    /// The queue is not registered: every enqueue fails (`QueueNotRegistered`).
    Missing,
}

// ── options ──────────────────────────────────────────────────────────────────

pub struct SummaryOptions {
    pub models: Vec<Value>,
    pub pdp: Arc<FakePdp>,
    /// Merged into the gear configuration (default: `orphan_watchdog.enabled = false`,
    /// `thread_summary_worker.summary_model_id = gpt-summary`, `max_attempts = 3`).
    pub config: Value,
    pub queue: SummaryQueue,
    pub limits: LimitsSpec,
}

impl Default for SummaryOptions {
    fn default() -> Self {
        Self {
            models: summary_models(),
            pdp: FakePdp::allow(),
            config: json!({}),
            queue: SummaryQueue::Recording,
            limits: LimitsSpec::generous(),
        }
    }
}

impl SummaryOptions {
    pub fn real() -> Self {
        Self {
            queue: SummaryQueue::Real,
            ..Default::default()
        }
    }

    pub fn with_config(mut self, config: Value) -> Self {
        self.config = config;
        self
    }

    pub fn with_models(mut self, models: Vec<Value>) -> Self {
        self.models = models;
        self
    }
}

// ── the harness ──────────────────────────────────────────────────────────────

pub struct SummaryHarness {
    /// The S2 harness: database, chat routes, seeds.
    pub chat: ChatHarness,
    /// Chat, stream, turn-status and turn-mutation routes.
    pub router: Router,
    /// The scripted chat provider (answers of the turns, recorded chat requests).
    pub oagw: Arc<FakeOagw>,
    /// The scripted summary provider (answers of the summary calls, recorded summary requests).
    pub summaries: Arc<FakeOagw>,
    pub routing: Arc<SummaryOagw>,
    pub policy: Arc<QuotaPolicy>,
    pub flaky: Arc<SummaryPolicy>,
    pub audit: Arc<RecordingAudit>,
    pub config: Arc<MiniChatConfig>,
    pub pdp: Arc<FakePdp>,
    /// Messages of the thread-summary queue when it runs [`SummaryQueue::Recording`].
    pub log: Arc<TaskLog>,
    /// A handler instance for direct `handle` calls (same dependencies as the queue's).
    pub handler: ThreadSummaryHandler,
    pub summary_port: Arc<ThreadSummaryOutbox>,
    pub usage_port: Arc<UsageOutbox>,
    llm: Arc<LlmProviders>,
    gateway: Arc<ModelPolicyGateway>,
    outbox: OutboxHandle,
}

impl SummaryHarness {
    /// Recording queue, default configuration.
    pub async fn new() -> Self {
        Self::with(SummaryOptions::default()).await
    }

    /// Real handler on the queue, default configuration.
    pub async fn real() -> Self {
        Self::with(SummaryOptions::real()).await
    }

    pub async fn with_config(config: Value) -> Self {
        Self::with(SummaryOptions::default().with_config(config)).await
    }

    pub async fn real_with_config(config: Value) -> Self {
        Self::with(SummaryOptions::real().with_config(config)).await
    }

    pub async fn with(opts: SummaryOptions) -> Self {
        let chat = crate::chat_support::chat_harness_with(ChatHarnessOptions {
            models: model_entries(&opts.models),
            pdp: opts.pdp.clone(),
            ..Default::default()
        })
        .await;
        let oagw = FakeOagw::new();
        let summaries = FakeOagw::new();
        summaries.set_default(|| summary_reply(DEFAULT_SUMMARY_TEXT));
        let routing = SummaryOagw::new(oagw.clone(), summaries.clone());
        let policy = QuotaPolicy::new(&opts.models, 1, opts.limits);
        let flaky = SummaryPolicy::new(policy.clone());
        let audit = Arc::new(RecordingAudit::default());
        let mut cfg = json!({
            "orphan_watchdog": {"enabled": false},
            "thread_summary_worker": {"summary_model_id": SUMMARY_MODEL},
        });
        merge(&mut cfg, &opts.config);
        let config = stream_config(&cfg);

        let gateway = Arc::new(ModelPolicyGateway::from_client(flaky.clone()));
        let audit_plugin: Arc<dyn MiniChatAuditPluginClientV1> = audit.clone();
        let audit_gateway = audit_gateway_for(&config.vendor, Some(audit_plugin));
        let partitions = u16::try_from(config.outbox.num_partitions).expect("partitions");
        let oagw_client: Arc<dyn ServiceGatewayClientV1> = routing.clone();
        let llm = Arc::new(LlmProviders::new(oagw_client, &config));

        let log = Arc::new(TaskLog::default());
        let queue_handler = ThreadSummaryHandler::new(
            chat.db.clone(),
            gateway.clone(),
            llm.clone(),
            config.clone(),
        );
        // The S2 harness already runs an outbox on this database; SQLite answers `database is
        // locked` to a registration that races its workers, so starting ours is retried.
        let mut attempts = 0;
        let outbox = loop {
            let mut builder = Outbox::builder(chat.db.db())
                .profile(OutboxProfile::low_latency())
                .queue(&config.outbox.queue_name, Partitions::of(partitions))
                .leased(UsageOutboxHandler::new(gateway.clone()))
                .done()
                .queue(&config.outbox.audit_queue_name, Partitions::of(partitions))
                .leased(AuditOutboxHandler::new(audit_gateway.clone()))
                .done();
            builder = match opts.queue {
                SummaryQueue::Real => builder
                    .queue(
                        &config.outbox.thread_summary_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(queue_handler.clone())
                    .lease(thread_summary_lease(&config))
                    .done(),
                SummaryQueue::Recording => builder
                    .queue(
                        &config.outbox.thread_summary_queue_name,
                        Partitions::of(partitions),
                    )
                    .leased(RecordingSummary {
                        log: log.clone(),
                        db: chat.db.clone(),
                    })
                    .lease(thread_summary_lease(&config))
                    .done(),
                SummaryQueue::Missing => builder,
            };
            match builder.start().await {
                Ok(outbox) => break outbox,
                Err(error) if attempts < 40 => {
                    attempts += 1;
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => panic!("S7 outbox starts: {error:?}"),
            }
        };

        let usage_port = Arc::new(UsageOutbox::new(
            outbox.outbox().clone(),
            config.outbox.queue_name.clone(),
            partitions,
        ));
        let audit_port = Arc::new(AuditOutbox::new(
            outbox.outbox().clone(),
            config.outbox.audit_queue_name.clone(),
            partitions,
        ));
        let summary_port = Arc::new(ThreadSummaryOutbox::new(
            outbox.outbox().clone(),
            config.outbox.thread_summary_queue_name.clone(),
            partitions,
        ));
        queue_handler.bind_usage(usage_port.clone());
        let handler = ThreadSummaryHandler::new(
            chat.db.clone(),
            gateway.clone(),
            llm.clone(),
            config.clone(),
        );
        handler.bind_usage(usage_port.clone());

        let enforcer = opts.pdp.enforcer();
        let stream = Arc::new(
            StreamService::new(
                chat.db.clone(),
                gateway.clone(),
                llm.clone(),
                config.clone(),
                enforcer.clone(),
            )
            .with_outbox(usage_port.clone(), audit_port.clone())
            .with_thread_summary(summary_port.clone()),
        );
        let turns = Arc::new(TurnsService::new(chat.db.clone(), enforcer.clone()));
        let mutations = Arc::new(
            TurnMutationService::new(chat.db.clone(), stream.clone(), enforcer)
                .with_audit(audit_port),
        );
        let router = register_stream_routes(
            chat.router.clone(),
            chat.openapi.as_ref(),
            stream,
            turns,
            &chat.prefix,
        );
        let router =
            register_turn_mutation_routes(router, chat.openapi.as_ref(), mutations, &chat.prefix);
        Self {
            chat,
            router,
            oagw,
            summaries,
            routing,
            policy,
            flaky,
            audit,
            config,
            pdp: opts.pdp,
            log,
            handler,
            summary_port,
            usage_port,
            llm,
            gateway,
            outbox,
        }
    }

    /// A handler over the same dependencies whose usage messages go to `queue` (a queue that is
    /// not registered makes the enqueue of the commit transaction fail).
    pub fn handler_with_usage_queue(&self, queue: &str) -> ThreadSummaryHandler {
        let partitions = u16::try_from(self.config.outbox.num_partitions).expect("partitions");
        let handler = ThreadSummaryHandler::new(
            self.chat.db.clone(),
            self.gateway.clone(),
            self.llm.clone(),
            self.config.clone(),
        );
        handler.bind_usage(Arc::new(UsageOutbox::new(
            self.outbox.outbox().clone(),
            queue,
            partitions,
        )));
        handler
    }

    /// A handler over the same dependencies with its own configuration.
    pub fn handler_with_config(&self, config: Value) -> ThreadSummaryHandler {
        let mut cfg = json!({
            "orphan_watchdog": {"enabled": false},
            "thread_summary_worker": {"summary_model_id": SUMMARY_MODEL},
        });
        merge(&mut cfg, &config);
        let config = stream_config(&cfg);
        let handler = ThreadSummaryHandler::new(
            self.chat.db.clone(),
            self.gateway.clone(),
            self.llm.clone(),
            config,
        );
        handler.bind_usage(self.usage_port.clone());
        handler
    }

    // ── URIs ─────────────────────────────────────────────────────────────────

    pub fn stream_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages:stream", self.chat.prefix)
    }

    pub fn turn_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/v1/chats/{chat}/turns/{request_id}", self.chat.prefix)
    }

    pub fn retry_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/retry", self.turn_uri(chat, request_id))
    }

    // ── chats and history ────────────────────────────────────────────────────

    /// A chat of `USER_A` on `gpt-premium`.
    pub async fn seed_chat(&self) -> Uuid {
        self.seed_chat_on(PREMIUM).await
    }

    pub async fn seed_chat_on(&self, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(model).at(0))
            .await
    }

    /// A chat with a fixed id (the id decides the outbox partition).
    pub async fn seed_chat_with_id(&self, n: u128) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_id(n).with_model(PREMIUM).at(0))
            .await
    }

    /// A chat of `USER_B` in `TENANT_B`.
    pub async fn seed_chat_of_b(&self) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::new(TENANT_B, USER_B).with_model(PREMIUM).at(0))
            .await
    }

    /// Insert a `chat_turns` row directly; returns its request id.
    pub async fn insert_turn(&self, seed: TurnRowSeed) -> Uuid {
        let conn = self.chat.db.conn().expect("db connection");
        let terminal = seed.state != "running";
        let updated = seed.last_progress_at.unwrap_or(seed.started_at) + time::Duration::seconds(1);
        let reserve = seed.reserve;
        secure_insert::<chat_turns::Entity>(
            chat_turns::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(seed.tenant),
                chat_id: Set(seed.chat_id),
                request_id: Set(seed.request_id),
                requester_type: Set("user".to_owned()),
                requester_user_id: Set(seed.requester),
                state: Set(seed.state),
                provider_name: Set(None),
                provider_response_id: Set(None),
                assistant_message_id: Set(seed.assistant_message_id),
                error_code: Set(seed.error_code),
                reserve_tokens: Set(reserve.map(|r| r.reserve_tokens)),
                max_output_tokens_applied: Set(reserve.map(|r| r.max_output_tokens)),
                reserved_credits_micro: Set(reserve.map(|r| r.reserved_credits_micro)),
                policy_version_applied: Set(reserve.map(|r| r.policy_version)),
                effective_model: Set(seed.effective_model),
                minimal_generation_floor_applied: Set(reserve.map(|r| r.floor)),
                error_detail: Set(None),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
                replaced_by_request_id: Set(seed.replaced_by),
                started_at: Set(to_db(seed.started_at)),
                last_progress_at: Set(to_db_opt(seed.last_progress_at)),
                web_search_enabled: Set(seed.web_search_enabled),
                web_search_completed_count: Set(seed.web_search_done),
                code_interpreter_completed_count: Set(seed.code_interpreter_done),
                file_search_completed_count: Set(seed.file_search_done),
                completed_at: Set(terminal.then(|| to_db(updated))),
                updated_at: Set(Some(to_db(updated))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed turn");
        seed.request_id
    }

    /// Seed one turn the way the API leaves it (user message at `seed.at`, assistant message one
    /// second later, the `chat_turns` row).
    pub async fn seed_pair(&self, chat: Uuid, seed: PairSeed) -> Pair {
        let started_at = ts(seed.at);
        let mut user = MessageSeed::user(chat)
            .request(Some(seed.request_id))
            .content(&seed.question)
            .at(seed.at);
        if let Some(id) = seed.user_id {
            user.id = id;
        }
        if seed.deleted {
            user = user.deleted();
        }
        let user_message = self.chat.seed_message(user).await;
        let assistant_message = match &seed.answer {
            Some(answer) => {
                let mut assistant = MessageSeed::assistant(chat)
                    .request(Some(seed.request_id))
                    .content(answer)
                    .model(&seed.model)
                    .tokens(seed.tokens.0, seed.tokens.1)
                    .at(seed.at + 1);
                if seed.deleted {
                    assistant = assistant.deleted();
                }
                Some(self.chat.seed_message(assistant).await)
            }
            None => None,
        };
        let mut row = TurnRowSeed::terminal(chat, &seed.state)
            .request(seed.request_id)
            .started(started_at)
            .effective(&seed.model)
            .requester(Some(seed.requester))
            .web_search(seed.web_search);
        row.error_code = seed.error_code.clone();
        row.assistant_message_id = assistant_message;
        if seed.deleted {
            row.deleted_at = Some(started_at + time::Duration::seconds(5));
        }
        let turn_id = row.id;
        self.insert_turn(row).await;
        Pair {
            request_id: seed.request_id,
            turn_id,
            user_message,
            assistant_message,
            started_at,
        }
    }

    /// `n` completed turns `question i` / `answer i` at 10 s, 20 s, ... (short messages).
    pub async fn seed_history(&self, chat: Uuid, n: usize) -> Vec<Pair> {
        let mut pairs = Vec::new();
        for i in 1..=n {
            pairs.push(
                self.seed_pair(
                    chat,
                    PairSeed::completed(&format!("question {i}"), &format!("answer {i}"))
                        .at(i64::try_from(i).unwrap() * 10),
                )
                .await,
            );
        }
        pairs
    }

    /// `n` completed turns whose two messages are `bytes` bytes each (`Q1xxx...`, `A1xxx...`) at
    /// 10 s, 20 s, ...
    pub async fn seed_sized(&self, chat: Uuid, n: usize, bytes: usize) -> Vec<Pair> {
        let mut pairs = Vec::new();
        for i in 1..=n {
            pairs.push(
                self.seed_pair(
                    chat,
                    PairSeed::completed(
                        &filler(&format!("Q{i}"), bytes),
                        &filler(&format!("A{i}"), bytes),
                    )
                    .at(i64::try_from(i).unwrap() * 10),
                )
                .await,
            );
        }
        pairs
    }

    /// Insert a `thread_summaries` row with `text` whose frontier is `(frontier_at, frontier_message)`.
    pub async fn seed_summary_text(
        &self,
        chat: Uuid,
        frontier_at: OffsetDateTime,
        frontier_message: Uuid,
        text: &str,
        token_estimate: i32,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<thread_summaries::Entity>(
            thread_summaries::ActiveModel {
                id: Set(id),
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                summary_text: Set(Some(text.to_owned())),
                summarized_up_to_created_at: Set(to_db(frontier_at)),
                summarized_up_to_message_id: Set(frontier_message),
                token_estimate: Set(Some(token_estimate)),
                created_at: Set(to_db(ts(0))),
                updated_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed summary");
        id
    }

    /// [`Self::seed_summary_text`] with the text `"a summary of the earlier conversation"` and
    /// `token_estimate` 12 (the S5 seed).
    pub async fn seed_summary(
        &self,
        chat: Uuid,
        frontier_at: OffsetDateTime,
        frontier_message: Uuid,
    ) -> Uuid {
        self.seed_summary_text(
            chat,
            frontier_at,
            frontier_message,
            "a summary of the earlier conversation",
            12,
        )
        .await
    }

    /// A summary whose frontier is the message `message` (read back from the database), flagging
    /// every message up to it (`(created_at, id)` order, soft-deleted ones excluded) compressed, as
    /// a finished worker leaves the chat.
    pub async fn seed_summarized_up_to(&self, chat: Uuid, message: Uuid, text: &str) -> Uuid {
        let frontier = self.frontier_of(message).await;
        let covered: Vec<Uuid> = self
            .messages_in(chat)
            .await
            .into_iter()
            .filter(|m| m.deleted_at.is_none())
            .filter(|m| (m.created_at, m.id) <= (to_db(frontier.created_at), frontier.message_id))
            .map(|m| m.id)
            .collect();
        self.mark_compressed(&covered).await;
        self.seed_summary_text(chat, frontier.created_at, frontier.message_id, text, 12)
            .await
    }

    /// Set `is_compressed = true` on `ids`.
    pub async fn mark_compressed(&self, ids: &[Uuid]) {
        let conn = self.chat.db.conn().expect("db connection");
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(true))
            .filter(messages::Column::Id.is_in(ids.iter().copied()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("mark compressed");
    }

    /// Clear `is_compressed` on every message of `chat` (what the mutation transaction does).
    pub async fn clear_compressed(&self, chat: Uuid) {
        let conn = self.chat.db.conn().expect("db connection");
        messages::Entity::update_many()
            .col_expr(messages::Column::IsCompressed, Expr::value(false))
            .filter(messages::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("clear compressed");
    }

    /// Soft-delete messages (what a retry, edit or delete does), without any other effect.
    pub async fn soft_delete_messages(&self, ids: &[Uuid]) {
        let conn = self.chat.db.conn().expect("db connection");
        messages::Entity::update_many()
            .col_expr(messages::Column::DeletedAt, Expr::value(to_db(ts(5_000))))
            .filter(messages::Column::Id.is_in(ids.iter().copied()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("soft delete messages");
    }

    /// Delete the summary row of `chat` directly (what the mutation transaction does).
    pub async fn delete_summary_row(&self, chat: Uuid) {
        use toolkit_db::secure::SecureDeleteExt;
        let conn = self.chat.db.conn().expect("db connection");
        thread_summaries::Entity::delete_many()
            .filter(thread_summaries::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("delete summary");
    }

    /// Soft-delete a chat directly.
    pub async fn soft_delete_chat(&self, chat: Uuid) {
        let conn = self.chat.db.conn().expect("db connection");
        chats::Entity::update_many()
            .col_expr(chats::Column::DeletedAt, Expr::value(to_db(ts(5_000))))
            .filter(chats::Column::Id.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&conn)
            .await
            .expect("soft delete chat");
    }

    /// The frontier of an existing message.
    pub async fn frontier_of(&self, message: Uuid) -> Frontier {
        let row = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .find(|m| m.id == message)
            .unwrap_or_else(|| panic!("no message {message}"));
        Frontier::of(&row)
    }

    // ── sending ──────────────────────────────────────────────────────────────

    /// Any request with an optional security context, content type and raw body.
    pub async fn raw(
        &self,
        method: &str,
        uri: &str,
        ctx: Option<&SecurityContext>,
        content_type: Option<&str>,
        body: &str,
    ) -> RawStream {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let mut req = builder
            .body(AxumBody::from(body.to_owned()))
            .expect("request");
        if let Some(ctx) = ctx {
            req.extensions_mut().insert(ctx.clone());
        }
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        RawStream::from_response(resp)
    }

    /// `POST messages:stream` as `ctx`; the body is not read.
    pub async fn open(&self, chat: Uuid, ctx: &SecurityContext, body: &Value) -> RawStream {
        self.raw(
            "POST",
            &self.stream_uri(chat),
            Some(ctx),
            Some("application/json"),
            &body.to_string(),
        )
        .await
    }

    /// Send as `USER_A`, read the whole SSE answer.
    pub async fn send(&self, chat: Uuid, body: &Value) -> Sent {
        self.open(chat, &ctx_a(), body).await.collect().await
    }

    pub async fn say(&self, chat: Uuid, text: &str) -> Sent {
        self.send(chat, &json!({"content": text})).await
    }

    pub async fn say_with(&self, chat: Uuid, text: &str, request_id: Uuid) -> Sent {
        self.send(chat, &json!({"content": text, "request_id": request_id}))
            .await
    }

    /// Say `text` and require a completed answer (`done`).
    pub async fn say_ok(&self, chat: Uuid, text: &str) -> Sent {
        let sent = self.say(chat, text).await;
        assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
        sent
    }

    pub async fn retry(&self, chat: Uuid, rid: Uuid) -> RawStream {
        self.raw("POST", &self.retry_uri(chat, rid), Some(&ctx_a()), None, "")
            .await
    }

    pub async fn edit(&self, chat: Uuid, rid: Uuid, content: &str) -> RawStream {
        self.raw(
            "PATCH",
            &self.turn_uri(chat, rid),
            Some(&ctx_a()),
            Some("application/json"),
            &json!({"content": content}).to_string(),
        )
        .await
    }

    pub async fn delete(&self, chat: Uuid, rid: Uuid) -> Resp {
        self.raw(
            "DELETE",
            &self.turn_uri(chat, rid),
            Some(&ctx_a()),
            None,
            "",
        )
        .await
        .into_resp()
        .await
    }

    pub async fn retried(&self, chat: Uuid, rid: Uuid) -> Sent {
        self.retry(chat, rid).await.collect().await
    }

    pub async fn edited(&self, chat: Uuid, rid: Uuid, content: &str) -> Sent {
        self.edit(chat, rid, content).await.collect().await
    }

    /// Run `op` as `USER_A` and require success (`done` for retry / edit, 204 for delete).
    pub async fn mutate(&self, op: Op, chat: Uuid, rid: Uuid) {
        match op {
            Op::Retry => assert_eq!(self.retried(chat, rid).await.terminal().event, "done"),
            Op::Edit => assert_eq!(
                self.edited(chat, rid, EDIT_TEXT).await.terminal().event,
                "done"
            ),
            Op::Delete => assert_eq!(self.delete(chat, rid).await.status.as_u16(), 204),
        }
    }

    // ── direct delivery to the real handler ──────────────────────────────────

    /// Deliver `payload` to the real handler as the `attempts`-th retry (`0` = first delivery).
    pub async fn deliver(&self, payload: &Value, attempts: i16) -> MessageResult {
        self.deliver_raw(payload.to_string().as_bytes(), attempts)
            .await
    }

    pub async fn deliver_raw(&self, raw: &[u8], attempts: i16) -> MessageResult {
        self.handler
            .handle(&outbox_message(raw.to_vec(), attempts))
            .await
    }

    /// Deliver `payload` on another task (for the tests that act while the provider call is held).
    pub fn spawn_deliver(
        &self,
        payload: &Value,
        attempts: i16,
    ) -> tokio::task::JoinHandle<MessageResult> {
        let handler = self.handler.clone();
        let raw = payload.to_string().into_bytes();
        tokio::spawn(async move { handler.handle(&outbox_message(raw, attempts)).await })
    }

    /// Deliver `task` on the first attempt.
    pub async fn deliver_task(&self, task: &TaskSpec) -> MessageResult {
        self.deliver(&task.json(), 0).await
    }

    // ── readers (unscoped: they assert what the services did) ────────────────

    pub async fn messages_in(&self, chat: Uuid) -> Vec<messages::Model> {
        let mut rows: Vec<_> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .filter(|m| m.chat_id == chat)
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows
    }

    /// `(id, is_compressed)` of every message of `chat`, soft-deleted ones included, in order.
    pub async fn compressed_flags(&self, chat: Uuid) -> Vec<(Uuid, bool)> {
        self.messages_in(chat)
            .await
            .into_iter()
            .map(|m| (m.id, m.is_compressed))
            .collect()
    }

    /// Ids of the compressed messages of `chat`.
    pub async fn compressed_ids(&self, chat: Uuid) -> Vec<Uuid> {
        self.compressed_flags(chat)
            .await
            .into_iter()
            .filter(|(_, c)| *c)
            .map(|(id, _)| id)
            .collect()
    }

    pub async fn summaries_of(&self, chat: Uuid) -> Vec<thread_summaries::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        thread_summaries::Entity::find()
            .filter(thread_summaries::Column::ChatId.eq(chat))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read summaries")
    }

    /// The only summary row of `chat` (must exist).
    pub async fn summary_of(&self, chat: Uuid) -> thread_summaries::Model {
        let mut rows = self.summaries_of(chat).await;
        assert_eq!(rows.len(), 1, "one summary row of {chat}: {rows:?}");
        rows.remove(0)
    }

    /// Wait (10 s) until `chat` has a summary row and return it.
    pub async fn wait_summary(&self, chat: Uuid) -> thread_summaries::Model {
        for _ in 0..1000 {
            if let Some(row) = self.summaries_of(chat).await.into_iter().next() {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("chat {chat} never got a summary");
    }

    /// Wait (10 s) until the summary of `chat` carries `text`.
    pub async fn wait_summary_text(&self, chat: Uuid, text: &str) -> thread_summaries::Model {
        for _ in 0..1000 {
            if let Some(row) = self
                .summaries_of(chat)
                .await
                .into_iter()
                .find(|r| r.summary_text.as_deref() == Some(text))
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("chat {chat} never got the summary text {text:?}");
    }

    pub async fn all_turns(&self) -> Vec<chat_turns::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
    }

    pub async fn turns_in(&self, chat: Uuid) -> Vec<chat_turns::Model> {
        let mut rows: Vec<_> = self
            .all_turns()
            .await
            .into_iter()
            .filter(|t| t.chat_id == chat)
            .collect();
        rows.sort_by_key(|t| (t.started_at, t.id));
        rows
    }

    pub async fn usage_rows(&self) -> Vec<quota_usage::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        quota_usage::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read quota_usage")
    }

    /// Fingerprint of what the summary worker must not touch when it skips: messages (deletion,
    /// compression), summary rows (id, text, frontier), turns, quota rows.
    pub async fn fingerprint(&self) -> Value {
        let mut messages: Vec<Value> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .map(|m| json!([m.id, m.deleted_at.map(|d| d.to_rfc3339()), m.is_compressed]))
            .collect();
        messages.sort_by_key(Value::to_string);
        let conn = self.chat.db.conn().expect("db connection");
        let mut summaries: Vec<Value> = thread_summaries::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read summaries")
            .into_iter()
            .map(|s| {
                json!([
                    s.id,
                    s.chat_id,
                    s.summary_text,
                    s.summarized_up_to_message_id,
                    s.summarized_up_to_created_at.to_rfc3339(),
                    s.token_estimate,
                    s.updated_at.to_rfc3339(),
                ])
            })
            .collect();
        summaries.sort_by_key(Value::to_string);
        let mut turns: Vec<Value> = self
            .all_turns()
            .await
            .into_iter()
            .map(|t| {
                json!([
                    t.request_id,
                    t.state,
                    t.error_code,
                    t.deleted_at.map(|d| d.to_rfc3339())
                ])
            })
            .collect();
        turns.sort_by_key(Value::to_string);
        let mut quota: Vec<Value> = self
            .usage_rows()
            .await
            .into_iter()
            .map(|r| {
                json!([
                    r.bucket,
                    r.period_type,
                    r.spent_credits_micro,
                    r.reserved_credits_micro,
                    r.calls
                ])
            })
            .collect();
        quota.sort_by_key(Value::to_string);
        json!({"messages": messages, "summaries": summaries, "turns": turns, "quota": quota})
    }

    // ── provider views ───────────────────────────────────────────────────────

    /// Requests that reached the summary provider.
    pub fn summary_requests(&self) -> Vec<RecordedProviderRequest> {
        self.summaries.requests()
    }

    /// Requests that reached the chat provider.
    pub fn chat_requests(&self) -> Vec<RecordedProviderRequest> {
        self.oagw.requests()
    }

    pub fn last_summary_request(&self) -> RecordedProviderRequest {
        self.summaries.last_request()
    }

    /// Wait (10 s) until the summary provider received `n` requests.
    pub async fn wait_summary_requests(&self, n: usize) -> Vec<RecordedProviderRequest> {
        for _ in 0..1000 {
            if self.summaries.request_count() >= n {
                return self.summaries.requests();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} summary request(s), got {}",
            self.summaries.request_count()
        );
    }

    /// Give the pipeline time, then assert the summary provider received exactly `n` requests.
    pub async fn assert_summary_requests_stay(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            self.summaries.request_count(),
            n,
            "unexpected summary requests"
        );
    }

    // ── usage events ─────────────────────────────────────────────────────────

    /// The usage events the plugin accepted whose billing outcome is `system_task`.
    pub fn system_usage(&self) -> Vec<UsageEvent> {
        self.policy
            .published()
            .into_iter()
            .filter(|e| e.billing_outcome == BillingOutcome::SystemTask)
            .collect()
    }

    /// Wait (15 s) until `n` system-task usage events were delivered; returns them.
    pub async fn wait_system_usage(&self, n: usize) -> Vec<UsageEvent> {
        for _ in 0..300 {
            let got = self.system_usage();
            if got.len() >= n {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} system-task usage event(s), got {:?}",
            self.system_usage()
        );
    }

    /// Give the pipeline time, then assert exactly `n` system-task usage events exist.
    pub async fn assert_system_usage_stays(&self, n: usize) {
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(
            self.system_usage().len(),
            n,
            "unexpected system-task usage events: {:?}",
            self.system_usage()
        );
    }

    // ── dead letters ─────────────────────────────────────────────────────────

    /// Dead-lettered messages of `queue`.
    pub async fn dead_letters(&self, queue: &str) -> u64 {
        let conn = self.chat.db.conn().expect("db connection");
        self.outbox
            .outbox()
            .dead_letter_count(
                &conn,
                &DeadLetterFilter::from_scope(DeadLetterScope::default().queue(queue)).any_status(),
            )
            .await
            .expect("dead letter count")
    }

    /// Dead-lettered thread-summary messages.
    pub async fn summary_dead_letters(&self) -> u64 {
        self.dead_letters(&self.config.outbox.thread_summary_queue_name)
            .await
    }

    /// Wait (15 s) until `n` thread-summary messages were dead-lettered.
    pub async fn wait_summary_dead_letters(&self, n: u64) {
        for _ in 0..300 {
            if self.summary_dead_letters().await >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} dead-lettered thread-summary message(s), got {}",
            self.summary_dead_letters().await
        );
    }

    /// The `thread_summary_applied` object of the `stream_started` event of `sent`.
    pub fn summary_applied(sent: &Sent) -> Option<Value> {
        sent.started()
            .get("thread_summary_applied")
            .filter(|v| !v.is_null())
            .cloned()
    }
}

/// `(role, text)` view of the conversation items of a chat request as `"role:text"` strings.
pub fn texts(request: &RecordedProviderRequest) -> Vec<String> {
    request.conversation_texts()
}

/// `true` when `item` (a `"user:..."` conversation text) is the summary message: a user item that
/// starts with the preamble.
pub fn is_summary_item(item: &str) -> bool {
    item.strip_prefix("user:")
        .is_some_and(|rest| rest.starts_with(PREAMBLE))
}

/// The summary text of a summary item: everything after the preamble, trimmed.
pub fn summary_text_of(item: &str) -> String {
    item.strip_prefix("user:")
        .and_then(|rest| rest.strip_prefix(PREAMBLE))
        .unwrap_or_else(|| panic!("not a summary item: {item}"))
        .trim()
        .to_owned()
}
