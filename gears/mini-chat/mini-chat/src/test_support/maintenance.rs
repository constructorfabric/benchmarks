//! The harness of the background-maintenance feature (`cpt-cf-mini-chat-feature-background-maintenance`):
//! the recording metrics double, the doubles of the ports of both handlers (summary store,
//! summary transport, chat-cleanup store, storage provider, policy gateway, usage queue) with
//! scripts that advance the virtual time, the payload and outbox-message builders, the provider
//! registry and catalog with one summary model per adapter kind, the response fixtures per kind
//! and the in-process rig over `SQLite` and the mock OAGW.
//!
//! If the author stage changes the constructors of the workers or their dependency structs,
//! [`summary_worker_of`], [`cleanup_worker_of`] and [`MaintRig`] are the only places to adapt.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    reason = "test support"
)]

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot, UsageEvent, UserLimits};
use oagw_sdk::api::ServiceGatewayClientV1;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use toolkit_db::outbox::{MessageResult, OutboxMessage, Wake};
use toolkit_db::secure::DbTx;
use uuid::Uuid;

use super::TestDb;
use super::attachments::{OPENAI_BACKEND, seed_vector_store};
use super::chats::{OutboxRow, a1, outbox_rows, seed_chat};
use super::manual_time::ManualTime;
use super::provider::MockProvider;
use super::quota::{QuotaRig, QuotaRigOptions, UsageQueueKind, default_now, quota_rig_with};
use super::s2::{MetricsHarness, metrics_harness};
use super::send::{
    ANTHROPIC_MODEL, CC_MODEL, MessageSeed, VLLM_MODEL, catalog_of, prem_model, provider_s2s,
    seed_msg, std_model, t,
};
use super::storage_mock::MockStorage;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::attachments::cleanup::AttemptRecord;
use crate::domain::attachments::provider::{
    FileUploadMeta, ProviderFailure, StorageProvider, StorageTarget, UploadBody, UploadError,
};
use crate::domain::attachments::timing::Timer;
use crate::domain::error::DomainError;
use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::chat_cleanup::{ChatCleanupDeps, ChatCleanupWorker};
use crate::domain::maintenance::chat_store::{
    ChatCleanupStore, ChatFacts, PendingAttachment, VectorStoreRow,
};
use crate::domain::maintenance::metrics::{CleanupMetrics, SummaryMetrics};
use crate::domain::maintenance::settings::{CleanupSettings, SummaryWorkerSettings};
use crate::domain::maintenance::summary_call::{HttpAnswer, SummaryTransport, TransportFailure};
use crate::domain::maintenance::summary_prompt::RangeMessage;
use crate::domain::maintenance::summary_store::{
    CommitOutcome, PreCheck, SummaryCommit, SummaryStore,
};
use crate::domain::maintenance::summary_worker::{SummaryDeps, ThreadSummaryWorker};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::ProviderRegistry;
use crate::domain::quota::{Clock, UsageQueue};
use crate::domain::streaming::summary::{Frontier, ThreadSummaryWork};
use crate::infra::chat_cleanup_handler::ChatCleanupHandler;
use crate::infra::oagw::storage_client::OagwStorageClient;
use crate::infra::oagw::summary_client::OagwSummaryClient;
use crate::infra::outbox::system_task_dedupe_key;
use crate::infra::provider_task::S2sContext;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::infra::thread_summary_handler::ThreadSummaryHandler;

// ── names ────────────────────────────────────────────────────────────────────

/// The summary models of the catalog, one per adapter kind.
pub const SUMMARY_MODEL: &str = "summarizer-1";
pub const SUMMARY_CC: &str = "summ-cc-1";
pub const SUMMARY_VLLM: &str = "summ-vllm-1";
pub const SUMMARY_ANTHROPIC: &str = "summ-ant-1";
pub const SUMMARY_AZURE: &str = "summ-azure-1";

/// The upstream aliases of the providers of [`providers`].
pub const AZURE_ALIAS: &str = "azure.example.com";
pub const CC_ALIAS: &str = "cc.example.com";
pub const VLLM_ALIAS: &str = "vllm.example.com";
pub const ANTHROPIC_ALIAS: &str = "anthropic.example.com";
pub const AZURE_RESP_ALIAS: &str = "res.openai.azure.com";
/// The `api_path` of the Azure Responses provider: `{model}` and a query.
pub const AZURE_RESP_PATH: &str =
    "/openai/deployments/{model}/responses?api-version=2025-03-01-preview";
/// The `storage_backend` label of the `azure_openai` provider.
pub const BACKEND: &str = OPENAI_BACKEND;

/// The system identity of the summary request, 32-character hex.
pub const SYSTEM_SUBJECT_HEX: &str = "111111116a8847689dfc6bcd5187d9ed";

// ── providers and catalog ────────────────────────────────────────────────────

/// The provider configuration: `azure_openai` (`openai_responses`), `cc`
/// (`openai_chat_completions`), `vllm` (`vllm_responses`), `anthropic` (`anthropic_messages`) and
/// `azure_resp` (`openai_responses` with the Azure `api_path`).
pub fn providers_json() -> Value {
    json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": { "kind": "openai_responses", "host": AZURE_ALIAS,
                               "storage_kind": "openai" },
            "cc": { "kind": "openai_chat_completions", "host": CC_ALIAS,
                     "storage_kind": "openai", "api_path": "/v1/chat/completions" },
            "vllm": { "kind": "vllm_responses", "host": VLLM_ALIAS, "storage_kind": "openai" },
            "anthropic": { "kind": "anthropic_messages", "host": ANTHROPIC_ALIAS,
                            "storage_kind": "openai", "api_path": "/v1/messages" },
            "azure_resp": { "kind": "openai_responses", "host": AZURE_RESP_ALIAS,
                             "storage_kind": "azure", "api_version": "2025-03-01-preview",
                             "api_path": AZURE_RESP_PATH,
                             "storage_backend": "azure-resp-label" }
        }
    })
}

/// A registry over `config` (a `MiniChatConfig` JSON value).
pub fn registry_of(config: Value) -> Arc<ProviderRegistry> {
    let config = MiniChatConfig::from_value(config).expect("valid config");
    Arc::new(ProviderRegistry::build(&config.providers))
}

/// The registry of [`providers_json`].
pub fn providers() -> Arc<ProviderRegistry> {
    registry_of(providers_json())
}

/// `base` with the top-level keys of `patch` replaced.
pub fn merged(mut base: Value, patch: &Value) -> Value {
    if let Some(patch) = patch.as_object() {
        for (key, value) in patch {
            base[key] = value.clone();
        }
    }
    base
}

/// A summary model entry on `provider` (`provider_model_id` `provider-{id}`), the patch applied
/// on top of the standard model of the rigs.
pub fn summary_entry(id: &str, provider: &str, patch: &Value) -> ModelCatalogEntry {
    std_model(&merged(
        json!({ "id": id, "provider_id": provider,
                "provider_model_id": format!("provider-{id}") }),
        patch,
    ))
}

/// The catalog: the premium and standard models of the rigs, one summary model per adapter kind
/// and the chat models `cc-1`, `vllm-1` and `anthropic-1` of the multi-adapter catalog.
pub fn catalog() -> PolicySnapshot {
    let on = |id: &str, provider: &str| summary_entry(id, provider, &json!({}));
    catalog_of(vec![
        prem_model(&json!({})),
        std_model(&json!({ "provider_id": "azure_openai" })),
        on(SUMMARY_MODEL, "azure_openai"),
        on(SUMMARY_CC, "cc"),
        on(SUMMARY_VLLM, "vllm"),
        on(SUMMARY_ANTHROPIC, "anthropic"),
        on(SUMMARY_AZURE, "azure_resp"),
        on(CC_MODEL, "cc"),
        on(VLLM_MODEL, "vllm"),
        on(ANTHROPIC_MODEL, "anthropic"),
    ])
}

/// `catalog()` with the entry `id` replaced by `entry` (or appended).
pub fn catalog_with(entry: ModelCatalogEntry) -> PolicySnapshot {
    let mut snapshot = catalog();
    snapshot.model_catalog.retain(|e| e.id != entry.id);
    snapshot.model_catalog.push(entry);
    snapshot
}

