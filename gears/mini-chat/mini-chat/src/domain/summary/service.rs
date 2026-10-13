//! The thread-summary handler (DESIGN section 3.6 "Thread Summary Update").
//!
//! One attempt per delivery of a work item: resolve the summary model, check the frontier, load
//! the frozen range, fit and send the prompt (non-streaming, system identity), parse the answer
//! and commit it with a compare-and-set on the frontier. The attempt never creates a turn, never
//! debits a user quota and reports its usage as a `system_task` event.

use std::sync::Arc;

use mini_chat_sdk::{
    BillingOutcome, ModelCatalogEntry, RequesterType, SettlementMethod, UsageEvent,
    UsageTerminalState,
};
use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;
use tracing::{debug, error, warn};

use super::model::{
    CommitOutcome, CommitSummary, Frontier, RangeMessage, SYSTEM_SUBJECT_ID, SYSTEM_TASK_TYPE,
    StoredSummary, SummaryOutcome, SummaryWorkItem,
};
use super::parse::{parse_summary, token_estimate};
use super::ports::SummaryRepository;
use super::prompt::{drop_step, fit, input_budget, user_prompt};
use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, ThreadSummaryWorkerConfig};
use crate::domain::clock;
use crate::domain::error::DomainError;
use crate::domain::llm::{
    ApiParams, InputMessage, InputRole, LlmCompletion, LlmProvider, LlmProviderResolver,
    LlmRequest, ProviderError, RequestIdentity, RequestType,
};
use crate::domain::metrics::Metrics;
use crate::domain::ports::ModelPolicyGateway;

/// How long the check of the summary model at gear start may take.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How many times a context-length failure is retried with fewer messages.
const MAX_CONTEXT_RETRIES: u32 = 2;

/// Why the summary model could not be used.
enum ModelError {
    /// Missing from the catalog or disabled: the task can never succeed.
    Unavailable,
    /// The policy could not be read: a later delivery may succeed.
    Failed(DomainError),
}

/// The state of the stored summary relative to the task's base frontier.
#[derive(Debug, PartialEq, Eq)]
enum BaseState {
    /// The stored frontier equals the base (or neither exists).
    Current,
    /// The task is based on a summary that no longer exists.
    Missing,
    /// Another commit already moved the frontier.
    Moved,
}

fn base_state(base: Option<Frontier>, stored: Option<&StoredSummary>) -> BaseState {
    match (base, stored) {
        (None, None) => BaseState::Current,
        (Some(base), Some(stored)) if stored.frontier == base => BaseState::Current,
        (Some(_), None) => BaseState::Missing,
        _ => BaseState::Moved,
    }
}

