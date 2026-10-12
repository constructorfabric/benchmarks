#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use async_trait::async_trait;
use mini_chat_sdk::{KillSwitches, PolicySnapshot, TokenUsage, UsageEvent, UserLimits};
use parking_lot::Mutex;
use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::*;
use crate::domain::llm::{LlmStream, ProviderError};
use crate::domain::ports::UsagePublication;

fn entry(id: &str, enabled: bool) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id, "provider_model_id": format!("provider-{id}"), "display_name": "M",
        "provider_id": "openai", "provider_display_name": "P", "tier": "standard",
        "enabled": enabled, "context_window": 128_000, "max_output_tokens": 700,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "estimation_budgets": {
            "bytes_per_token_conservative": 4, "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10, "image_token_budget": 1000, "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500, "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50
        },
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": false, "file_search": false,
                "image_generation": false, "code_interpreter": false, "mcp": false },
            "supported_endpoints": {}
        }
    }))
    .unwrap()
}

struct Policy {
    catalog: Vec<ModelCatalogEntry>,
}

#[async_trait]
impl ModelPolicyGateway for Policy {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        assert_eq!(
            user_id, SYSTEM_SUBJECT_ID,
            "a system task asks as the system subject"
        );
        Ok(PolicySnapshot {
            policy_version: 1,
            model_catalog: self.catalog.clone(),
            kill_switches: KillSwitches::default(),
        })
    }
    async fn snapshot_at(&self, _: Uuid, _: i64) -> Result<PolicySnapshot, DomainError> {
        Err(DomainError::internal("unused"))
    }
    async fn user_limits(&self, _: Uuid, _: i64) -> Result<UserLimits, DomainError> {
        Err(DomainError::internal("unused"))
    }
    async fn publish_usage(&self, _: UsageEvent) -> UsagePublication {
        UsagePublication::Published
    }
}

#[derive(Default)]
struct Provider {
    answers: Mutex<VecDeque<Result<LlmCompletion, ProviderError>>>,
    requests: Mutex<Vec<LlmRequest>>,
}

#[async_trait]
impl LlmProvider for Provider {
    fn provider_id(&self) -> &'static str {
        "openai"
    }
    async fn stream(
        &self,
        _: &SecurityContext,
        _: &LlmRequest,
    ) -> Result<LlmStream, ProviderError> {
        Err(ProviderError::provider("unused"))
    }
    async fn complete(
        &self,
        ctx: &SecurityContext,
        request: &LlmRequest,
    ) -> Result<LlmCompletion, ProviderError> {
        assert_eq!(ctx.subject_id(), SYSTEM_SUBJECT_ID);
        self.requests.lock().push(request.clone());
        self.answers
            .lock()
            .pop_front()
            .unwrap_or_else(|| Err(ProviderError::provider("no scripted answer")))
    }
}

struct Providers(Arc<Provider>);

impl LlmProviderResolver for Providers {
    fn resolve(&self, _: Uuid, _: &str) -> Result<Arc<dyn LlmProvider>, DomainError> {
        Ok(self.0.clone())
    }
}

struct Repo {
    stored: Option<StoredSummary>,
    alive: bool,
    range: Vec<RangeMessage>,
    commit: Mutex<Option<Result<CommitOutcome, DomainError>>>,
    commits: Mutex<Vec<CommitSummary>>,
}

#[async_trait]
impl SummaryRepository for Repo {
    async fn current(&self, _: Uuid, _: Uuid) -> Result<Option<StoredSummary>, DomainError> {
        Ok(self.stored.clone())
    }
    async fn message_alive(&self, _: Uuid, _: Uuid, _: Uuid) -> Result<bool, DomainError> {
        Ok(self.alive)
    }
    async fn load_range(
        &self,
        _: Uuid,
        _: Uuid,
        _: Option<Frontier>,
        _: Frontier,
    ) -> Result<Vec<RangeMessage>, DomainError> {
        Ok(self.range.clone())
    }
    async fn commit(&self, _: Uuid, command: CommitSummary) -> Result<CommitOutcome, DomainError> {
        self.commits.lock().push(command);
        self.commit
            .lock()
            .take()
            .unwrap_or(Ok(CommitOutcome::Committed))
    }
}

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<String>>,
}