/// The catalog without the entry `id`.
pub fn catalog_without(id: &str) -> PolicySnapshot {
    let mut snapshot = catalog();
    snapshot.model_catalog.retain(|e| e.id != id);
    snapshot
}

/// The summary settings of the tests: `summary_model_id` set, the rest as the defaults.
pub fn settings_for(model: &str) -> SummaryWorkerSettings {
    SummaryWorkerSettings {
        summary_model_id: model.to_owned(),
        summary_system_prompt: crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned(),
        message_content_limit: 4000,
        max_attempts: 3,
        claim_timeout: Duration::from_secs(300),
    }
}

// ── recording metrics ────────────────────────────────────────────────────────

#[derive(Default)]
struct Recorded {
    executions: Vec<String>,
    cas_conflicts: usize,
    fallbacks: usize,
    completed: Vec<String>,
    retries: Vec<(String, String)>,
    failed: Vec<String>,
    debts: usize,
    skipped: Vec<String>,
}

/// A recording double of [`SummaryMetrics`] and [`CleanupMetrics`].
#[derive(Default)]
pub struct RecordingMaintenanceMetrics {
    inner: Mutex<Recorded>,
}

impl RecordingMaintenanceMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn executions(&self) -> Vec<String> {
        self.inner.lock().unwrap().executions.clone()
    }

    pub fn cas_conflicts(&self) -> usize {
        self.inner.lock().unwrap().cas_conflicts
    }

    pub fn fallbacks(&self) -> usize {
        self.inner.lock().unwrap().fallbacks
    }

    /// The `resource_type` of every `completed`.
    pub fn completed(&self) -> Vec<String> {
        self.inner.lock().unwrap().completed.clone()
    }

    /// The `(resource_type, reason)` of every `retry`.
    pub fn retries(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().retries.clone()
    }

    /// The `resource_type` of every `failed`.
    pub fn failed(&self) -> Vec<String> {
        self.inner.lock().unwrap().failed.clone()
    }

    pub fn debts(&self) -> usize {
        self.inner.lock().unwrap().debts
    }

    /// The `provider_kind` of every `secondary_skipped`.
    pub fn skipped(&self) -> Vec<String> {
        self.inner.lock().unwrap().skipped.clone()
    }

    /// Nothing at all was recorded.
    pub fn is_silent(&self) -> bool {
        let r = self.inner.lock().unwrap();
        r.executions.is_empty()
            && r.cas_conflicts == 0
            && r.fallbacks == 0
            && r.completed.is_empty()
            && r.retries.is_empty()
            && r.failed.is_empty()
            && r.debts == 0
            && r.skipped.is_empty()
    }
}

impl SummaryMetrics for RecordingMaintenanceMetrics {
    fn execution(&self, result: &str) {
        self.inner
            .lock()
            .unwrap()
            .executions
            .push(result.to_owned());
    }

    fn cas_conflict(&self) {
        self.inner.lock().unwrap().cas_conflicts += 1;
    }

    fn fallback(&self) {
        self.inner.lock().unwrap().fallbacks += 1;
    }
}

impl CleanupMetrics for RecordingMaintenanceMetrics {
    fn completed(&self, resource_type: &str) {
        self.inner
            .lock()
            .unwrap()
            .completed
            .push(resource_type.to_owned());
    }

    fn retry(&self, resource_type: &str, reason: &str) {
        self.inner
            .lock()
            .unwrap()
            .retries
            .push((resource_type.to_owned(), reason.to_owned()));
    }

    fn failed(&self, resource_type: &str) {
        self.inner
            .lock()
            .unwrap()
            .failed
            .push(resource_type.to_owned());
    }

    fn vector_store_with_failed_attachments(&self) {
        self.inner.lock().unwrap().debts += 1;
    }

    fn secondary_skipped(&self, provider_kind: &str) {
        self.inner
            .lock()
            .unwrap()
            .skipped
            .push(provider_kind.to_owned());
    }
}

// ── payloads and messages ────────────────────────────────────────────────────

/// RFC 3339 text of an instant.
pub fn rfc3339(instant: OffsetDateTime) -> String {
    instant.format(&Rfc3339).expect("rfc3339")
}

/// The JSON payload of a thread-summary message as the enqueue writes it.
pub fn summary_payload(
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
    system_request_id: Uuid,
) -> Value {
    json!({
        "tenant_id": tenant_id,
        "chat_id": chat_id,
        "system_request_id": system_request_id,
        "system_task_type": "thread_summary_update",
        "base_frontier_created_at": base.map(|b| rfc3339(b.0)),
        "base_frontier_message_id": base.map(|b| b.1),
        "frozen_target_created_at": rfc3339(target.0),
        "frozen_target_message_id": target.1,
        "dedupe_key": system_task_dedupe_key(tenant_id, "thread_summary_update", system_request_id),
    })
}

/// The JSON payload of a chat-cleanup message as the DELETE transaction enqueues it.
pub fn chat_cleanup_payload(tenant_id: Uuid, chat_id: Uuid, system_request_id: Uuid) -> Value {
    json!({
        "tenant_id": tenant_id,
        "chat_id": chat_id,
        "system_request_id": system_request_id,
        "reason": "chat_soft_delete",
        "chat_deleted_at": "2026-02-28T15:30:00Z",
        "dedupe_key": system_task_dedupe_key(tenant_id, "chat_cleanup", system_request_id),
    })
}

/// The payload of a chat-cleanup message of `chat`.
pub fn cleanup_payload_of(chat: &ChatRecord) -> Value {
    chat_cleanup_payload(chat.tenant_id, chat.id, Uuid::new_v4())
}

/// An outbox message of `payload_type` with `attempts` (0 on the first delivery).
pub fn message_of(payload_type: &str, payload: &[u8], attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: payload.to_vec(),
        payload_type: payload_type.to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

/// The thread-summary message of a JSON payload.
pub fn summary_message(payload: &Value, attempts: i16) -> OutboxMessage {
    message_of(
        "mini_chat.thread_summary",
        &serde_json::to_vec(payload).unwrap(),
        attempts,
    )
}

/// The chat-cleanup message of a JSON payload.
pub fn cleanup_message(payload: &Value, attempts: i16) -> OutboxMessage {
    message_of(
        "mini_chat.chat_cleanup",
        &serde_json::to_vec(payload).unwrap(),
        attempts,
    )
}

/// `ok`, `retry` or `reject` for a domain outcome.
pub fn label(outcome: &HandlerOutcome) -> &'static str {
    match outcome {
        HandlerOutcome::Ok => "ok",
        HandlerOutcome::Retry => "retry",
        HandlerOutcome::Reject(_) => "reject",
    }
}

/// The reject reason of a domain outcome.
pub fn reason(outcome: &HandlerOutcome) -> Option<String> {
    match outcome {
        HandlerOutcome::Reject(reason) => Some(reason.clone()),
        _ => None,
    }
}

/// `ok`, `retry` or `reject` for an outbox result.
pub fn result_label(result: &MessageResult) -> &'static str {
    match result {
        MessageResult::Ok => "ok",
        MessageResult::Retry => "retry",
        MessageResult::Reject(_) => "reject",
    }
}

/// The reject reason of an outbox result.
pub fn result_reason(result: &MessageResult) -> Option<String> {
    match result {
        MessageResult::Reject(reason) => Some(reason.clone()),
        _ => None,
    }
}

/// A work item of the trigger.
pub fn work(
    tenant_id: Uuid,
    chat_id: Uuid,
    base: Option<(OffsetDateTime, Uuid)>,
    target: (OffsetDateTime, Uuid),
) -> ThreadSummaryWork {
    ThreadSummaryWork {
        tenant_id,
        chat_id,
        system_request_id: Uuid::from_u128(0x5151_0000_0000_0000_0000_0000_0000_0001),
        base_frontier: base.map(|(created_at, message_id)| Frontier {
            created_at,
            message_id,
        }),
        frozen_target: Frontier {
            created_at: target.0,
            message_id: target.1,
        },
    }
}

