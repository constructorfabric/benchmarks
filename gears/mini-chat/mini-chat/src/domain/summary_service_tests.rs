#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatModelPolicyPluginError, ModelCatalogEntry, UsageTokens};
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;
use tokio_util::sync::CancellationToken;
use toolkit_security::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use super::*;
use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, ProviderKind, ThreadSummaryWorkerConfig};
use crate::domain::error::DomainError;
use crate::domain::llm::{
    LlmCompletion, LlmEventStream, LlmFailure, LlmInputItem, LlmProvider, LlmProviderResolver,
    LlmRequest, LlmRequestType, LlmUsage, ProviderTarget,
};
use crate::domain::models_service::tests::{FakePolicy, catalog_entry};
use crate::domain::ports::{
    ContextMessageRecord, MessageKey, MessageRole, ThreadSummaryCommit, ThreadSummaryCommitOutcome,
    ThreadSummaryRecord, ThreadSummaryStore, ThreadSummaryTask,
};

// ── Fakes ────────────────────────────────────────────────────────────────

const TENANT: Uuid = Uuid::from_u128(0xa000_0000_0000_4000_8000_0000_0000_000a);
const CHAT: Uuid = Uuid::from_u128(0xc000_0000_0000_4000_8000_0000_0000_000c);
const SYSTEM_REQUEST: Uuid = Uuid::from_u128(0x5000_0000_0000_4000_8000_0000_0000_0005);
const SUMMARY_MODEL: &str = "gpt-4.1-mini";

#[derive(Default)]
struct StoreState {
    current: Option<ThreadSummaryRecord>,
    range: Vec<ContextMessageRecord>,
    range_queries: Vec<(Option<MessageKey>, MessageKey)>,
    commit_outcome: Option<ThreadSummaryCommitOutcome>,
    commits: Vec<ThreadSummaryCommit>,
    fail_reads: bool,
    fail_commit: bool,
}

#[derive(Default)]
struct FakeStore {
    state: Mutex<StoreState>,
}

impl FakeStore {
    fn with(range: Vec<ContextMessageRecord>, current: Option<ThreadSummaryRecord>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(StoreState {
                current,
                range,
                ..StoreState::default()
            }),
        })
    }

    fn commits(&self) -> Vec<ThreadSummaryCommit> {
        self.state.lock().unwrap().commits.clone()
    }
}