impl Metrics for Recorder {
    fn stream_started(&self, _: &str, _: &str) {}
    fn stream_completed(&self, _: &str, _: &str) {}
    fn stream_failed(&self, _: &str, _: &str, _: &str) {}
    fn stream_incomplete(&self, _: &str, _: &str, _: &str) {}
    fn stream_disconnected(&self, _: &str) {}
    fn active_streams(&self, _: i64) {}
    fn cancel_requested(&self, _: &str) {}
    fn cancel_effective(&self, _: &str) {}
    fn time_to_abort_ms(&self, _: &str, _: f64) {}
    fn streams_aborted(&self, _: &str) {}
    fn quota_preflight(&self, _: &str, _: &str, _: &str) {}
    fn quota_reserve(&self, _: &str) {}
    fn quota_commit(&self, _: &str) {}
    fn quota_overshoot(&self, _: &str) {}
    fn quota_estimated_tokens(&self, _: f64) {}
    fn quota_actual_tokens(&self, _: f64) {}
    fn code_interpreter_calls(&self, _: &str, _: u64) {}
    fn finalization_latency_ms(&self, _: f64) {}
    fn ttft_provider_ms(&self, _: &str, _: &str, _: f64) {}
    fn stream_total_latency_ms(&self, _: &str, _: &str, _: f64) {}
    fn orphan_detected(&self, _: &str) {}
    fn orphan_finalized(&self, _: &str) {}
    fn orphan_scan_duration_seconds(&self, _: f64) {}
    fn audit_emit(&self, _: &str) {}
    fn thread_summary_execution(&self, result: &str) {
        self.seen.lock().push(format!("execution:{result}"));
    }
    fn thread_summary_cas_conflict(&self) {
        self.seen.lock().push("cas_conflict".to_owned());
    }
    fn summary_fallback(&self) {
        self.seen.lock().push("fallback".to_owned());
    }
}

struct Fixture {
    service: SummaryService,
    provider: Arc<Provider>,
    repo: Arc<Repo>,
    metrics: Arc<Recorder>,
}

fn frontier(second: i64, id: u128) -> Frontier {
    Frontier {
        created_at: datetime!(2026-01-01 00:00:00 UTC) + time::Duration::seconds(second),
        message_id: Uuid::from_u128(id),
    }
}

fn messages(count: usize) -> Vec<RangeMessage> {
    (0..count)
        .map(|index| RangeMessage {
            role: if index % 2 == 0 {
                InputRole::User
            } else {
                InputRole::Assistant
            },
            content: format!("message {index}"),
        })
        .collect()
}

fn fixture(
    catalog: Vec<ModelCatalogEntry>,
    stored: Option<StoredSummary>,
    alive: bool,
    range: Vec<RangeMessage>,
    answers: Vec<Result<LlmCompletion, ProviderError>>,
) -> Fixture {
    let provider = Arc::new(Provider::default());
    provider.answers.lock().extend(answers);
    let repo = Arc::new(Repo {
        stored,
        alive,
        range,
        commit: Mutex::new(None),
        commits: Mutex::new(Vec::new()),
    });
    let metrics = Arc::new(Recorder::default());
    let service = SummaryService::new(SummaryServiceParts {
        repo: repo.clone(),
        policy: Arc::new(Policy { catalog }),
        providers: Arc::new(Providers(provider.clone())),
        config: ThreadSummaryWorkerConfig {
            summary_model_id: "summary".to_owned(),
            ..ThreadSummaryWorkerConfig::default()
        },
        metrics: metrics.clone(),
    });
    Fixture {
        service,
        provider,
        repo,
        metrics,
    }
}

fn item(base: Option<Frontier>) -> SummaryWorkItem {
    SummaryWorkItem::new(
        Uuid::from_u128(1),
        Uuid::from_u128(2),
        Uuid::from_u128(3),
        base,
        frontier(10, 9),
    )
}

fn completion(text: &str) -> LlmCompletion {
    LlmCompletion {
        text: text.to_owned(),
        usage: Some(TokenUsage {
            input_tokens: 120,
            output_tokens: 40,
            ..TokenUsage::default()
        }),
    }
}