/// A range message.
pub fn range_msg(role: &str, content: &str, micros: i64) -> RangeMessage {
    RangeMessage {
        id: Uuid::from_u128(
            0xA5A5_0000_0000_0000_0000_0000_0000_0000 + u128::try_from(micros).unwrap(),
        ),
        role: role.to_owned(),
        content: content.to_owned(),
        created_at: t(micros),
    }
}

/// `n` alternating user and assistant range messages `m1`, `m2`, ... at 10 µs steps.
pub fn range_of(n: usize) -> Vec<RangeMessage> {
    (1..=n)
        .map(|i| {
            range_msg(
                if i % 2 == 1 { "user" } else { "assistant" },
                &format!("m{i}"),
                i64::try_from(i).unwrap() * 10,
            )
        })
        .collect()
}

// ── provider response fixtures per adapter kind ──────────────────────────────

/// The text of a well-formed model answer.
pub fn summary_text(body: &str) -> String {
    format!("<analysis>thinking about it</analysis>\n<summary>{body}</summary>")
}

/// An OpenAI Responses 2xx body: one `message` item with one `output_text` part.
pub fn responses_answer(text: &str, usage: (u64, u64)) -> Value {
    json!({
        "id": "resp_1", "status": "completed",
        "output": [{ "type": "message", "role": "assistant",
                      "content": [{ "type": "output_text", "text": text, "annotations": [] }] }],
        "usage": { "input_tokens": usage.0, "output_tokens": usage.1 }
    })
}

/// An OpenAI Chat Completions 2xx body.
pub fn chat_answer(text: &str, usage: (u64, u64)) -> Value {
    json!({
        "id": "chatcmpl-1",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": text },
                       "finish_reason": "stop" }],
        "usage": { "prompt_tokens": usage.0, "completion_tokens": usage.1 }
    })
}

/// An Anthropic Messages 2xx body.
pub fn anthropic_answer(text: &str, usage: (u64, u64)) -> Value {
    json!({
        "id": "msg_1", "type": "message", "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": usage.0, "output_tokens": usage.1 }
    })
}

/// The 2xx body of `kind` with `text` and `usage`.
pub fn answer_of(kind: ProviderKind, text: &str, usage: (u64, u64)) -> Value {
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            responses_answer(text, usage)
        }
        ProviderKind::OpenaiChatCompletions => chat_answer(text, usage),
        ProviderKind::AnthropicMessages => anthropic_answer(text, usage),
    }
}

/// An OpenAI-style context-length error body.
pub fn context_length_error() -> Value {
    json!({ "error": { "message": "This model's maximum context length is 8192 tokens.",
                        "type": "invalid_request_error", "code": "context_length_exceeded" } })
}

// ── doubles: usage queue ─────────────────────────────────────────────────────

/// A usage queue that records the events and returns an empty wake, or fails.
#[derive(Default)]
pub struct RecordingUsageQueue {
    events: Mutex<Vec<UsageEvent>>,
    attempted: Mutex<Vec<UsageEvent>>,
    pub fail: AtomicBool,
}

impl RecordingUsageQueue {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn failing() -> Arc<Self> {
        let queue = Self::default();
        queue.fail.store(true, Ordering::SeqCst);
        Arc::new(queue)
    }

    /// The events that were enqueued successfully.
    pub fn events(&self) -> Vec<UsageEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Every event the queue was asked to enqueue, failed attempts included.
    pub fn attempted(&self) -> Vec<UsageEvent> {
        self.attempted.lock().unwrap().clone()
    }
}

#[async_trait]
impl UsageQueue for RecordingUsageQueue {
    async fn enqueue_usage(&self, _tx: &DbTx<'_>, event: &UsageEvent) -> Result<Wake, DomainError> {
        self.attempted.lock().unwrap().push(event.clone());
        if self.fail.load(Ordering::SeqCst) {
            return Err(DomainError::internal("forced usage enqueue failure"));
        }
        self.events.lock().unwrap().push(event.clone());
        Ok(Wake::empty())
    }
}

// ── doubles: summary store ───────────────────────────────────────────────────

/// How the fake store answers a commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    Committed,
    TargetDeleted,
    BaseMissing,
    FrontierMoved,
    DbError,
}

/// An in-memory [`SummaryStore`]: scripted answers and a call log.
pub struct FakeSummaryStore {
    pub precheck: Mutex<Result<PreCheck, ()>>,
    pub range: Mutex<Result<Vec<RangeMessage>, ()>>,
    pub commit: Mutex<CommitKind>,
    calls: Mutex<Vec<&'static str>>,
    commits: Mutex<Vec<SummaryCommit>>,
}

impl FakeSummaryStore {
    /// Ready with no base, a range of four messages, a successful commit.
    pub fn ready() -> Arc<Self> {
        Arc::new(Self {
            precheck: Mutex::new(Ok(PreCheck::Ready { base_summary: None })),
            range: Mutex::new(Ok(range_of(4))),
            commit: Mutex::new(CommitKind::Committed),
            calls: Mutex::new(Vec::new()),
            commits: Mutex::new(Vec::new()),
        })
    }

    pub fn set_precheck(&self, result: Result<PreCheck, ()>) {
        *self.precheck.lock().unwrap() = result;
    }

    pub fn set_range(&self, result: Result<Vec<RangeMessage>, ()>) {
        *self.range.lock().unwrap() = result;
    }

    pub fn set_commit(&self, kind: CommitKind) {
        *self.commit.lock().unwrap() = kind;
    }

    /// `precheck`, `range` and `commit` in call order.
    pub fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    pub fn commits(&self) -> Vec<SummaryCommit> {
        self.commits.lock().unwrap().clone()
    }
}

#[async_trait]
impl SummaryStore for FakeSummaryStore {
    async fn precheck(&self, _work: &ThreadSummaryWork) -> Result<PreCheck, DomainError> {
        self.calls.lock().unwrap().push("precheck");
        self.precheck
            .lock()
            .unwrap()
            .clone()
            .map_err(|()| DomainError::internal("injected pre-check failure"))
    }

    async fn load_range(
        &self,
        _work: &ThreadSummaryWork,
    ) -> Result<Vec<RangeMessage>, DomainError> {
        self.calls.lock().unwrap().push("range");
        self.range
            .lock()
            .unwrap()
            .clone()
            .map_err(|()| DomainError::internal("injected range failure"))
    }

    async fn commit(
        &self,
        commit: &SummaryCommit,
        _usage: Arc<dyn UsageQueue>,
    ) -> Result<CommitOutcome, DomainError> {
        self.calls.lock().unwrap().push("commit");
        self.commits.lock().unwrap().push(commit.clone());
        match *self.commit.lock().unwrap() {
            CommitKind::Committed => Ok(CommitOutcome::Committed(Wake::empty())),
            CommitKind::TargetDeleted => Ok(CommitOutcome::TargetDeleted),
            CommitKind::BaseMissing => Ok(CommitOutcome::BaseMissing),
            CommitKind::FrontierMoved => Ok(CommitOutcome::FrontierMoved),
            CommitKind::DbError => Err(DomainError::internal("injected commit failure")),
        }
    }
}

// ── doubles: summary transport ───────────────────────────────────────────────

/// One scripted answer of [`ScriptedTransport`].
#[derive(Clone)]
pub enum Scripted {
    /// A 2xx or error answer with the body bytes.
    Answer { status: u16, body: Vec<u8> },
    /// A 2xx answer whose body exceeded the read bound.
    TooLarge,
    /// No provider answer.
    Fail(TransportFailure),
    /// The call never answers.
    Hang,
    /// The virtual time advances by the duration, then the inner answer is given.
    Takes(Duration, Box<Scripted>),
}

impl Scripted {
    /// A 200 answer with a JSON body.
    pub fn ok(body: &Value) -> Self {
        Self::Answer {
            status: 200,
            body: body.to_string().into_bytes(),
        }
    }

    /// An error answer with a JSON body.
    pub fn status(status: u16, body: &Value) -> Self {
        Self::Answer {
            status,
            body: body.to_string().into_bytes(),
        }
    }

