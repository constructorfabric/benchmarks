//! The thread-summary handler logic
//! (`cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`,
//! `-call-summary-model`): summarizes the frozen range of a work item as a
//! system task and commits it under the frontier compare-and-set.

use std::sync::Arc;
use std::time::Duration;

use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::clock::{WorkerClock, with_timeout};
use super::metrics::{SummaryResult, WorkerMetrics};
use super::ports::{
    CommitOutcome, SummaryCallError, SummaryCommit, SummaryModelClient, SummaryRequest,
    SummaryStore,
};
use super::summary_parse::{parse_summary, token_estimate};
use super::summary_prompt::{
    SummaryPrompt, build_prompt, drop_oldest_fifth, rebuild_prompt, select_system_prompt,
};
use super::summary_trigger::SummaryTask;
use super::{HandlerOutcome, POLICY_CALL_BOUND, SUMMARY_COMMIT_RESERVE};
use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::attachments::ports::BackgroundRuntime;
use crate::domain::service::quota::service::Clock;
use crate::domain::service::stream::ports::ProviderDirectory;

/// The model used when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL_ID: &str = "gpt-4.1-mini";

/// Context-length retries within one delivery.
pub const MAX_CONTEXT_RETRIES: u32 = 2;

/// What the handler is built from.
pub struct SummaryDeps {
    /// The store of the handler.
    pub store: Arc<dyn SummaryStore>,
    /// Model-policy catalog (the summary model).
    pub policy: Arc<dyn PolicyCatalog>,
    /// Provider entries by catalog provider id.
    pub providers: Arc<dyn ProviderDirectory>,
    /// The non-streaming summary call.
    pub client: Arc<dyn SummaryModelClient>,
    /// Instruments.
    pub metrics: Arc<dyn WorkerMetrics>,
    /// The S2S context of the gear.
    pub runtime: Arc<dyn BackgroundRuntime>,
    /// Wall clock of the timestamps.
    pub clock: Clock,
    /// Monotonic clock of the deadlines.
    pub timer: Arc<dyn WorkerClock>,
    /// `thread_summary_worker.*`.
    pub config: ThreadSummaryWorkerConfig,
}

/// The thread-summary handler logic.
pub struct SummaryService {
    deps: SummaryDeps,
}

/// How the summary model resolved.
enum Resolution {
    Ready(Box<ResolvedModel>),
    /// Missing from the catalog or disabled.
    Unavailable,
    /// A plugin error or timeout, or an unresolvable provider entry.
    Failed,
}

struct ResolvedModel {
    entry: mini_chat_sdk::ModelCatalogEntry,
    target: crate::domain::service::stream::ports::ProviderTarget,
}

/// How the model call ended.
enum Called {
    Answer(super::ports::SummaryAnswer),
    ProviderFailure,
}

impl SummaryService {
    /// Creates the service.
    #[must_use]
    pub fn new(deps: SummaryDeps) -> Self {
        Self { deps }
    }

    /// Records `result` and turns a retry into a dead letter at the last
    /// delivery (`inst-retry-budget`).
    fn retry(&self, delivery_number: i64, result: SummaryResult) -> HandlerOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-retry-budget
        self.deps.metrics.summary_execution(result);
        if delivery_number >= i64::from(self.deps.config.max_attempts) {
            tracing::error!(
                max_attempts = self.deps.config.max_attempts,
                "a thread summary work item is dead-lettered"
            );
            HandlerOutcome::Reject(format!(
                "thread summary: max attempts ({}) reached",
                self.deps.config.max_attempts
            ))
        } else {
            HandlerOutcome::Retry
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-retry-budget
    }

    fn model_id(&self) -> &str {
        if self.deps.config.summary_model_id.is_empty() {
            DEFAULT_SUMMARY_MODEL_ID
        } else {
            &self.deps.config.summary_model_id
        }
    }