fn seen(fixture: &Fixture) -> Vec<String> {
    fixture.metrics.seen.lock().clone()
}

#[tokio::test]
async fn a_successful_attempt_commits_the_parsed_summary_with_a_system_task_usage_event() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(4),
        vec![Ok(completion(
            "<analysis>x</analysis><summary>THE SUMMARY</summary>",
        ))],
    );

    let outcome = fx.service.run(&item(None)).await;

    assert_eq!(outcome, SummaryOutcome::Done);
    let commits = fx.repo.commits.lock();
    assert_eq!(commits.len(), 1);
    let commit = &commits[0];
    assert_eq!(commit.summary_text, "THE SUMMARY");
    assert_eq!(commit.token_estimate, 40);
    assert_eq!(commit.target, frontier(10, 9));
    assert_eq!(commit.base, None);
    let event = &commit.usage_event;
    assert_eq!(event.billing_outcome, BillingOutcome::SystemTask);
    assert_eq!(event.settlement_method, SettlementMethod::None);
    assert_eq!(event.requester_type, RequesterType::System);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.user_id, None);
    assert_eq!(event.turn_id, None);
    assert_eq!(event.effective_model, "summary");
    assert_eq!(
        event.dedupe_key,
        format!(
            "{}/thread_summary_update/{}",
            Uuid::from_u128(1).simple(),
            Uuid::from_u128(3).simple()
        )
    );
    assert_eq!(seen(&fx), ["execution:success"]);
}

#[tokio::test]
async fn the_request_is_non_streaming_system_and_without_tools() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(2),
        vec![Ok(completion("S"))],
    );

    fx.service.run(&item(None)).await;

    let requests = fx.provider.requests.lock();
    let request = &requests[0];
    assert_eq!(request.model, "provider-summary");
    assert_eq!(request.max_output_tokens, 700);
    assert_eq!(request.request_type, RequestType::Summary);
    assert_eq!(request.feature, "none");
    assert!(request.tools.is_empty());
    assert_eq!(request.identity.user_id, SYSTEM_SUBJECT_ID);
    assert_eq!(request.input.len(), 1);
    assert_eq!(request.system_prompt, DEFAULT_SUMMARY_SYSTEM_PROMPT);
}

#[tokio::test]
async fn a_missing_or_disabled_model_rejects_without_a_provider_call() {
    for catalog in [vec![entry("other", true)], vec![entry("summary", false)]] {
        let fx = fixture(catalog, None, true, messages(2), vec![Ok(completion("S"))]);

        let outcome = fx.service.run(&item(None)).await;

        assert!(matches!(outcome, SummaryOutcome::Reject(_)), "{outcome:?}");
        assert!(fx.provider.requests.lock().is_empty());
        assert_eq!(seen(&fx), ["execution:model_unavailable"]);
    }
}

#[tokio::test]
async fn a_provider_failure_retries_and_keeps_the_previous_summary() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(2),
        vec![Err(ProviderError::provider("down"))],
    );

    let outcome = fx.service.run(&item(None)).await;

    assert_eq!(outcome, SummaryOutcome::Retry);
    assert!(fx.repo.commits.lock().is_empty());
    assert_eq!(seen(&fx), ["execution:provider_error", "fallback"]);
}

#[tokio::test]
async fn an_empty_answer_retries() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(2),
        vec![Ok(completion("<summary>  </summary>"))],
    );

    assert_eq!(fx.service.run(&item(None)).await, SummaryOutcome::Retry);
    assert!(fx.repo.commits.lock().is_empty());
    assert_eq!(seen(&fx), ["execution:empty_summary"]);
}

#[tokio::test]
async fn a_moved_frontier_finishes_without_a_provider_call() {
    let stored = StoredSummary {
        text: "OLD".to_owned(),
        frontier: frontier(5, 5),
    };
    let fx = fixture(
        vec![entry("summary", true)],
        Some(stored),
        true,
        messages(2),
        vec![Ok(completion("S"))],
    );

    assert_eq!(fx.service.run(&item(None)).await, SummaryOutcome::Done);
    assert!(fx.provider.requests.lock().is_empty());
    assert_eq!(seen(&fx), ["cas_conflict"]);
}