    pub fn takes(duration: Duration, then: Self) -> Self {
        Self::Takes(duration, Box::new(then))
    }
}

/// One call the scripted transport received.
#[derive(Debug, Clone)]
pub struct TransportCall {
    pub uri: String,
    pub body: Value,
    /// The virtual monotonic time at the start of the call.
    pub at: Duration,
}

/// A [`SummaryTransport`] with queued answers and a call log.
pub struct ScriptedTransport {
    queued: Mutex<VecDeque<Scripted>>,
    default: Mutex<Scripted>,
    calls: Mutex<Vec<TransportCall>>,
    time: Arc<ManualTime>,
}

impl ScriptedTransport {
    /// A transport whose default answer is `default`.
    pub fn answering(time: &Arc<ManualTime>, default: Scripted) -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(VecDeque::new()),
            default: Mutex::new(default),
            calls: Mutex::new(Vec::new()),
            time: Arc::clone(time),
        })
    }

    pub fn queue(&self, answer: Scripted) {
        self.queued.lock().unwrap().push_back(answer);
    }

    pub fn calls(&self) -> Vec<TransportCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl SummaryTransport for ScriptedTransport {
    async fn post_json(&self, uri: &str, body: &Value) -> Result<HttpAnswer, TransportFailure> {
        self.calls.lock().unwrap().push(TransportCall {
            uri: uri.to_owned(),
            body: body.clone(),
            at: self.time.monotonic(),
        });
        let mut answer = self
            .queued
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.default.lock().unwrap().clone());
        loop {
            match answer {
                Scripted::Takes(duration, then) => {
                    self.time.advance(duration);
                    answer = *then;
                }
                Scripted::Answer { status, body } => {
                    return Ok(HttpAnswer {
                        status,
                        body,
                        too_large: false,
                    });
                }
                Scripted::TooLarge => {
                    return Ok(HttpAnswer {
                        status: 200,
                        body: Vec::new(),
                        too_large: true,
                    });
                }
                Scripted::Fail(failure) => return Err(failure),
                Scripted::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never completes")
                }
            }
        }
    }
}

// ── doubles: policy gateway ──────────────────────────────────────────────────

/// How the scripted policy gateway answers `current_snapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMode {
    Serve,
    Fail,
    /// The virtual time advances by the duration, then the snapshot is served.
    Takes(Duration),
    /// The call never answers.
    Hang,
}

/// A [`ModelPolicyGateway`] that serves a snapshot per script and logs the user ids it was
/// asked for and the virtual time of each call.
pub struct ScriptedPolicy {
    snapshot: PolicySnapshot,
    mode: Mutex<PolicyMode>,
    calls: Mutex<Vec<(Uuid, Duration)>>,
    time: Arc<ManualTime>,
}

impl ScriptedPolicy {
    pub fn new(time: &Arc<ManualTime>, snapshot: PolicySnapshot, mode: PolicyMode) -> Arc<Self> {
        Arc::new(Self {
            snapshot,
            mode: Mutex::new(mode),
            calls: Mutex::new(Vec::new()),
            time: Arc::clone(time),
        })
    }

    pub fn set_mode(&self, mode: PolicyMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    /// The user id of each `current_snapshot` call.
    pub fn users(&self) -> Vec<Uuid> {
        self.calls.lock().unwrap().iter().map(|c| c.0).collect()
    }

    /// The virtual time of each call.
    pub fn times(&self) -> Vec<Duration> {
        self.calls.lock().unwrap().iter().map(|c| c.1).collect()
    }
}

#[async_trait]
impl ModelPolicyGateway for ScriptedPolicy {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push((user_id, self.time.monotonic()));
        let mode = *self.mode.lock().unwrap();
        match mode {
            PolicyMode::Serve => Ok(self.snapshot.clone()),
            PolicyMode::Fail => Err(DomainError::PolicyUnavailable {
                cause: "injected policy failure".to_owned(),
            }),
            PolicyMode::Takes(duration) => {
                self.time.advance(duration);
                Ok(self.snapshot.clone())
            }
            PolicyMode::Hang => {
                std::future::pending::<()>().await;
                unreachable!("pending never completes")
            }
        }
    }

    async fn snapshot_of_version(
        &self,
        user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.current_snapshot(user_id).await
    }

    async fn user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        Err(DomainError::internal("the scripted policy has no limits"))
    }
}

// ── doubles: chat-cleanup store ──────────────────────────────────────────────

/// One attachment row of [`FakeChatStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRow {
    pub id: Uuid,
    pub provider_file_id: Option<String>,
    pub storage_backend: String,
    pub status: Option<String>,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub secondary_file_id: Option<String>,
    pub secondary_status: String,
    pub secondary_provider_kind: Option<String>,
}

impl FakeRow {
    /// A `pending` document row with a provider file.
    pub fn pending(n: u128, file: &str) -> Self {
        Self {
            id: Uuid::from_u128(0xFA00_0000_0000_0000_0000_0000_0000_0000 + n),
            provider_file_id: Some(file.to_owned()),
            storage_backend: BACKEND.to_owned(),
            status: Some("pending".to_owned()),
            attempts: 0,
            last_error: None,
            secondary_file_id: None,
            secondary_status: "none".to_owned(),
            secondary_provider_kind: None,
        }
    }

    pub fn with_secondary(mut self, file: &str) -> Self {
        self.secondary_file_id = Some(file.to_owned());
        self.secondary_status = "uploaded".to_owned();
        self.secondary_provider_kind = Some("anthropic".to_owned());
        self
    }

    pub fn with_status(mut self, status: &str) -> Self {
        self.status = Some(status.to_owned());
        self
    }

    pub fn attempts(mut self, attempts: i32) -> Self {
        self.attempts = attempts;
        self
    }

    pub fn no_file(mut self) -> Self {
        self.provider_file_id = None;
        self
    }
}

/// An in-memory [`ChatCleanupStore`] with the guarded-update semantics of the port, a call log
/// and per-method error injection.
pub struct FakeChatStore {
    pub chat: Mutex<Option<ChatFacts>>,
    pub rows: Mutex<Vec<FakeRow>>,
    pub store_row: Mutex<Option<VectorStoreRow>>,
    calls: Mutex<Vec<String>>,
    /// The method names (`load_chat`, `pending_attachments`, `count_pending`, `has_failed`,
    /// `mark_done`, `record_failed_attempt`, `vector_store`, `delete_vector_store_row`) that
    /// answer an error.
    pub failing: Mutex<Vec<&'static str>>,
    /// Rows another path finishes between the read and the guarded write: the guarded write
    /// finds the row already `done` and matches nothing.
    pub lose_guard: Mutex<Vec<Uuid>>,
    /// `(method, n)`: the `n`-th call (1-based) of the method answers an error, once.
    pub fail_nth: Mutex<Vec<(&'static str, usize)>>,
}

impl FakeChatStore {
    /// A soft-deleted chat of `model` with the given rows and no vector store.
    pub fn deleted_chat(model: &str, rows: Vec<FakeRow>) -> Arc<Self> {
        Arc::new(Self {
            chat: Mutex::new(Some(ChatFacts {
                model: model.to_owned(),
                deleted: true,
            })),
            rows: Mutex::new(rows),
            store_row: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            failing: Mutex::new(Vec::new()),
            lose_guard: Mutex::new(Vec::new()),
            fail_nth: Mutex::new(Vec::new()),
        })
    }

    /// The `n`-th call (1-based) of `method` answers an error, once.
    pub fn fail_on_call(self: &Arc<Self>, method: &'static str, n: usize) -> Arc<Self> {
        self.fail_nth.lock().unwrap().push((method, n));
        Arc::clone(self)
    }

    /// The guarded write of `id` loses to another path that marks the row `done` first.
    pub fn lose_guard_for(self: &Arc<Self>, id: Uuid) -> Arc<Self> {
        self.lose_guard.lock().unwrap().push(id);
        Arc::clone(self)
    }

    fn rival_finished(&self, id: Uuid) -> bool {
        if !self.lose_guard.lock().unwrap().contains(&id) {
            return false;
        }
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|row| row.id == id)
        {
            row.status = Some("done".to_owned());
        }
        true
    }