/// Whether a provider failure says the prompt was too large for the model.
fn is_context_length_error(err: &ProviderError) -> bool {
    let message = err.message.to_ascii_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "context window",
        "maximum context",
        "prompt is too long",
        "input is too long",
        "too many tokens",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// Collaborators of the summary service.
pub struct SummaryServiceParts {
    /// Summary persistence.
    pub repo: Arc<dyn SummaryRepository>,
    /// The model policy (catalog).
    pub policy: Arc<dyn ModelPolicyGateway>,
    /// The provider resolver.
    pub providers: Arc<dyn LlmProviderResolver>,
    /// `thread_summary_worker`.
    pub config: ThreadSummaryWorkerConfig,
    /// The metrics sink.
    pub metrics: Arc<dyn Metrics>,
}

/// The thread-summary handler.
#[domain_model]
pub struct SummaryService {
    repo: Arc<dyn SummaryRepository>,
    policy: Arc<dyn ModelPolicyGateway>,
    providers: Arc<dyn LlmProviderResolver>,
    config: ThreadSummaryWorkerConfig,
    metrics: Arc<dyn Metrics>,
}

impl SummaryService {
    /// Creates the service.
    #[must_use]
    pub fn new(parts: SummaryServiceParts) -> Self {
        Self {
            repo: parts.repo,
            policy: parts.policy,
            providers: parts.providers,
            config: parts.config,
            metrics: parts.metrics,
        }
    }

    /// Resolves the summary model with the enabled filter.
    async fn resolve_model(&self) -> Result<ModelCatalogEntry, ModelError> {
        let snapshot = self
            .policy
            .current_snapshot(SYSTEM_SUBJECT_ID)
            .await
            .map_err(ModelError::Failed)?;
        let id = self.config.effective_summary_model_id();
        snapshot
            .model_catalog
            .into_iter()
            .find(|entry| entry.id == id && entry.enabled)
            .ok_or(ModelError::Unavailable)
    }

    /// Gear start: logs an error when summaries are enabled and the summary model is missing or
    /// disabled. Startup continues, because a dynamic policy plugin can add the model later.
    pub async fn check_model(&self) {
        if !self.config.enabled {
            return;
        }
        let checked = tokio::time::timeout(CHECK_TIMEOUT, self.resolve_model()).await;
        self.report_model_check(checked.ok());
    }

    /// Logs the result of the start check; `None` is a check that timed out.
    fn report_model_check(&self, checked: Option<Result<ModelCatalogEntry, ModelError>>) {
        match checked {
            Some(Ok(_)) => {}
            Some(Err(ModelError::Unavailable)) => self.log_model_missing(),
            Some(Err(ModelError::Failed(err))) => log_policy_unavailable(&err),
            None => log_check_timeout(),
        }
    }

    fn log_model_missing(&self) {
        error!(
            model = self.config.effective_summary_model_id(),
            "the thread summary model is missing from the catalog or disabled; summary tasks will be rejected"
        );
    }

    /// One attempt of a work item.
    pub async fn run(&self, item: &SummaryWorkItem) -> SummaryOutcome {
        let entry = match self.resolve_model().await {
            Ok(entry) => entry,
            Err(err) => return self.model_failure(item, err),
        };
        match self.attempt(item, &entry).await {
            Ok(outcome) | Err(outcome) => outcome,
        }
    }

    fn model_failure(&self, item: &SummaryWorkItem, err: ModelError) -> SummaryOutcome {
        match err {
            ModelError::Unavailable => {
                error!(
                    chat_id = %item.chat_id,
                    model = self.config.effective_summary_model_id(),
                    "the thread summary model is missing or disabled; rejecting the task"
                );
                self.metrics.thread_summary_execution("model_unavailable");
                SummaryOutcome::Reject("the summary model is missing or disabled".to_owned())
            }
            ModelError::Failed(err) => {
                self.step_failed(item, "the summary model could not be resolved", &err)
            }
        }
    }

    /// Logs a failed step and asks for another delivery.
    fn step_failed(
        &self,
        item: &SummaryWorkItem,
        step: &str,
        err: &dyn std::fmt::Display,
    ) -> SummaryOutcome {
        warn!(chat_id = %item.chat_id, error = %err, "{step}");
        self.metrics.thread_summary_execution("retry");
        SummaryOutcome::Retry
    }

    /// The stages of one attempt; an `Err` ends the attempt with its outcome.
    async fn attempt(
        &self,
        item: &SummaryWorkItem,
        entry: &ModelCatalogEntry,
    ) -> Result<SummaryOutcome, SummaryOutcome> {
        let existing = self.check_frontier(item).await?;
        self.check_target(item).await?;
        let messages = self.load_messages(item).await?;
        let completion = self
            .summarize(item, entry, existing.as_deref(), &messages)
            .await?;
        let Some(summary_text) = parse_summary(&completion.text) else {
            warn!(chat_id = %item.chat_id, "the summary answer is empty");
            self.metrics.thread_summary_execution("empty_summary");
            return Ok(SummaryOutcome::Retry);
        };
        Ok(self.commit(item, entry, summary_text, completion).await)
    }

    /// The pre-check of the compare-and-set: the stored summary must still be the one the task is
    /// based on. Returns its text for an update prompt.
    async fn check_frontier(
        &self,
        item: &SummaryWorkItem,
    ) -> Result<Option<String>, SummaryOutcome> {
        let stored = self
            .repo
            .current(item.tenant_id, item.chat_id)
            .await
            .map_err(|err| self.step_failed(item, "the stored summary could not be read", &err))?;
        match base_state(item.base(), stored.as_ref()) {
            BaseState::Current => Ok(stored.map(|summary| summary.text)),
            BaseState::Missing => {
                debug!(chat_id = %item.chat_id, "the summary the task was based on is gone");
                self.metrics.thread_summary_execution("base_missing");
                Err(SummaryOutcome::Done)
            }
            BaseState::Moved => {
                debug!(chat_id = %item.chat_id, "the summary frontier already moved");
                self.metrics.thread_summary_cas_conflict();
                Err(SummaryOutcome::Done)
            }
        }
    }

    /// The frozen target must not have been soft-deleted by a mutation.
    async fn check_target(&self, item: &SummaryWorkItem) -> Result<(), SummaryOutcome> {
        let alive = self
            .repo
            .message_alive(item.tenant_id, item.chat_id, item.frozen_target_message_id)
            .await
            .map_err(|err| {
                self.step_failed(item, "the frozen target could not be checked", &err)
            })?;
        if alive {
            return Ok(());
        }
        debug!(chat_id = %item.chat_id, "the frozen target message was deleted");
        self.metrics.thread_summary_execution("frontier_deleted");
        Err(SummaryOutcome::Done)
    }

    /// The messages of the frozen range; an empty range ends the attempt.
    async fn load_messages(
        &self,
        item: &SummaryWorkItem,
    ) -> Result<Vec<RangeMessage>, SummaryOutcome> {
        let messages = self
            .repo
            .load_range(item.tenant_id, item.chat_id, item.base(), item.target())
            .await
            .map_err(|err| self.step_failed(item, "the summary range could not be loaded", &err))?;
        if messages.is_empty() {
            debug!(chat_id = %item.chat_id, "no message is left to summarize");
            return Err(SummaryOutcome::Done);
        }
        Ok(messages)
    }

    /// The provider call with the fitted prompt. A failure is counted here and ends the attempt
    /// with a retry.
    async fn summarize(
        &self,
        item: &SummaryWorkItem,
        entry: &ModelCatalogEntry,
        existing: Option<&str>,
        messages: &[RangeMessage],
    ) -> Result<LlmCompletion, SummaryOutcome> {
        let provider = self
            .providers
            .resolve(item.tenant_id, &entry.provider_id)
            .map_err(|err| {
                self.step_failed(item, "the summary provider could not be resolved", &err)
            })?;
        let ctx = SecurityContext::builder()
            .subject_id(SYSTEM_SUBJECT_ID)
            .subject_tenant_id(item.tenant_id)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .map_err(|err| {
                self.step_failed(item, "the system security context could not be built", &err)
            })?;
        let system_prompt = self.system_prompt(entry);
        let limit = self.config.message_content_limit;
        let mut dropped = fit(
            &system_prompt,
            existing,
            messages,
            limit,
            input_budget(
                entry.context_window,
                entry.max_output_tokens,
                entry.max_input_tokens,
            ),
            entry.estimation_budgets.bytes_per_token_conservative,
        );
        let mut context_retries = 0;
        loop {
            let kept = &messages[dropped..];
            let prompt = user_prompt(existing, kept, limit);
            let request = summary_request(item, entry, &system_prompt, prompt);
            let err = match provider.complete(&ctx, &request).await {
                Ok(completion) => return Ok(completion),
                Err(err) => err,
            };
            let step = drop_step(kept.len());
            if is_context_length_error(&err) && context_retries < MAX_CONTEXT_RETRIES && step > 0 {
                context_retries += 1;
                dropped += step;
                continue;
            }
            self.report_provider_failure(item, provider.as_ref(), &err);
            return Err(SummaryOutcome::Retry);
        }
    }

    fn report_provider_failure(
        &self,
        item: &SummaryWorkItem,
        provider: &dyn LlmProvider,
        err: &ProviderError,
    ) {
        warn!(
            chat_id = %item.chat_id,
            provider = provider.provider_id(),
            kind = err.kind.code(),
            error = %err.message,
            "the summary call failed; keeping the previous summary"
        );
        self.metrics.thread_summary_execution("provider_error");
        self.metrics.summary_fallback();
    }

    /// The system prompt: the model's `thread_summary_prompt`, else the configured one, else the
    /// built-in text.
    fn system_prompt(&self, entry: &ModelCatalogEntry) -> String {
        [
            entry.thread_summary_prompt.as_str(),
            self.config.summary_system_prompt.as_str(),
        ]
        .into_iter()
        .find(|prompt| !prompt.is_empty())
        .unwrap_or(DEFAULT_SUMMARY_SYSTEM_PROMPT)
        .to_owned()
    }

    async fn commit(
        &self,
        item: &SummaryWorkItem,
        entry: &ModelCatalogEntry,
        summary_text: String,
        completion: LlmCompletion,
    ) -> SummaryOutcome {
        let now = clock::now();
        let estimate = token_estimate(&summary_text, completion.usage.as_ref());
        let usage_event = UsageEvent {
            tenant_id: item.tenant_id,
            user_id: None,
            chat_id: item.chat_id,
            turn_id: None,
            request_id: item.system_request_id,
            effective_model: entry.id.clone(),
            selected_model: entry.id.clone(),
            terminal_state: UsageTerminalState::Completed,
            billing_outcome: BillingOutcome::SystemTask,
            usage: completion.usage,
            actual_credits_micro: 0,
            settlement_method: SettlementMethod::None,
            policy_version_applied: 0,
            web_search_calls: 0,
            code_interpreter_calls: 0,
            file_search_calls: 0,
            timestamp: now,
            requester_type: RequesterType::System,
            dedupe_key: format!(
                "{}/{SYSTEM_TASK_TYPE}/{}",
                item.tenant_id.simple(),
                item.system_request_id.simple()
            ),
            system_task_type: Some(SYSTEM_TASK_TYPE.to_owned()),
        };
        let command = CommitSummary {
            chat_id: item.chat_id,
            base: item.base(),
            target: item.target(),
            summary_text,
            token_estimate: estimate,
            usage_event,
            now,
        };
        self.commit_result(item, self.repo.commit(item.tenant_id, command).await)
    }

    /// Maps the commit result to the outcome and the metrics.
    fn commit_result(
        &self,
        item: &SummaryWorkItem,
        result: Result<CommitOutcome, DomainError>,
    ) -> SummaryOutcome {
        let superseded = match result {
            Ok(CommitOutcome::Committed) => {
                self.metrics.thread_summary_execution("success");
                return SummaryOutcome::Done;
            }
            Ok(CommitOutcome::FrontierDeleted) => "frontier_deleted",
            Ok(CommitOutcome::BaseMissing) => "base_missing",
            Ok(CommitOutcome::CasConflict) => "cas_conflict",
            Err(err) => return self.step_failed(item, "the summary commit failed", &err),
        };
        debug!(chat_id = %item.chat_id, result = superseded, "the summary commit was superseded");
        if superseded == "cas_conflict" {
            self.metrics.thread_summary_cas_conflict();
        } else {
            self.metrics.thread_summary_execution(superseded);
        }
        SummaryOutcome::Done
    }
}

fn log_policy_unavailable(err: &DomainError) {
    warn!(
        error = %err,
        "the thread summary model could not be checked at start: the policy is unavailable"
    );
}

fn log_check_timeout() {
    warn!("the thread summary model check at start timed out");
}

fn summary_request(
    item: &SummaryWorkItem,
    entry: &ModelCatalogEntry,
    system_prompt: &str,
    user_prompt: String,
) -> LlmRequest {
    LlmRequest {
        model: entry.provider_model_id.clone(),
        system_prompt: system_prompt.to_owned(),
        input: vec![InputMessage {
            role: InputRole::User,
            text: user_prompt,
            image_file_ids: Vec::new(),
        }],
        max_output_tokens: entry.max_output_tokens,
        tools: Vec::new(),
        max_tool_calls: None,
        identity: RequestIdentity {
            tenant_id: item.tenant_id,
            user_id: SYSTEM_SUBJECT_ID,
            chat_id: item.chat_id,
        },
        request_type: RequestType::Summary,
        api_params: ApiParams::from_entry(entry),
        feature: "none".to_owned(),
        secondary_file_ids: std::collections::HashMap::new(),
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