#[tokio::test]
async fn a_dropped_base_summary_finishes_without_a_provider_call() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(2),
        vec![Ok(completion("S"))],
    );

    assert_eq!(
        fx.service.run(&item(Some(frontier(5, 5)))).await,
        SummaryOutcome::Done
    );
    assert!(fx.provider.requests.lock().is_empty());
    assert_eq!(seen(&fx), ["execution:base_missing"]);
}

#[tokio::test]
async fn a_deleted_target_finishes_without_a_provider_call() {
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        false,
        messages(2),
        vec![Ok(completion("S"))],
    );

    assert_eq!(fx.service.run(&item(None)).await, SummaryOutcome::Done);
    assert!(fx.provider.requests.lock().is_empty());
    assert_eq!(seen(&fx), ["execution:frontier_deleted"]);
}

#[tokio::test]
async fn an_update_sends_the_existing_summary() {
    let stored = StoredSummary {
        text: "OLD-SUMMARY".to_owned(),
        frontier: frontier(5, 5),
    };
    let fx = fixture(
        vec![entry("summary", true)],
        Some(stored),
        true,
        messages(2),
        vec![Ok(completion("S"))],
    );

    fx.service.run(&item(Some(frontier(5, 5)))).await;

    let requests = fx.provider.requests.lock();
    assert!(
        requests[0].input[0]
            .text
            .contains("<existing_summary>\nOLD-SUMMARY\n")
    );
}

#[tokio::test]
async fn a_context_length_failure_is_retried_with_fewer_messages_at_most_twice() {
    let too_long = || {
        Err(ProviderError::provider(
            "This model's maximum context length is 8192 tokens",
        ))
    };
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(10),
        vec![too_long(), too_long(), Ok(completion("S"))],
    );

    assert_eq!(fx.service.run(&item(None)).await, SummaryOutcome::Done);

    let requests = fx.provider.requests.lock();
    assert_eq!(requests.len(), 3);
    let kept = |request: &LlmRequest| {
        request.input[0].text.matches("\n\nUser: ").count()
            + request.input[0].text.matches("\n\nAssistant: ").count()
            + 1
    };
    assert!(kept(&requests[1]) < kept(&requests[0]));
    assert!(kept(&requests[2]) < kept(&requests[1]));
}

#[tokio::test]
async fn the_third_context_length_failure_is_a_provider_failure() {
    let too_long = || Err(ProviderError::provider("context_length_exceeded"));
    let fx = fixture(
        vec![entry("summary", true)],
        None,
        true,
        messages(10),
        vec![too_long(), too_long(), too_long(), Ok(completion("S"))],
    );

    assert_eq!(fx.service.run(&item(None)).await, SummaryOutcome::Retry);
    assert_eq!(fx.provider.requests.lock().len(), 3);
}

#[tokio::test]
async fn the_commit_results_map_to_outcomes_and_metrics() {
    for (commit, outcome, metric) in [
        (
            Ok(CommitOutcome::FrontierDeleted),
            SummaryOutcome::Done,
            "execution:frontier_deleted",
        ),
        (
            Ok(CommitOutcome::BaseMissing),
            SummaryOutcome::Done,
            "execution:base_missing",
        ),
        (
            Ok(CommitOutcome::CasConflict),
            SummaryOutcome::Done,
            "cas_conflict",
        ),
        (
            Err(DomainError::internal("db down")),
            SummaryOutcome::Retry,
            "execution:retry",
        ),
    ] {
        let fx = fixture(
            vec![entry("summary", true)],
            None,
            true,
            messages(2),
            vec![Ok(completion("S"))],
        );
        *fx.repo.commit.lock() = Some(commit);

        assert_eq!(fx.service.run(&item(None)).await, outcome);
        assert_eq!(seen(&fx), [metric]);
    }
}

#[tokio::test]
async fn the_catalog_prompt_wins_over_the_configured_one() {
    let mut model = entry("summary", true);
    model.thread_summary_prompt = "CATALOG".to_owned();
    let fx = fixture(
        vec![model],
        None,
        true,
        messages(2),
        vec![Ok(completion("S"))],
    );

    fx.service.run(&item(None)).await;

    assert_eq!(fx.provider.requests.lock()[0].system_prompt, "CATALOG");
}