    pub fn with_store(self: &Arc<Self>, vector_store_id: Option<&str>) -> Arc<Self> {
        *self.store_row.lock().unwrap() = Some(VectorStoreRow {
            id: Uuid::from_u128(0x5705_0000_0000_0000_0000_0000_0000_0001),
            vector_store_id: vector_store_id.map(str::to_owned),
            provider: BACKEND.to_owned(),
        });
        Arc::clone(self)
    }

    pub fn fail(self: &Arc<Self>, method: &'static str) -> Arc<Self> {
        self.failing.lock().unwrap().push(method);
        Arc::clone(self)
    }

    pub fn stop_failing(&self) {
        self.failing.lock().unwrap().clear();
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The writes the store saw (`mark_done`, `record_failed_attempt`, `delete_vector_store_row`).
    pub fn writes(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| {
                c.starts_with("mark_done")
                    || c.starts_with("record_failed_attempt")
                    || c.starts_with("delete_vector_store_row")
            })
            .collect()
    }

    pub fn row(&self, id: Uuid) -> FakeRow {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.id == id)
            .cloned()
            .expect("row")
    }

    pub fn statuses(&self) -> Vec<(Option<String>, i32)> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .map(|row| (row.status.clone(), row.attempts))
            .collect()
    }

    pub fn has_store_row(&self) -> bool {
        self.store_row.lock().unwrap().is_some()
    }

    fn enter(&self, method: &'static str, detail: &str) -> Result<(), DomainError> {
        self.calls.lock().unwrap().push(if detail.is_empty() {
            method.to_owned()
        } else {
            format!("{method}:{detail}")
        });
        if self.failing.lock().unwrap().contains(&method) {
            return Err(DomainError::internal(format!("injected {method} failure")));
        }
        let seen = self
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.split(':').next() == Some(method))
            .count();
        if self
            .fail_nth
            .lock()
            .unwrap()
            .iter()
            .any(|(m, n)| *m == method && *n == seen)
        {
            return Err(DomainError::internal(format!("injected {method} failure")));
        }
        Ok(())
    }
}

#[async_trait]
impl ChatCleanupStore for FakeChatStore {
    async fn load_chat(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Option<ChatFacts>, DomainError> {
        self.enter("load_chat", "")?;
        Ok(self.chat.lock().unwrap().clone())
    }

    async fn pending_attachments(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Vec<PendingAttachment>, DomainError> {
        self.enter("pending_attachments", "")?;
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.status.as_deref() == Some("pending"))
            .map(|row| PendingAttachment {
                id: row.id,
                provider_file_id: row.provider_file_id.clone(),
                storage_backend: row.storage_backend.clone(),
                cleanup_attempts: row.attempts,
                secondary_file_id: row.secondary_file_id.clone(),
                secondary_status: row.secondary_status.clone(),
                secondary_provider_kind: row.secondary_provider_kind.clone(),
            })
            .collect())
    }

    async fn count_pending(&self, _tenant_id: Uuid, _chat_id: Uuid) -> Result<u64, DomainError> {
        self.enter("count_pending", "")?;
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.status.as_deref() == Some("pending"))
            .count() as u64)
    }

    async fn has_failed(&self, _tenant_id: Uuid, _chat_id: Uuid) -> Result<bool, DomainError> {
        self.enter("has_failed", "")?;
        Ok(self
            .rows
            .lock()
            .unwrap()
            .iter()
            .any(|row| row.status.as_deref() == Some("failed")))
    }

    async fn mark_done(
        &self,
        _tenant_id: Uuid,
        attachment_id: Uuid,
        _now: OffsetDateTime,
    ) -> Result<bool, DomainError> {
        self.enter("mark_done", &attachment_id.to_string())?;
        if self.rival_finished(attachment_id) {
            return Ok(false);
        }
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.id == attachment_id && row.status.as_deref() == Some("pending"))
        else {
            return Ok(false);
        };
        row.status = Some("done".to_owned());
        Ok(true)
    }

    async fn record_failed_attempt(
        &self,
        _tenant_id: Uuid,
        attachment_id: Uuid,
        cause: &str,
        max_attempts: u32,
        _now: OffsetDateTime,
    ) -> Result<Option<AttemptRecord>, DomainError> {
        self.enter(
            "record_failed_attempt",
            &format!("{attachment_id}:{max_attempts}"),
        )?;
        if self.rival_finished(attachment_id) {
            return Ok(None);
        }
        let mut rows = self.rows.lock().unwrap();
        let Some(row) = rows
            .iter_mut()
            .find(|row| row.id == attachment_id && row.status.as_deref() == Some("pending"))
        else {
            return Ok(None);
        };
        row.attempts += 1;
        row.last_error = Some(cause.to_owned());
        let exhausted = u32::try_from(row.attempts).unwrap_or(0) >= max_attempts;
        if exhausted {
            row.status = Some("failed".to_owned());
        }
        Ok(Some(AttemptRecord {
            attempts: row.attempts,
            exhausted,
        }))
    }

    async fn vector_store(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
    ) -> Result<Option<VectorStoreRow>, DomainError> {
        self.enter("vector_store", "")?;
        Ok(self.store_row.lock().unwrap().clone())
    }

    async fn delete_vector_store_row(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        id: Uuid,
        placeholder_only: bool,
    ) -> Result<bool, DomainError> {
        self.enter(
            "delete_vector_store_row",
            &format!("{id}:placeholder_only={placeholder_only}"),
        )?;
        let mut row = self.store_row.lock().unwrap();
        let matches = row
            .as_ref()
            .is_some_and(|r| r.id == id && (!placeholder_only || r.vector_store_id.is_none()));
        if matches {
            *row = None;
        }
        Ok(matches)
    }
}

// ── doubles: storage provider ────────────────────────────────────────────────

/// The kind of a provider call of the chat-cleanup handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    Primary,
    Secondary,
    VectorStore,
    /// Any call the cleanup handler must never make.
    Other,
}

/// One scripted step of [`ScriptedStorage`].
#[derive(Debug, Clone)]
pub enum Step {
    Ok,
    NotFound,
    Fail(ProviderFailure),
    /// The call never answers.
    Hang,
    /// The virtual time advances by the duration, then the inner step applies.
    Takes(Duration, Box<Step>),
}

impl Step {
    pub fn takes(duration: Duration, then: Self) -> Self {
        Self::Takes(duration, Box::new(then))
    }
}

/// One provider call the scripted storage received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCall {
    pub kind: CallKind,
    pub resource: String,
    pub alias: String,
    pub backend: String,
    /// The virtual monotonic time at the start of the call.
    pub at: Duration,
}

/// A [`StorageProvider`] over scripts per call kind that advances the virtual time and logs
/// every call.
pub struct ScriptedStorage {
    time: Arc<ManualTime>,
    scripts: Mutex<BTreeMap<u8, VecDeque<Step>>>,
    calls: Mutex<Vec<StorageCall>>,
    fail_all: Mutex<BTreeMap<u8, Step>>,
}

fn key(kind: CallKind) -> u8 {
    match kind {
        CallKind::Primary => 0,
        CallKind::Secondary => 1,
        CallKind::VectorStore => 2,
        CallKind::Other => 3,
    }
}

impl ScriptedStorage {
    pub fn new(time: &Arc<ManualTime>) -> Arc<Self> {
        Arc::new(Self {
            time: Arc::clone(time),
            scripts: Mutex::new(BTreeMap::new()),
            calls: Mutex::new(Vec::new()),
            fail_all: Mutex::new(BTreeMap::new()),
        })
    }

    /// The next unanswered call of `kind` takes `step`.
    pub fn queue(&self, kind: CallKind, step: Step) {
        self.scripts
            .lock()
            .unwrap()
            .entry(key(kind))
            .or_default()
            .push_back(step);
    }

    /// Every call of `kind` takes `step` when nothing is queued.
    pub fn always(&self, kind: CallKind, step: Step) {
        self.fail_all.lock().unwrap().insert(key(kind), step);
    }