#[async_trait]
impl ThreadSummaryStore for FakeStore {
    async fn current_summary(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError> {
        let state = self.state.lock().unwrap();
        if state.fail_reads {
            return Err(DomainError::internal("db down"));
        }
        Ok(state.current.clone())
    }

    async fn summary_range(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
        base: Option<MessageKey>,
        target: MessageKey,
    ) -> Result<Vec<ContextMessageRecord>, DomainError> {
        let mut state = self.state.lock().unwrap();
        state.range_queries.push((base, target));
        Ok(state.range.clone())
    }

    async fn commit_summary(
        &self,
        _scope: &AccessScope,
        commit: ThreadSummaryCommit,
    ) -> Result<ThreadSummaryCommitOutcome, DomainError> {
        let mut state = self.state.lock().unwrap();
        if state.fail_commit {
            return Err(DomainError::internal("commit failed"));
        }
        state.commits.push(commit);
        Ok(state
            .commit_outcome
            .unwrap_or(ThreadSummaryCommitOutcome::Committed))
    }
}

struct FakeResolver;

impl LlmProviderResolver for FakeResolver {
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError> {
        if provider_id != "openai" {
            return Err(DomainError::internal("unknown provider"));
        }
        Ok(ProviderTarget {
            provider_id: provider_id.to_owned(),
            kind: ProviderKind::OpenaiResponses,
            alias: "api.openai.com".to_owned(),
            api_path: "/v1/responses".to_owned(),
            tenant_id,
            tenant_override: false,
        })
    }
}

#[derive(Default)]
struct FakeLlm {
    script: Mutex<VecDeque<Result<LlmCompletion, LlmFailure>>>,
    requests: Mutex<Vec<(ProviderTarget, LlmRequest)>>,
}

impl FakeLlm {
    fn scripted(script: Vec<Result<LlmCompletion, LlmFailure>>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<(ProviderTarget, LlmRequest)> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for FakeLlm {
    async fn stream(
        &self,
        _target: &ProviderTarget,
        _req: LlmRequest,
        _cancel: CancellationToken,
    ) -> Result<LlmEventStream, LlmFailure> {
        Err(LlmFailure::provider_error("streaming is not used"))
    }

    async fn complete(
        &self,
        target: &ProviderTarget,
        req: LlmRequest,
    ) -> Result<LlmCompletion, LlmFailure> {
        self.requests.lock().unwrap().push((target.clone(), req));
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(LlmFailure::provider_error("not scripted")))
    }
}

#[allow(clippy::unnecessary_wraps, reason = "script entries are results")]
fn answer(text: &str, usage: Option<LlmUsage>) -> Result<LlmCompletion, LlmFailure> {
    Ok(LlmCompletion {
        text: text.to_owned(),
        usage,
        response_id: Some("resp_1".to_owned()),
        incomplete_reason: None,
    })
}

fn usage(output: i64, reasoning: i64) -> LlmUsage {
    LlmUsage {
        input_tokens: 900,
        output_tokens: output,
        cache_read_input_tokens: 300,
        cache_write_input_tokens: 0,
        reasoning_tokens: reasoning,
    }
}

fn summary_model(extra: serde_json::Value) -> ModelCatalogEntry {
    let mut entry = catalog_entry(SUMMARY_MODEL, extra);
    entry.thread_summary_prompt = "Catalog summary prompt.".to_owned();
    entry
}

fn key(seconds: i64, id: u128) -> MessageKey {
    MessageKey {
        created_at: datetime!(2026-01-01 00:00:00 UTC) + time::Duration::seconds(seconds),
        id: Uuid::from_u128(id),
    }
}

fn message(role: MessageRole, content: &str, seconds: i64) -> ContextMessageRecord {
    let k = key(seconds, u128::try_from(seconds).unwrap() + 1);
    ContextMessageRecord {
        id: k.id,
        role,
        content: content.to_owned(),
        created_at: k.created_at,
    }
}

fn conversation() -> Vec<ContextMessageRecord> {
    vec![
        message(MessageRole::User, "Hello there", 1),
        message(MessageRole::Assistant, "Hi, how can I help?", 2),
        message(MessageRole::User, "Tell me about Rust", 3),
        message(MessageRole::Assistant, "Rust is a systems language.", 4),
    ]
}

fn task(base: Option<MessageKey>) -> ThreadSummaryTask {
    ThreadSummaryTask {
        tenant_id: TENANT,
        chat_id: CHAT,
        system_request_id: SYSTEM_REQUEST,
        base_frontier: base,
        frozen_target: key(4, 5),
    }
}

fn settings() -> SummarySettings {
    SummarySettings {
        model_id: SUMMARY_MODEL.to_owned(),
        system_prompt: "Configured summary prompt.".to_owned(),
        message_content_limit: 4000,
        max_attempts: 3,
    }
}

fn service(
    store: Arc<FakeStore>,
    catalog: Vec<ModelCatalogEntry>,
    llm: Arc<FakeLlm>,
    settings: SummarySettings,
) -> ThreadSummaryService {
    ThreadSummaryService::new(
        store,
        FakePolicy::new(catalog),
        Arc::new(FakeResolver),
        llm,
        settings,
    )
}

const RAW: &str = "<analysis>Reasoning\nover lines.</analysis>\n<summary>1. Purpose: chat.\n\n\n\n2. Info: Rust.</summary>";
const STORED: &str = "1. Purpose: chat.\n\n2. Info: Rust.";

// ── Prompt ───────────────────────────────────────────────────────────────

#[test]
fn first_summary_prompt_is_exact() {
    let prompt = build_user_prompt(None, &conversation(), 0);
    let expected = format!(
        "Summarize the following conversation:\n\nUser: Hello there\n\nAssistant: Hi, how can I help?\n\nUser: Tell me about Rust\n\nAssistant: Rust is a systems language.\n\n{ANALYSIS_INSTRUCTION}"
    );
    assert_eq!(prompt, expected);
    assert!(prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."));
}

#[test]
fn merge_prompt_carries_the_existing_summary_block() {
    let messages = conversation();
    let prompt = build_user_prompt(Some("Old summary."), &messages[2..], 0);
    let expected = format!(
        "{MERGE_OPENING}\n\n<existing_summary>\nOld summary.\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: Tell me about Rust\n\nAssistant: Rust is a systems language.\n\n{ANALYSIS_INSTRUCTION}"
    );
    assert_eq!(prompt, expected);
    assert!(prompt.starts_with(
        "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise."
    ));
}

#[test]
fn analysis_instruction_matches_b55() {
    let lines: Vec<&str> = ANALYSIS_INSTRUCTION.lines().map(str::trim).collect();
    for expected in [
        "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:",
        "1. Chronologically review each exchange, identifying:",
        "- The user's requests and questions",
        "- Key decisions, answers, and information shared",
        "- Any follow-up actions or commitments",
        "- Specific names, dates, numbers, URLs, or references mentioned",
        "2. Verify accuracy and completeness.",
        "Your summary MUST include these sections:",
        "1. Conversation Purpose: The user's primary goals and recurring themes",
        "4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit",
        "5. Current Topic: What was being discussed most recently, with enough detail to continue naturally",
        "Respond with an <analysis> block followed by a <summary> block.",
    ] {
        assert!(lines.contains(&expected), "missing line {expected:?}");
    }
}

#[test]
fn system_messages_are_not_summarized() {
    let mut messages = conversation();
    messages.insert(1, message(MessageRole::System, "internal", 10));
    assert!(!prompt_entries(&messages, 0).contains("internal"));
}

#[test]
fn content_is_clipped_to_the_limit_in_characters() {
    assert_eq!(clip_content("abcdef", 4), "abcd...");
    assert_eq!(clip_content("abcd", 4), "abcd");
    assert_eq!(clip_content("abcdef", 0), "abcdef");
    // Characters, not bytes.
    assert_eq!(
        clip_content("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 3),
        "\u{e9}\u{e9}\u{e9}..."
    );
}

#[test]
fn system_prompt_falls_back_from_catalog_to_config_to_builtin() {
    assert_eq!(select_system_prompt("catalog", "config"), "catalog");
    assert_eq!(select_system_prompt("", "config"), "config");
    assert_eq!(select_system_prompt("", ""), DEFAULT_SUMMARY_SYSTEM_PROMPT);
}

#[test]
fn input_budget_uses_window_minus_output_capped_by_max_input() {
    let model = catalog_entry(
        "m",
        json!({"context_window": 10_000, "max_output_tokens": 2_000}),
    );
    assert_eq!(input_budget(&model), Some(8_000));
    let capped = catalog_entry(
        "m",
        json!({"context_window": 10_000, "max_output_tokens": 2_000, "max_input_tokens": 5_000}),
    );
    assert_eq!(input_budget(&capped), Some(5_000));
    let unbounded = catalog_entry("m", json!({"context_window": 0}));
    assert_eq!(input_budget(&unbounded), None);
}

#[test]
fn drop_step_is_a_fifth_rounded_up_keeping_two() {
    assert_eq!(drop_step(10), 2);
    assert_eq!(drop_step(11), 3);
    assert_eq!(drop_step(3), 1);
    assert_eq!(drop_step(2), 0);
    assert_eq!(drop_step(1), 0);
}

#[test]
fn fitting_drops_oldest_messages_until_the_prompt_fits() {
    let messages: Vec<_> = (0..10)
        .map(|i| message(MessageRole::User, &"x".repeat(400), i))
        .collect();
    // No budget: everything is kept.
    assert_eq!(fit_start("sys", None, &messages, 0, None, 4), 0);
    // Each entry is ~102 tokens; the fixed text is ~250 tokens.
    let start = fit_start("sys", None, &messages, 0, Some(700), 4);
    assert!(start > 0);
    let prompt = build_user_prompt(None, &messages[start..], 0);
    assert!(estimate_prompt_tokens("sys", &prompt, 4) <= 700);
    // Never fewer than two messages.
    assert_eq!(fit_start("sys", None, &messages, 0, Some(1), 4), 8);
}

// ── Parsing ──────────────────────────────────────────────────────────────

#[test]
fn parse_keeps_summary_content_and_collapses_blank_lines() {
    assert_eq!(parse_summary(RAW), STORED);
}

#[test]
fn parse_without_summary_block_keeps_remaining_text() {
    assert_eq!(
        parse_summary("<analysis>a</analysis>\n\nPlain summary.\n\n\n\nMore."),
        "Plain summary.\n\nMore."
    );
    assert_eq!(parse_summary("Just text."), "Just text.");
}

#[test]
fn parse_with_leftover_markup_is_empty() {
    assert_eq!(
        parse_summary("<analysis>Nothing worth keeping.</analysis>"),
        ""
    );
    assert_eq!(parse_summary("<analysis>unterminated reasoning"), "");
    assert_eq!(parse_summary("<summary>unterminated"), "");
    assert_eq!(parse_summary("   \n\n  "), "");
}

#[test]
fn token_estimate_prefers_output_minus_reasoning() {
    assert_eq!(token_estimate(Some(&usage(85, 5)), STORED), 80);
    let text = "Merged summary: the earlier long texts and the third long question.";
    let fallback = i32::try_from(text.len().div_ceil(4)).unwrap();
    assert_eq!(token_estimate(Some(&usage(40, 40)), text), fallback);
    assert_eq!(token_estimate(None, text), fallback);
}

#[test]
fn context_length_failures_are_detected() {
    let mut failure = LlmFailure::provider_error("too big");
    failure.provider_code = Some("context_length_exceeded".to_owned());
    assert!(is_context_length_exceeded(&failure));
    assert!(is_context_length_exceeded(&LlmFailure::provider_error(
        "This model's maximum context length is 8192 tokens"
    )));
    assert!(!is_context_length_exceeded(&LlmFailure::provider_error(
        "Provider returned HTTP 500"
    )));
}

// ── Usage event ──────────────────────────────────────────────────────────

#[test]
fn system_usage_event_follows_the_attribution_rules() {
    let at = OffsetDateTime::UNIX_EPOCH;
    let event = system_usage_event(&task(None), SUMMARY_MODEL, Some(&usage(85, 5)), at);
    assert_eq!(event.tenant_id, TENANT);
    assert_eq!(event.user_id, None);
    assert_eq!(event.turn_id, None);
    assert_eq!(event.chat_id, Some(CHAT));
    assert_eq!(event.request_id, SYSTEM_REQUEST);
    assert_eq!(event.effective_model, SUMMARY_MODEL);
    assert_eq!(event.selected_model, SUMMARY_MODEL);
    assert_eq!(event.terminal_state, "completed");
    assert_eq!(event.billing_outcome, "system_task");
    assert_eq!(event.settlement_method, "none");
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.requester_type, "system");
    assert_eq!(
        event.system_task_type.as_deref(),
        Some("thread_summary_update")
    );
    assert_eq!(
        event.dedupe_key,
        format!(
            "{}/thread_summary_update/{}",
            TENANT.simple(),
            SYSTEM_REQUEST.simple()
        )
    );
    assert_eq!(
        event.usage,
        Some(UsageTokens {
            input_tokens: 900,
            output_tokens: 85,
            cache_read_input_tokens: 300,
            cache_write_input_tokens: 0,
            reasoning_tokens: 5,
        })
    );
    let wire = serde_json::to_value(&event).unwrap();
    assert!(wire.get("user_id").is_none());
    assert!(wire.get("turn_id").is_none());
}

// ── Disposition ──────────────────────────────────────────────────────────

#[test]
fn retryable_outcomes_are_rejected_on_the_last_attempt() {
    for outcome in [
        SummaryOutcome::ProviderError("500".to_owned()),
        SummaryOutcome::EmptySummary,
        SummaryOutcome::Retry("db".to_owned()),
    ] {
        assert_eq!(disposition(&outcome, 1, 3), Disposition::Retry);
        assert_eq!(disposition(&outcome, 2, 3), Disposition::Retry);
        assert!(matches!(
            disposition(&outcome, 3, 3),
            Disposition::Reject(_)
        ));
    }
}

#[test]
fn finished_outcomes_are_ok_and_model_unavailable_is_rejected() {
    for outcome in [
        SummaryOutcome::Committed { token_estimate: 1 },
        SummaryOutcome::CasConflict,
        SummaryOutcome::BaseMissing,
        SummaryOutcome::FrontierDeleted,
        SummaryOutcome::NothingToSummarize,
    ] {
        assert_eq!(disposition(&outcome, 3, 3), Disposition::Ok);
    }
    assert!(matches!(
        disposition(&SummaryOutcome::ModelUnavailable("x".to_owned()), 1, 3),
        Disposition::Reject(_)
    ));
}

#[test]
fn metric_results_use_the_bounded_allowlist() {
    assert_eq!(
        SummaryOutcome::Committed { token_estimate: 1 }.metric_result(),
        Some("success")
    );
    assert_eq!(
        SummaryOutcome::FrontierDeleted.metric_result(),
        Some("frontier_deleted")
    );
    assert_eq!(
        SummaryOutcome::BaseMissing.metric_result(),
        Some("base_missing")
    );
    assert_eq!(SummaryOutcome::CasConflict.metric_result(), None);
}

// ── Execution ────────────────────────────────────────────────────────────

#[tokio::test]
async fn first_summary_is_requested_and_committed() {
    let store = FakeStore::with(conversation(), None);
    let llm = FakeLlm::scripted(vec![answer(RAW, Some(usage(85, 5)))]);
    let model = summary_model(json!({"max_output_tokens": 16_384}));
    let svc = service(
        Arc::clone(&store),
        vec![model.clone()],
        Arc::clone(&llm),
        settings(),
    );

    let outcome = svc.execute(&task(None)).await;

    assert_eq!(outcome, SummaryOutcome::Committed { token_estimate: 80 });
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    let (target, req) = &requests[0];
    assert_eq!(target.tenant_id, TENANT);
    assert_eq!(req.model, model.provider_model_id);
    assert_eq!(req.instructions, "Catalog summary prompt.");
    assert_eq!(req.max_output_tokens, 16_384);
    assert!(req.tools.is_empty());
    assert_eq!(
        req.user,
        format!("{}{}", TENANT.simple(), DEFAULT_SUBJECT_ID.simple())
    );
    assert_eq!(req.metadata.tenant_id, TENANT.to_string());
    assert_eq!(req.metadata.user_id, DEFAULT_SUBJECT_ID.to_string());
    assert_eq!(req.metadata.chat_id, CHAT.to_string());
    assert_eq!(req.metadata.request_type, LlmRequestType::Summary);
    assert_eq!(req.metadata.feature, "none");
    let [LlmInputItem::User { text, .. }] = req.input.as_slice() else {
        panic!("one user prompt expected: {:?}", req.input);
    };
    assert_eq!(text, &build_user_prompt(None, &conversation(), 4000));

    let commits = store.commits();
    assert_eq!(commits.len(), 1);
    let commit = &commits[0];
    assert_eq!(commit.summary_text, STORED);
    assert_eq!(commit.token_estimate, 80);
    assert_eq!(commit.base_frontier, None);
    assert_eq!(commit.target, key(4, 5));
    assert_eq!(commit.usage_event.request_id, SYSTEM_REQUEST);
    assert_eq!(commit.usage_event.billing_outcome, "system_task");
    let queries = store.state.lock().unwrap().range_queries.clone();
    assert_eq!(queries, vec![(None, key(4, 5))]);
}

#[tokio::test]
async fn configured_prompt_is_used_when_the_catalog_prompt_is_empty() {
    let store = FakeStore::with(conversation(), None);
    let llm = FakeLlm::scripted(vec![answer(RAW, None)]);
    let mut model = summary_model(json!({}));
    model.thread_summary_prompt = String::new();
    let svc = service(store, vec![model], Arc::clone(&llm), settings());

    svc.execute(&task(None)).await;

    assert_eq!(
        llm.requests()[0].1.instructions,
        "Configured summary prompt."
    );
}

#[tokio::test]
async fn resummarize_merges_the_existing_summary() {
    let base = key(2, 3);
    let current = ThreadSummaryRecord {
        text: "Old summary.".to_owned(),
        frontier: base,
        token_estimate: 10,
    };
    let range = conversation()[2..].to_vec();
    let store = FakeStore::with(range.clone(), Some(current));
    let llm = FakeLlm::scripted(vec![answer(
        "<summary>Merged.</summary>",
        Some(usage(40, 40)),
    )]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        Arc::clone(&llm),
        settings(),
    );

    let outcome = svc.execute(&task(Some(base))).await;

    assert_eq!(outcome, SummaryOutcome::Committed { token_estimate: 2 });
    let LlmInputItem::User { text, .. } = &llm.requests()[0].1.input[0] else {
        panic!("user prompt expected");
    };
    assert_eq!(text, &build_user_prompt(Some("Old summary."), &range, 4000));
    assert_eq!(store.commits()[0].base_frontier, Some(base));
}

#[tokio::test]
async fn stored_frontier_ahead_of_the_base_is_a_cas_conflict_without_a_call() {
    let current = ThreadSummaryRecord {
        text: "Newer.".to_owned(),
        frontier: key(9, 9),
        token_estimate: 1,
    };
    let store = FakeStore::with(conversation(), Some(current.clone()));
    let llm = FakeLlm::scripted(vec![]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        Arc::clone(&llm),
        settings(),
    );

    assert_eq!(
        svc.execute(&task(Some(key(2, 3)))).await,
        SummaryOutcome::CasConflict
    );
    // A first-summary task whose chat got a summary meanwhile.
    assert_eq!(svc.execute(&task(None)).await, SummaryOutcome::CasConflict);
    assert!(llm.requests().is_empty());
    assert!(store.commits().is_empty());
}

#[tokio::test]
async fn missing_base_summary_finishes_without_a_call() {
    let store = FakeStore::with(conversation(), None);
    let llm = FakeLlm::scripted(vec![]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        Arc::clone(&llm),
        settings(),
    );

    assert_eq!(
        svc.execute(&task(Some(key(2, 3)))).await,
        SummaryOutcome::BaseMissing
    );
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn commit_outcomes_without_changes_are_reported() {
    for (stored, expected) in [
        (
            ThreadSummaryCommitOutcome::FrontierDeleted,
            SummaryOutcome::FrontierDeleted,
        ),
        (
            ThreadSummaryCommitOutcome::CasConflict,
            SummaryOutcome::CasConflict,
        ),
        (
            ThreadSummaryCommitOutcome::BaseMissing,
            SummaryOutcome::BaseMissing,
        ),
    ] {
        let store = FakeStore::with(conversation(), None);
        store.state.lock().unwrap().commit_outcome = Some(stored);
        let llm = FakeLlm::scripted(vec![answer(RAW, None)]);
        let svc = service(store, vec![summary_model(json!({}))], llm, settings());
        let outcome = svc.execute(&task(None)).await;
        assert_eq!(outcome, expected);
        assert_eq!(disposition(&outcome, 1, 3), Disposition::Ok);
    }
}

#[tokio::test]
async fn missing_or_disabled_model_is_unavailable_without_a_call() {
    for catalog in [
        vec![catalog_entry("other", json!({}))],
        vec![summary_model(json!({"enabled": false}))],
    ] {
        let store = FakeStore::with(conversation(), None);
        let llm = FakeLlm::scripted(vec![]);
        let svc = service(store, catalog, Arc::clone(&llm), settings());
        let outcome = svc.execute(&task(None)).await;
        assert!(
            matches!(outcome, SummaryOutcome::ModelUnavailable(_)),
            "{outcome:?}"
        );
        assert!(matches!(
            disposition(&outcome, 1, 3),
            Disposition::Reject(_)
        ));
        assert!(llm.requests().is_empty());
    }
}

#[tokio::test]
async fn plugin_and_provider_resolution_failures_are_retried() {
    let llm = FakeLlm::scripted(vec![]);
    let svc = ThreadSummaryService::new(
        FakeStore::with(conversation(), None),
        FakePolicy::failing(MiniChatModelPolicyPluginError::VersionNotFound { policy_version: 1 }),
        Arc::new(FakeResolver),
        Arc::clone(&llm) as Arc<dyn LlmProvider>,
        settings(),
    );
    assert!(matches!(
        svc.execute(&task(None)).await,
        SummaryOutcome::Retry(_)
    ));

    let unknown_provider = summary_model(json!({"provider_id": "nowhere"}));
    let svc = service(
        FakeStore::with(conversation(), None),
        vec![unknown_provider],
        Arc::clone(&llm),
        settings(),
    );
    assert!(matches!(
        svc.execute(&task(None)).await,
        SummaryOutcome::Retry(_)
    ));
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn provider_failure_keeps_the_previous_summary() {
    let store = FakeStore::with(conversation(), None);
    let llm = FakeLlm::scripted(vec![Err(LlmFailure::provider_error("HTTP 500"))]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        llm,
        settings(),
    );

    let outcome = svc.execute(&task(None)).await;

    assert!(
        matches!(outcome, SummaryOutcome::ProviderError(_)),
        "{outcome:?}"
    );
    assert_eq!(outcome.metric_result(), Some("provider_error"));
    assert!(store.commits().is_empty());
}

#[tokio::test]
async fn empty_summary_is_retried_without_a_commit() {
    let store = FakeStore::with(conversation(), None);
    let llm = FakeLlm::scripted(vec![answer(
        "<analysis>Nothing worth keeping.</analysis>",
        None,
    )]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        llm,
        settings(),
    );

    assert_eq!(svc.execute(&task(None)).await, SummaryOutcome::EmptySummary);
    assert!(store.commits().is_empty());
}

#[tokio::test]
async fn context_length_errors_drop_the_oldest_messages_twice_at_most() {
    let messages: Vec<_> = (0..10)
        .map(|i| message(MessageRole::User, &format!("message {i}"), i))
        .collect();
    let ptl = || {
        let mut failure = LlmFailure::provider_error("too long");
        failure.provider_code = Some("context_length_exceeded".to_owned());
        Err(failure)
    };
    let store = FakeStore::with(messages.clone(), None);
    let llm = FakeLlm::scripted(vec![ptl(), ptl(), ptl()]);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        Arc::clone(&llm),
        settings(),
    );

    let outcome = svc.execute(&task(None)).await;

    assert!(
        matches!(outcome, SummaryOutcome::ProviderError(_)),
        "{outcome:?}"
    );
    let prompts: Vec<String> = llm
        .requests()
        .into_iter()
        .map(|(_, req)| match &req.input[0] {
            LlmInputItem::User { text, .. } => text.clone(),
            other => panic!("user prompt expected, got {other:?}"),
        })
        .collect();
    assert_eq!(prompts.len(), 3, "one call and two PTL retries");
    assert_eq!(prompts[0], build_user_prompt(None, &messages, 4000));
    assert_eq!(prompts[1], build_user_prompt(None, &messages[2..], 4000));
    assert_eq!(prompts[2], build_user_prompt(None, &messages[4..], 4000));

    // A retry that succeeds commits the full frozen range.
    let llm = FakeLlm::scripted(vec![ptl(), answer(RAW, None)]);
    let store = FakeStore::with(messages, None);
    let svc = service(
        Arc::clone(&store),
        vec![summary_model(json!({}))],
        llm,
        settings(),
    );
    assert!(matches!(
        svc.execute(&task(None)).await,
        SummaryOutcome::Committed { .. }
    ));
    assert_eq!(store.commits()[0].target, key(4, 5));
}

#[tokio::test]
async fn database_failures_are_retried() {
    let store = FakeStore::with(conversation(), None);
    store.state.lock().unwrap().fail_reads = true;
    let svc = service(
        store,
        vec![summary_model(json!({}))],
        FakeLlm::scripted(vec![]),
        settings(),
    );
    assert!(matches!(
        svc.execute(&task(None)).await,
        SummaryOutcome::Retry(_)
    ));

    let store = FakeStore::with(conversation(), None);
    store.state.lock().unwrap().fail_commit = true;
    let svc = service(
        store,
        vec![summary_model(json!({}))],
        FakeLlm::scripted(vec![answer(RAW, None)]),
        settings(),
    );
    assert!(matches!(
        svc.execute(&task(None)).await,
        SummaryOutcome::Retry(_)
    ));
}

#[tokio::test]
async fn empty_range_finishes_without_a_call() {
    let store = FakeStore::with(vec![message(MessageRole::System, "sys", 1)], None);
    let llm = FakeLlm::scripted(vec![]);
    let svc = service(
        store,
        vec![summary_model(json!({}))],
        Arc::clone(&llm),
        settings(),
    );
    assert_eq!(
        svc.execute(&task(None)).await,
        SummaryOutcome::NothingToSummarize
    );
    assert!(llm.requests().is_empty());
}

// ── Startup check ────────────────────────────────────────────────────────

#[tokio::test]
async fn startup_check_reports_the_summary_model_state() {
    let config = ThreadSummaryWorkerConfig::default();
    let available = FakePolicy::new(vec![summary_model(json!({}))]);
    assert_eq!(
        check_summary_model(available.as_ref(), &config).await,
        SummaryModelCheck::Available
    );
    let disabled = FakePolicy::new(vec![summary_model(json!({"enabled": false}))]);
    assert_eq!(
        check_summary_model(disabled.as_ref(), &config).await,
        SummaryModelCheck::Unavailable
    );
    let failing =
        FakePolicy::failing(MiniChatModelPolicyPluginError::VersionNotFound { policy_version: 1 });
    assert!(matches!(
        check_summary_model(failing.as_ref(), &config).await,
        SummaryModelCheck::CatalogUnavailable(_)
    ));
    let off = ThreadSummaryWorkerConfig {
        enabled: false,
        ..ThreadSummaryWorkerConfig::default()
    };
    assert_eq!(
        check_summary_model(disabled.as_ref(), &off).await,
        SummaryModelCheck::NotEnabled
    );
}

#[test]
fn settings_apply_the_default_model_and_attempt_floor() {
    let config = ThreadSummaryWorkerConfig {
        max_attempts: 0,
        ..ThreadSummaryWorkerConfig::default()
    };
    let settings = SummarySettings::from_config(&config);
    assert_eq!(settings.model_id, "gpt-4.1-mini");
    assert_eq!(settings.max_attempts, 1);
    assert_eq!(settings.message_content_limit, 4000);
}