    /// Handles one work item delivered `attempts` times before.
    #[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
    pub async fn handle(&self, task: &SummaryTask, attempts: i16) -> HandlerOutcome {
        let deps = &self.deps;
        let delivery_number = i64::from(attempts) + 1;
        let started = deps.timer.now();
        let (tenant_id, chat_id) = (task.tenant_id, task.chat_id);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-load-summary-chat
        let deleted = match deps.store.chat_deleted(tenant_id, chat_id).await {
            Ok(deleted) => deleted,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the chat of a summary work item could not be read");
                return self.retry(delivery_number, SummaryResult::Retry);
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-load-summary-chat
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-summary-chat-gone
        if deleted != Some(false) {
            return HandlerOutcome::Ok;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-summary-chat-gone
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-load-current-summary
        let stored = match deps.store.summary(tenant_id, chat_id).await {
            Ok(stored) => stored,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the summary of a chat could not be read");
                return self.retry(delivery_number, SummaryResult::Retry);
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-load-current-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-base-missing
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        if task.base.is_some() && stored.is_none() {
            deps.metrics.summary_execution(SummaryResult::BaseMissing);
            return HandlerOutcome::Ok;
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-base-missing
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-frontier-moved
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        if stored.as_ref().map(|summary| summary.frontier) != task.base {
            deps.metrics.summary_cas_conflict();
            return HandlerOutcome::Ok;
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-frontier-moved
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-target-deleted
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        match deps
            .store
            .message_deleted(tenant_id, chat_id, task.target.id)
            .await
        {
            Ok(Some(false)) => {}
            Ok(Some(true) | None) => {
                deps.metrics
                    .summary_execution(SummaryResult::FrontierDeleted);
                return HandlerOutcome::Ok;
            }
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the frozen target could not be read");
                return self.retry(delivery_number, SummaryResult::Retry);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-target-deleted
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-select-range
        let range = match deps
            .store
            .range(tenant_id, chat_id, task.base, task.target)
            .await
        {
            Ok(range) => range,
            Err(error) => {
                tracing::warn!(%chat_id, %error, "the summary range could not be read");
                return self.retry(delivery_number, SummaryResult::Retry);
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-select-range
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-range-empty
        if range.is_empty() {
            tracing::warn!(%chat_id, "the summary range is empty");
            return HandlerOutcome::Ok;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-range-empty
        // The S2S context is needed only from here on (the chat-state checks above
        // come first). The gear binds the runtime before the outbox delivers; an
        // unbound runtime is a plain `Retry` that records no metric and never
        // dead-letters, because it is not a failed delivery of the work item.
        let Some(ctx) = deps.runtime.s2s_context() else {
            tracing::warn!(%chat_id, "the gear runtime is not bound yet, the summary is retried");
            return HandlerOutcome::Retry;
        };
        let model = match self.resolve_model(&ctx, tenant_id).await {
            Resolution::Ready(model) => model,
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-model-unavailable
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Resolution::Unavailable => {
                tracing::error!(
                    model = self.model_id(),
                    "the thread summary model is unavailable"
                );
                deps.metrics
                    .summary_execution(SummaryResult::ModelUnavailable);
                return HandlerOutcome::Reject("summary model unavailable".to_owned());
            }
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-model-unavailable
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-model-resolution-error
            Resolution::Failed => return self.retry(delivery_number, SummaryResult::Retry),
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-if-model-resolution-error
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-build-prompt
        let system = select_system_prompt(
            &model.entry.thread_summary_prompt,
            &deps.config.summary_system_prompt,
        );
        let existing = stored.as_ref().map(|summary| summary.text.as_str());
        let prompt = build_prompt(
            &model.entry,
            &range,
            existing,
            &system,
            deps.config.message_content_limit,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-build-prompt
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-match-summary-call
        let answer = match self
            .call_model(&ctx, &model, prompt, existing, tenant_id, chat_id, started)
            .await
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-summary-text
            Called::Answer(answer) => answer,
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-summary-text
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-provider-error
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Called::ProviderFailure => {
                deps.metrics.summary_fallback();
                return self.retry(delivery_number, SummaryResult::ProviderError);
            } // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
              // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-provider-error
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-match-summary-call
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-empty-summary
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        let Some(summary_text) = parse_summary(&answer.text) else {
            return self.retry(delivery_number, SummaryResult::EmptySummary);
        };
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-empty-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-precompute-commit
        let estimate = token_estimate(&answer.usage, &summary_text);
        let commit = SummaryCommit {
            tenant_id,
            chat_id,
            base: task.base,
            target: task.target,
            summary_text,
            token_estimate: estimate,
            summary_id: Uuid::new_v4(),
            system_request_id: task.system_request_id,
            summary_model: model.entry.id.clone(),
            usage: Some(answer.usage),
            now: (deps.clock)(),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-commit-summary:p1:inst-precompute-commit
        self.commit(&commit, delivery_number).await
    }

    /// Step 17: the commit and the mapping of its result.
    async fn commit(&self, commit: &SummaryCommit, delivery_number: i64) -> HandlerOutcome {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-commit-outcome
        match deps.store.commit(commit).await {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-committed
            Ok(CommitOutcome::Committed) => {
                deps.metrics.summary_execution(SummaryResult::Success);
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-committed
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-frontier-deleted
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Ok(CommitOutcome::FrontierDeleted) => {
                deps.metrics
                    .summary_execution(SummaryResult::FrontierDeleted);
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-frontier-deleted
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-base-missing
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Ok(CommitOutcome::BaseMissing) => {
                deps.metrics.summary_execution(SummaryResult::BaseMissing);
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-base-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-conflict
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Ok(CommitOutcome::CasConflict) => {
                deps.metrics.summary_cas_conflict();
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-conflict
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-error
            // @cpt-begin:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
            Err(error) => {
                tracing::warn!(chat_id = %commit.chat_id, %error, "the summary commit failed");
                self.retry(delivery_number, SummaryResult::Retry)
            } // @cpt-end:cpt-cf-mini-chat-state-background-workers-summary-frontier:p1:inst-summary-unchanged
              // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-case-commit-error
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-commit-outcome
    }

    /// Step 12: the summary model with the enabled filter, its provider entry
    /// and alias for the payload tenant.
    #[allow(clippy::cognitive_complexity)]
    async fn resolve_model(&self, ctx: &SecurityContext, tenant_id: Uuid) -> Resolution {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-resolve-summary-model
        let user = ctx.subject_id();
        let snapshot = with_timeout(&*deps.timer, POLICY_CALL_BOUND, async {
            let version = deps.policy.current_policy_version(user).await?;
            deps.policy
                .policy_snapshot(user, version.policy_version)
                .await
        })
        .await;
        let snapshot = match snapshot {
            Some(Ok(snapshot)) => snapshot,
            Some(Err(error)) => {
                tracing::warn!(%error, "the summary model could not be resolved");
                return Resolution::Failed;
            }
            None => {
                tracing::warn!("the summary model resolution timed out");
                return Resolution::Failed;
            }
        };
        let model_id = self.model_id();
        let Some(entry) = snapshot
            .model_catalog
            .into_iter()
            .find(|entry| entry.id == model_id && entry.enabled)
        else {
            return Resolution::Unavailable;
        };
        match deps.providers.resolve(&entry.provider_id, tenant_id) {
            Ok(target) => Resolution::Ready(Box::new(ResolvedModel { entry, target })),
            Err(error) => {
                tracing::warn!(%error, "the provider of the summary model does not resolve");
                Resolution::Failed
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-resolve-summary-model
    }

    /// Steps 14 of the handler and 1 to 4 of the call: the non-streaming call
    /// with up to [`MAX_CONTEXT_RETRIES`] context-length retries, bounded by the
    /// call deadline.
    #[allow(clippy::too_many_arguments)]
    async fn call_model(
        &self,
        ctx: &SecurityContext,
        model: &ResolvedModel,
        mut prompt: SummaryPrompt,
        existing: Option<&str>,
        tenant_id: Uuid,
        chat_id: Uuid,
        started: Duration,
    ) -> Called {
        let deps = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-call-summary
        let call_deadline = started
            + Duration::from_secs(deps.config.claim_timeout_secs)
                .saturating_sub(SUMMARY_COMMIT_RESERVE);
        let mut retries = 0;
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-call-summary
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-send-summary-request
            let request = SummaryRequest {
                target: model.target.clone(),
                entry: model.entry.clone(),
                system: prompt.system.clone(),
                user: prompt.user.clone(),
                tenant_id,
                chat_id,
            };
            let remaining = call_deadline.saturating_sub(deps.timer.now());
            let result = with_timeout(
                &*deps.timer,
                remaining,
                deps.client.summarize(ctx, &request),
            )
            .await;
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-send-summary-request
            match result {
                // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-return-summary-text
                Some(Ok(answer)) => return Called::Answer(answer),
                // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-return-summary-text
                Some(Err(SummaryCallError::ContextLength))
                    if retries < MAX_CONTEXT_RETRIES && prompt.kept.len() > 2 =>
                {
                    drop_oldest_fifth(&mut prompt.kept);
                    prompt = rebuild_prompt(
                        std::mem::take(&mut prompt.kept),
                        existing,
                        &prompt.system,
                        deps.config.message_content_limit,
                    );
                    retries += 1;
                }
                // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-return-summary-failure
                Some(Err(error)) => {
                    tracing::warn!(%chat_id, ?error, "the summary call failed");
                    return Called::ProviderFailure;
                }
                None => {
                    tracing::warn!(%chat_id, "the summary call hit its deadline");
                    return Called::ProviderFailure;
                } // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-return-summary-failure
            }
        }
    }
}