    pub fn calls(&self) -> Vec<StorageCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn calls_of(&self, kind: CallKind) -> Vec<StorageCall> {
        self.calls()
            .into_iter()
            .filter(|call| call.kind == kind)
            .collect()
    }

    async fn run(
        &self,
        kind: CallKind,
        target: &StorageTarget,
        resource: &str,
    ) -> Result<(), ProviderFailure> {
        self.calls.lock().unwrap().push(StorageCall {
            kind,
            resource: resource.to_owned(),
            alias: target.upstream_alias.clone(),
            backend: target.storage_backend.clone(),
            at: self.time.monotonic(),
        });
        let mut step = self
            .scripts
            .lock()
            .unwrap()
            .get_mut(&key(kind))
            .and_then(VecDeque::pop_front)
            .or_else(|| self.fail_all.lock().unwrap().get(&key(kind)).cloned())
            .unwrap_or(Step::Ok);
        loop {
            match step {
                Step::Ok => return Ok(()),
                Step::NotFound => return Err(ProviderFailure::NotFound),
                Step::Fail(failure) => return Err(failure),
                Step::Takes(duration, then) => {
                    self.time.advance(duration);
                    step = *then;
                }
                Step::Hang => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never completes")
                }
            }
        }
    }
}

#[async_trait]
impl StorageProvider for ScriptedStorage {
    async fn upload_file(
        &self,
        target: &StorageTarget,
        _meta: &FileUploadMeta,
        _body: UploadBody,
    ) -> Result<String, UploadError> {
        self.run(CallKind::Other, target, "upload").await.ok();
        Err(UploadError::Provider)
    }

    async fn delete_file(
        &self,
        target: &StorageTarget,
        provider_file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.run(CallKind::Primary, target, provider_file_id).await
    }

    async fn create_vector_store(&self, target: &StorageTarget) -> Result<String, ProviderFailure> {
        self.run(CallKind::Other, target, "create").await?;
        Err(ProviderFailure::Other)
    }

    async fn delete_vector_store(
        &self,
        target: &StorageTarget,
        vector_store_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.run(CallKind::VectorStore, target, vector_store_id)
            .await
    }

    async fn add_vector_store_file(
        &self,
        target: &StorageTarget,
        _vector_store_id: &str,
        _provider_file_id: &str,
        _attachment_id: Uuid,
    ) -> Result<Option<String>, ProviderFailure> {
        self.run(CallKind::Other, target, "add").await?;
        Ok(None)
    }

    async fn read_vector_store_file(
        &self,
        target: &StorageTarget,
        _vector_store_id: &str,
        _provider_file_id: &str,
        _timeout: Duration,
    ) -> Result<Option<String>, ProviderFailure> {
        self.run(CallKind::Other, target, "read").await?;
        Ok(None)
    }

    async fn upload_secondary_image(
        &self,
        target: &StorageTarget,
        _filename: &str,
        _content_type: &str,
        _bytes: bytes::Bytes,
    ) -> Result<String, ProviderFailure> {
        self.run(CallKind::Other, target, "upload_secondary")
            .await?;
        Err(ProviderFailure::Other)
    }

    async fn delete_secondary_file(
        &self,
        target: &StorageTarget,
        file_id: &str,
    ) -> Result<(), ProviderFailure> {
        self.run(CallKind::Secondary, target, file_id).await
    }
}

// ── worker builders ──────────────────────────────────────────────────────────

/// Everything a summary worker is built from.
pub struct SummaryWiring {
    pub store: Arc<dyn SummaryStore>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub providers: Arc<ProviderRegistry>,
    pub transport: Arc<dyn SummaryTransport>,
    pub usage: Arc<dyn UsageQueue>,
    pub clock: Arc<dyn Clock>,
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn SummaryMetrics>,
    pub settings: SummaryWorkerSettings,
}

/// The summary worker over `wiring`.
pub fn summary_worker_of(wiring: SummaryWiring) -> Arc<ThreadSummaryWorker> {
    Arc::new(ThreadSummaryWorker::new(SummaryDeps {
        store: wiring.store,
        policy: wiring.policy,
        providers: wiring.providers,
        transport: wiring.transport,
        usage: wiring.usage,
        clock: wiring.clock,
        timer: wiring.timer,
        metrics: wiring.metrics,
        settings: wiring.settings,
    }))
}

/// Everything a chat-cleanup worker is built from.
pub struct CleanupWiring {
    pub store: Arc<dyn ChatCleanupStore>,
    pub storage: Arc<dyn StorageProvider>,
    pub secondary_storage: Option<Arc<dyn StorageProvider>>,
    pub providers: Arc<ProviderRegistry>,
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub clock: Arc<dyn Clock>,
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn CleanupMetrics>,
    pub settings: CleanupSettings,
}

/// The chat-cleanup worker over `wiring`.
pub fn cleanup_worker_of(wiring: CleanupWiring) -> Arc<ChatCleanupWorker> {
    Arc::new(ChatCleanupWorker::new(ChatCleanupDeps {
        store: wiring.store,
        storage: wiring.storage,
        secondary_storage: wiring.secondary_storage,
        providers: wiring.providers,
        policy: wiring.policy,
        clock: wiring.clock,
        timer: wiring.timer,
        metrics: wiring.metrics,
        settings: wiring.settings,
    }))
}

/// A summary worker over the fakes: `FakeSummaryStore::ready()`, the scripted transport that
/// answers a well-formed Responses body, the scripted policy over [`catalog`], the registry of
/// [`providers`] and the recording metrics.
pub struct UnitSummary {
    pub worker: Arc<ThreadSummaryWorker>,
    pub store: Arc<FakeSummaryStore>,
    pub transport: Arc<ScriptedTransport>,
    pub policy: Arc<ScriptedPolicy>,
    pub usage: Arc<RecordingUsageQueue>,
    pub metrics: Arc<RecordingMaintenanceMetrics>,
    pub time: Arc<ManualTime>,
}

/// The default answer of the unit transport: a 200 Responses body with a `<summary>`.
pub fn default_answer() -> Scripted {
    Scripted::ok(&responses_answer(&summary_text("the summary"), (100, 40)))
}

pub fn unit_summary(settings: SummaryWorkerSettings) -> UnitSummary {
    unit_summary_with(settings, catalog())
}

pub fn unit_summary_with(settings: SummaryWorkerSettings, snapshot: PolicySnapshot) -> UnitSummary {
    let time = ManualTime::auto(default_now());
    let store = FakeSummaryStore::ready();
    let transport = ScriptedTransport::answering(&time, default_answer());
    let policy = ScriptedPolicy::new(&time, snapshot, PolicyMode::Serve);
    let usage = RecordingUsageQueue::new();
    let metrics = RecordingMaintenanceMetrics::new();
    let worker = summary_worker_of(SummaryWiring {
        store: store.clone(),
        policy: policy.clone(),
        providers: providers(),
        transport: transport.clone(),
        usage: usage.clone(),
        clock: time.clone(),
        timer: time.clone(),
        metrics: metrics.clone(),
        settings,
    });
    UnitSummary {
        worker,
        store,
        transport,
        policy,
        usage,
        metrics,
        time,
    }
}

/// A chat-cleanup worker over the fakes.
pub struct UnitCleanup {
    pub worker: Arc<ChatCleanupWorker>,
    pub store: Arc<FakeChatStore>,
    pub storage: Arc<ScriptedStorage>,
    pub secondary: Arc<ScriptedStorage>,
    pub policy: Arc<ScriptedPolicy>,
    pub metrics: Arc<RecordingMaintenanceMetrics>,
    pub time: Arc<ManualTime>,
}

pub fn unit_cleanup(store: Arc<FakeChatStore>) -> UnitCleanup {
    unit_cleanup_with(store, providers(), 5, true)
}

pub fn unit_cleanup_with(
    store: Arc<FakeChatStore>,
    registry: Arc<ProviderRegistry>,
    max_attempts: u32,
    with_secondary: bool,
) -> UnitCleanup {
    let time = ManualTime::auto(default_now());
    let storage = ScriptedStorage::new(&time);
    let secondary = ScriptedStorage::new(&time);
    let policy = ScriptedPolicy::new(&time, catalog(), PolicyMode::Serve);
    let metrics = RecordingMaintenanceMetrics::new();
    let worker = cleanup_worker_of(CleanupWiring {
        store: store.clone(),
        storage: storage.clone(),
        secondary_storage: with_secondary.then(|| secondary.clone() as Arc<dyn StorageProvider>),
        providers: registry,
        policy: policy.clone(),
        clock: time.clone(),
        timer: time.clone(),
        metrics: metrics.clone(),
        settings: CleanupSettings { max_attempts },
    });
    UnitCleanup {
        worker,
        store,
        storage,
        secondary,
        policy,
        metrics,
        time,
    }
}

// ── the in-process rig ───────────────────────────────────────────────────────

/// How to build a [`MaintRig`].
pub struct MaintOptions {
    pub snapshot: PolicySnapshot,
    pub providers: Arc<ProviderRegistry>,
    /// Record on the real OTLP instruments instead of the recording double.
    pub otlp: bool,
}

impl MaintOptions {
    pub fn new() -> Self {
        Self {
            snapshot: catalog(),
            providers: providers(),
            otlp: false,
        }
    }

    pub fn with_snapshot(mut self, snapshot: PolicySnapshot) -> Self {
        self.snapshot = snapshot;
        self
    }

    pub fn with_providers(mut self, providers: Arc<ProviderRegistry>) -> Self {
        self.providers = providers;
        self
    }

    pub fn otlp(mut self) -> Self {
        self.otlp = true;
        self
    }
}

impl Default for MaintOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The in-process harness of both handlers: the quota rig (database, PEP, outbox pipeline), the
/// mock OAGW of the summary provider and of the storage APIs, the virtual clock and timer and
/// the metrics sinks.
pub struct MaintRig {
    pub quota: QuotaRig,
    pub time: Arc<ManualTime>,
    /// The mock provider that answers the summary calls.
    pub upstream: Arc<MockProvider>,
    /// The mock OAGW of the Files and Vector Stores calls.
    pub storage: Arc<MockStorage>,
    pub recorder: Arc<RecordingMaintenanceMetrics>,
    pub otlp: Option<MetricsHarness>,
    pub providers: Arc<ProviderRegistry>,
}

pub async fn maint_rig() -> MaintRig {
    maint_rig_with(MaintOptions::new()).await
}

pub async fn maint_rig_with(options: MaintOptions) -> MaintRig {
    let time = ManualTime::auto(default_now());
    let gateway = super::quota::QuotaGateway::serving(
        options.snapshot,
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    );
    let quota = quota_rig_with(QuotaRigOptions {
        gateway,
        queue: UsageQueueKind::Real,
        clock: time.clone(),
        ..QuotaRigOptions::new()
    })
    .await;
    MaintRig {
        quota,
        time,
        upstream: MockProvider::new(),
        storage: MockStorage::new(),
        recorder: RecordingMaintenanceMetrics::new(),
        otlp: options.otlp.then(|| metrics_harness("mini_chat")),
        providers: options.providers,
    }
}

impl MaintRig {
    pub fn db(&self) -> &TestDb {
        &self.quota.db
    }

    fn summary_sink(&self) -> Arc<dyn SummaryMetrics> {
        self.otlp.as_ref().map_or_else(
            || self.recorder.clone() as Arc<dyn SummaryMetrics>,
            |harness| harness.metrics.clone() as Arc<dyn SummaryMetrics>,
        )
    }

    fn cleanup_sink(&self) -> Arc<dyn CleanupMetrics> {
        self.otlp.as_ref().map_or_else(
            || self.recorder.clone() as Arc<dyn CleanupMetrics>,
            |harness| harness.metrics.clone() as Arc<dyn CleanupMetrics>,
        )
    }

    /// The summary worker over the real store, the real OAGW summary client over `upstream`, the
    /// real enqueuer and the virtual clock and timer.
    pub fn summary_worker(&self, settings: SummaryWorkerSettings) -> Arc<ThreadSummaryWorker> {
        self.summary_worker_over(self.upstream.clone(), settings)
    }

    pub fn summary_worker_over(
        &self,
        gateway: Arc<dyn ServiceGatewayClientV1>,
        settings: SummaryWorkerSettings,
    ) -> Arc<ThreadSummaryWorker> {
        summary_worker_of(SummaryWiring {
            store: self.quota.db.provider.clone(),
            policy: self.quota.gateway.clone(),
            providers: self.providers.clone(),
            transport: Arc::new(OagwSummaryClient::new(
                gateway,
                S2sContext::bound(provider_s2s()),
            )),
            usage: self.quota.enqueuer.clone(),
            clock: self.time.clone(),
            timer: self.time.clone(),
            metrics: self.summary_sink(),
            settings,
        })
    }

    pub fn summary_handler(&self, settings: SummaryWorkerSettings) -> ThreadSummaryHandler {
        ThreadSummaryHandler::new(self.summary_worker(settings))
    }

    /// The chat-cleanup worker over the real store and the real storage client over `storage`.
    pub fn cleanup_worker(&self, max_attempts: u32, secondary: bool) -> Arc<ChatCleanupWorker> {
        self.cleanup_worker_with(self.quota.gateway.clone(), max_attempts, secondary)
    }

    pub fn cleanup_worker_with(
        &self,
        policy: Arc<dyn ModelPolicyGateway>,
        max_attempts: u32,
        secondary: bool,
    ) -> Arc<ChatCleanupWorker> {
        let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
            self.storage.clone(),
            S2sContext::bound(provider_s2s()),
        ));
        cleanup_worker_of(CleanupWiring {
            store: self.quota.db.provider.clone(),
            storage: Arc::clone(&client),
            secondary_storage: secondary.then(|| Arc::clone(&client)),
            providers: self.providers.clone(),
            policy,
            clock: self.time.clone(),
            timer: self.time.clone(),
            metrics: self.cleanup_sink(),
            settings: CleanupSettings { max_attempts },
        })
    }

    pub fn cleanup_handler(&self, max_attempts: u32, secondary: bool) -> ChatCleanupHandler {
        ChatCleanupHandler::new(self.cleanup_worker(max_attempts, secondary))
    }

    /// A live chat of user 1 of tenant A on `model`.
    pub async fn chat(&self, model: &str) -> ChatRecord {
        seed_chat(
            self.db(),
            &a1(),
            Uuid::new_v4(),
            Some("maintenance"),
            model,
            t(0),
        )
        .await
    }

    /// The rows of the outbox queue `queue`.
    pub async fn rows_of(&self, queue: &str) -> Vec<OutboxRow> {
        outbox_rows(self.db())
            .await
            .into_iter()
            .filter(|row| row.queue == queue)
            .collect()
    }

    /// The usage events enqueued so far.
    pub async fn usage_rows(&self) -> Vec<OutboxRow> {
        self.rows_of(&self.quota.outbox.queue_name.clone()).await
    }
}

/// Seed `n` alternating user and assistant messages `m1`, `m2`, ... at `t(10)`, `t(20)`, ... and
/// return their ids in order.
pub async fn seed_range(db: &TestDb, chat: &ChatRecord, n: usize) -> Vec<Uuid> {
    let mut ids = Vec::new();
    for i in 1..=n {
        let role = if i % 2 == 1 { "user" } else { "assistant" };
        ids.push(
            seed_msg(
                db,
                chat,
                MessageSeed::new(role, &format!("m{i}"), t(i64::try_from(i).unwrap() * 10)),
            )
            .await,
        );
    }
    ids
}

/// Seed a vector store row of the chat on the `azure_openai` backend.
pub async fn seed_store(db: &TestDb, chat: &ChatRecord, id: Option<&str>) -> Uuid {
    seed_vector_store(db, chat, id, BACKEND, t(5)).await
}

// ── seeding and inspection helpers of the repository tests ───────────────────

/// The system-task usage event of the summary of `work`.
pub fn usage_event_for(
    work: &ThreadSummaryWork,
    model: &str,
    usage: (u64, u64),
    now: OffsetDateTime,
) -> UsageEvent {
    crate::domain::quota::usage_event::build_system_task_usage_event(
        &crate::domain::quota::usage_event::SystemTaskUsageInput {
            tenant_id: work.tenant_id,
            chat_id: work.chat_id,
            summary_model: model,
            usage: mini_chat_sdk::UsageTokens {
                input_tokens: usage.0,
                output_tokens: usage.1,
                ..mini_chat_sdk::UsageTokens::default()
            },
            system_request_id: work.system_request_id,
            timestamp: now,
        },
    )
}

/// A commit of `work` with the given text and estimate.
pub fn commit_of(
    work: &ThreadSummaryWork,
    text: &str,
    token_estimate: i32,
    now: OffsetDateTime,
) -> SummaryCommit {
    SummaryCommit {
        work: work.clone(),
        summary_text: text.to_owned(),
        token_estimate,
        usage_event: usage_event_for(work, SUMMARY_MODEL, (100, 40), now),
        now,
    }
}

/// The ids of the chat's `is_compressed` messages (deleted ones included) in chronological order.
pub async fn compressed_ids(db: &TestDb, chat_id: Uuid) -> Vec<Uuid> {
    let mut rows: Vec<_> = super::chats::message_rows(db, chat_id)
        .await
        .into_iter()
        .filter(|row| row.is_compressed)
        .collect();
    rows.sort_by_key(|row| (row.created_at, row.id));
    rows.into_iter().map(|row| row.id).collect()
}

/// The number of rows of `table`.
pub async fn table_count(db: &TestDb, table: &str) -> i64 {
    super::raw_i64(&db.raw, &format!("SELECT COUNT(*) FROM {table}")).await
}

/// The ids of a range load.
pub fn ids_of(messages: &[RangeMessage]) -> Vec<Uuid> {
    messages.iter().map(|m| m.id).collect()
}

/// A deterministic message id.
pub fn mid(n: u128) -> Uuid {
    Uuid::from_u128(0x3E55_0000_0000_0000_0000_0000_0000_0000 + n)
}

/// A chat-cleanup worker over the database, the mock storage gateway and the virtual time of an
/// attachments rig (the rig of FEATURE 0005), with the recording metrics.
pub fn cleanup_worker_over_attach_rig(
    rig: &super::attachments::AttachRig,
    max_attempts: u32,
) -> (Arc<ChatCleanupWorker>, Arc<RecordingMaintenanceMetrics>) {
    let metrics = RecordingMaintenanceMetrics::new();
    let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
        rig.storage.clone(),
        S2sContext::bound(provider_s2s()),
    ));
    let worker = cleanup_worker_of(CleanupWiring {
        store: rig.quota.db.provider.clone(),
        storage: Arc::clone(&client),
        secondary_storage: Some(client),
        providers: super::attachments::providers(),
        policy: rig.quota.gateway.clone(),
        clock: rig.time.clone(),
        timer: rig.time.clone(),
        metrics: metrics.clone(),
        settings: CleanupSettings { max_attempts },
    });
    (worker, metrics)
}

// ── a gateway whose calls take virtual time ──────────────────────────────────

/// A [`ServiceGatewayClientV1`] over the mock storage gateway whose `proxy_request` first advances
/// the virtual time by the next scripted delay (the default delay when none is queued): a scripted
/// slow provider that answers inside its call deadline.
pub struct SlowGateway {
    inner: Arc<MockStorage>,
    time: Arc<ManualTime>,
    delays: Mutex<VecDeque<Duration>>,
    default_delay: Mutex<Duration>,
}

impl SlowGateway {
    pub fn new(inner: &Arc<MockStorage>, time: &Arc<ManualTime>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(inner),
            time: Arc::clone(time),
            delays: Mutex::new(VecDeque::new()),
            default_delay: Mutex::new(Duration::ZERO),
        })
    }

    /// Every call without a queued delay takes `delay`.
    pub fn always(&self, delay: Duration) {
        *self.default_delay.lock().unwrap() = delay;
    }
}

#[async_trait]
impl ServiceGatewayClientV1 for SlowGateway {
    async fn create_upstream(
        &self,
        ctx: toolkit_security::SecurityContext,
        req: oagw_sdk::CreateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.create_upstream(ctx, req).await
    }

    async fn get_upstream(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<oagw_sdk::Upstream, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.get_upstream(ctx, id).await
    }

    async fn list_upstreams(
        &self,
        ctx: toolkit_security::SecurityContext,
        query: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Upstream>, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.list_upstreams(ctx, query).await
    }

    async fn update_upstream(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
        req: oagw_sdk::UpdateUpstreamRequest,
    ) -> Result<oagw_sdk::Upstream, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.update_upstream(ctx, id, req).await
    }

    async fn delete_upstream(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<(), toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.delete_upstream(ctx, id).await
    }

    async fn create_route(
        &self,
        ctx: toolkit_security::SecurityContext,
        req: oagw_sdk::CreateRouteRequest,
    ) -> Result<oagw_sdk::Route, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.create_route(ctx, req).await
    }

    async fn get_route(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<oagw_sdk::Route, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.get_route(ctx, id).await
    }

    async fn list_routes(
        &self,
        ctx: toolkit_security::SecurityContext,
        upstream_id: Option<Uuid>,
        query: &oagw_sdk::ListQuery,
    ) -> Result<Vec<oagw_sdk::Route>, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.list_routes(ctx, upstream_id, query).await
    }

    async fn update_route(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
        req: oagw_sdk::UpdateRouteRequest,
    ) -> Result<oagw_sdk::Route, toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.update_route(ctx, id, req).await
    }

    async fn delete_route(
        &self,
        ctx: toolkit_security::SecurityContext,
        id: Uuid,
    ) -> Result<(), toolkit::api::canonical_prelude::CanonicalError> {
        self.inner.delete_route(ctx, id).await
    }

    async fn resolve_proxy_target(
        &self,
        ctx: toolkit_security::SecurityContext,
        alias: &str,
        method: &str,
        path: &str,
    ) -> Result<
        (oagw_sdk::Upstream, oagw_sdk::Route),
        toolkit::api::canonical_prelude::CanonicalError,
    > {
        self.inner
            .resolve_proxy_target(ctx, alias, method, path)
            .await
    }

    async fn proxy_request(
        &self,
        ctx: toolkit_security::SecurityContext,
        req: http::Request<oagw_sdk::Body>,
    ) -> Result<http::Response<oagw_sdk::Body>, toolkit::api::canonical_prelude::CanonicalError>
    {
        let delay = self
            .delays
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| *self.default_delay.lock().unwrap());
        self.time.advance(delay);
        self.inner.proxy_request(ctx, req).await
    }
}

impl MaintRig {
    /// The chat-cleanup worker over an arbitrary storage gateway (a [`SlowGateway`] for a
    /// scripted slow provider), the policy gateway of the rig and the virtual time.
    pub fn cleanup_worker_over(
        &self,
        gateway: Arc<dyn ServiceGatewayClientV1>,
        policy: Arc<dyn ModelPolicyGateway>,
        max_attempts: u32,
        secondary: bool,
    ) -> Arc<ChatCleanupWorker> {
        let client: Arc<dyn StorageProvider> = Arc::new(OagwStorageClient::new(
            gateway,
            S2sContext::bound(provider_s2s()),
        ));
        cleanup_worker_of(CleanupWiring {
            store: self.quota.db.provider.clone(),
            storage: Arc::clone(&client),
            secondary_storage: secondary.then(|| Arc::clone(&client)),
            providers: self.providers.clone(),
            policy,
            clock: self.time.clone(),
            timer: self.time.clone(),
            metrics: self.cleanup_sink(),
            settings: CleanupSettings { max_attempts },
        })
    }
}

/// The names of the tables of the database.
pub async fn table_names(db: &TestDb) -> Vec<String> {
    use sea_orm::ConnectionTrait;
    let rows = db
        .raw
        .query_all_raw(sea_orm::Statement::from_string(
            db.raw.get_database_backend(),
            "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
        ))
        .await
        .expect("table names");
    rows.into_iter()
        .map(|row| row.try_get_by_index::<String>(0).expect("name"))
        .collect()
}
