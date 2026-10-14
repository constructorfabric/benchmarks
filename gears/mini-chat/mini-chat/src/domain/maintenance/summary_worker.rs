//! The thread-summary handler
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-handler`) with the retry budget, the
//! summary model resolution and the commit step. [`ThreadSummaryWorker::handle`] decides one
//! leased message; `infra::thread_summary_handler` maps the outcome to the shared outbox.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, UsageTokens};
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::timing::Timer;
use crate::domain::maintenance::HandlerOutcome;
use crate::domain::maintenance::metrics::{
    RESULT_BASE_MISSING, RESULT_EMPTY_SUMMARY, RESULT_FRONTIER_DELETED, RESULT_MODEL_UNAVAILABLE,
    RESULT_PROVIDER_ERROR, RESULT_RETRY, RESULT_SUCCESS, SummaryMetrics,
};
use crate::domain::maintenance::settings::SummaryWorkerSettings;
use crate::domain::maintenance::summary_call::{ProviderCall, SummaryCaller, SummaryTransport};
use crate::domain::maintenance::summary_parse::{ParsedSummary, parse_summary_text};
use crate::domain::maintenance::summary_payload::parse_summary_payload;
use crate::domain::maintenance::summary_prompt::{RangeMessage, build_prompt};
use crate::domain::maintenance::summary_response::SummaryCompletion;
use crate::domain::maintenance::summary_store::{
    CommitOutcome, PreCheck, SummaryCommit, SummaryStore,
};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::provider_registry::{EffectiveProvider, ProviderRegistry};
use crate::domain::quota::usage_event::{SystemTaskUsageInput, build_system_task_usage_event};
use crate::domain::quota::{Clock, UsageQueue};
use crate::domain::streaming::summary::ThreadSummaryWork;

/// The collaborators of [`ThreadSummaryWorker`].
#[domain_model]
pub struct SummaryDeps {
    pub store: Arc<dyn SummaryStore>,
    /// The policy snapshot access (read without a user id: `Uuid::nil()`).
    pub policy: Arc<dyn ModelPolicyGateway>,
    pub providers: Arc<ProviderRegistry>,
    pub transport: Arc<dyn SummaryTransport>,
    /// The usage queue the commit transaction enqueues the system-task event on.
    pub usage: Arc<dyn UsageQueue>,
    pub clock: Arc<dyn Clock>,
    pub timer: Arc<dyn Timer>,
    pub metrics: Arc<dyn SummaryMetrics>,
    pub settings: SummaryWorkerSettings,
}

/// The retry budget: `Reject` with the reason `thread summary: max attempts (N) reached` on the
/// `max_attempts`-th delivery or a later one (`attempts` is 0-based, so `attempts + 1 >=
/// max_attempts`), `Retry` before.
#[must_use]
pub fn retry_budget(attempts: i16, max_attempts: u32) -> HandlerOutcome {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-last
    if i64::from(attempts) >= i64::from(max_attempts) - 1 {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-reject
        // the message is dead-lettered so a persistent failure does not block the other chats of
        // the partition; the frontier stays where it was
        HandlerOutcome::Reject(format!(
            "thread summary: max attempts ({max_attempts}) reached"
        ))
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-reject
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-last
    } else {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-else
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-retry
        // the shared outbox applies its lease-aware backoff capped at 30 s
        HandlerOutcome::Retry
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-retry
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-else
    }
}

/// The summary model of a delivery: its catalog entry and the provider resolved for the chat's
/// tenant.
#[domain_model]
struct ResolvedModel {
    model: ModelCatalogEntry,
    provider: EffectiveProvider,
}

/// Why the summary model could not be resolved.
#[domain_model]
enum ResolveError {
    /// The model is missing from the catalog or disabled.
    Unavailable,
    /// The policy snapshot or the provider of the tenant is unavailable.
    Failed,
}

/// The thread-summary handler logic.
#[domain_model]
pub struct ThreadSummaryWorker {
    deps: SummaryDeps,
}

impl ThreadSummaryWorker {
    /// Create the worker over its collaborators.
    #[must_use]
    pub fn new(deps: SummaryDeps) -> Self {
        Self { deps }
    }

    /// The outcome of a failed delivery: `Retry`, or `Reject` on the last allowed delivery.
    fn budget(&self, work: &ThreadSummaryWork, attempts: i16, last_result: &str) -> HandlerOutcome {
        let outcome = retry_budget(attempts, self.deps.settings.max_attempts);
        if matches!(outcome, HandlerOutcome::Reject(_)) {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-reject
            tracing::warn!(
                chat_id = %work.chat_id,
                system_request_id = %work.system_request_id,
                last_result,
                "the thread summary reached its last delivery and is dead-lettered"
            );
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-retry-budget:p1:inst-rb-return-reject
        }
        outcome
    }

    /// Record `result="retry"` and decide the retry budget of an infrastructure failure.
    fn infrastructure_failure(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
        stage: &str,
        error: &dyn std::fmt::Display,
    ) -> HandlerOutcome {
        // only the chat id, the request id and the stage are logged: no content, no prompt
        tracing::warn!(
            chat_id = %work.chat_id,
            system_request_id = %work.system_request_id,
            stage,
            %error,
            "the thread summary delivery failed on infrastructure"
        );
        self.deps.metrics.execution(RESULT_RETRY);
        self.budget(work, attempts, RESULT_RETRY)
    }

    /// Decide one leased message with its 0-based `attempts`.
    pub async fn handle(&self, payload: &[u8], attempts: i16) -> HandlerOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-no-content-logs
        // the logs of this handler name the chat id, the request id and the outcome only: never a
        // message, the prompt or the summary text
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-no-content-logs
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-parse
        let parsed = parse_summary_payload(payload);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-parse
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-malformed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-malformed
        let work = match parsed {
            Ok(work) => work,
            Err(reason) => {
                // no execution result is recorded: the allowlist has none for it
                tracing::error!(%reason, "the thread summary message is malformed");
                return HandlerOutcome::Reject(format!(
                    "malformed thread summary payload: {reason}"
                ));
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-malformed
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-malformed
        let base_summary = match self.ready(&work, attempts).await {
            Ok(base_summary) => base_summary,
            Err(outcome) => return outcome,
        };
        let messages = match self.range(&work, attempts).await {
            Ok(messages) => messages,
            Err(outcome) => return outcome,
        };
        let resolved = match self.resolve_model(&work, attempts).await {
            Ok(resolved) => resolved,
            Err(outcome) => return outcome,
        };
        let completion = match self
            .call_model(
                &work,
                attempts,
                &resolved,
                base_summary.as_deref(),
                &messages,
            )
            .await
        {
            Ok(completion) => completion,
            Err(outcome) => return outcome,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-parse-response
        let parsed = parse_summary_text(&completion.text, &completion.usage);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-parse-response
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-empty
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-empty
        let Some(parsed) = parsed else {
            // an empty summary is retried through the budget; it is not a provider failure, so
            // the fallback counter stays where it is
            self.deps.metrics.execution(RESULT_EMPTY_SUMMARY);
            return self.budget(&work, attempts, RESULT_EMPTY_SUMMARY);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-empty
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-empty
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return
        self.commit(&work, attempts, &resolved.model, parsed, completion.usage)
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return
    }

    /// The frontier pre-check; `Ok` carries the stored summary text of the base (`None` for an
    /// empty base), `Err` the outcome that ends the delivery.
    async fn ready(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
    ) -> Result<Option<String>, HandlerOutcome> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-precheck
        let checked = d.store.precheck(work).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-precheck
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-precheck-match
        match checked {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-ready
            Ok(PreCheck::Ready { base_summary }) => Ok(base_summary),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-ready
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-chat-gone
            // a deleted chat's content is not sent to the provider again
            Ok(PreCheck::ChatGone) => Err(HandlerOutcome::Ok),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-chat-gone
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-base-missing
            Ok(PreCheck::BaseMissing) => {
                d.metrics.execution(RESULT_BASE_MISSING);
                Err(HandlerOutcome::Ok)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-base-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-conflict
            Ok(PreCheck::FrontierMoved) => {
                d.metrics.cas_conflict();
                Err(HandlerOutcome::Ok)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-conflict
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-target-deleted
            Ok(PreCheck::TargetDeleted) => {
                d.metrics.execution(RESULT_FRONTIER_DELETED);
                Err(HandlerOutcome::Ok)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-target-deleted
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-precheck-db
            Err(error) => Err(self.infrastructure_failure(work, attempts, "pre-check", &error)),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-precheck-db
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-precheck-match
    }

    /// The frozen range; `Err` carries the outcome of a failed load or of an empty range.
    async fn range(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
    ) -> Result<Vec<RangeMessage>, HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range
        let loaded = self.deps.store.load_range(work).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-range-db
        let messages =
            loaded.map_err(|error| self.infrastructure_failure(work, attempts, "range", &error))?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-range-db
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range-db
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range-empty
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-range-empty
        if messages.is_empty() {
            // nothing to summarize: no provider call and no execution result
            return Err(HandlerOutcome::Ok);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-range-empty
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-range-empty
        Ok(messages)
    }

    /// The summary model resolution: the enabled catalog entry of the configured summary model
    /// and its provider for the chat's tenant. No quota preflight, no downgrade cascade, no tier
    /// selection and no kill-switch check: the summary is a system task (ADR-0008).
    async fn resolve(&self, tenant_id: Uuid) -> Result<ResolvedModel, ResolveError> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-model-id
        let model_id = d.settings.model_id();
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-model-id
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-snapshot
        // read without a user id: the handler runs outside a user scope
        let read = d.policy.current_snapshot(Uuid::nil()).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-snapshot
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-snapshot-failed
        let snapshot = read.map_err(|_| ResolveError::Failed)?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-snapshot-failed
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-lookup
        let entry = snapshot
            .model_catalog
            .iter()
            .find(|entry| entry.enabled && entry.id == model_id);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-lookup
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-unavailable
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-unavailable
        let Some(entry) = entry else {
            return Err(ResolveError::Unavailable);
        };
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-unavailable
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-unavailable
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-provider
        let provider = d.providers.resolve(&entry.provider_id, tenant_id);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-provider
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-provider-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-provider-failed
        let provider = provider.map_err(|_| ResolveError::Failed)?;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return-provider-failed
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-provider-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-catalog
        // per delivery the catalog fields `provider_model_id`, `provider_id`, `enabled`,
        // `context_window`, `max_output_tokens`, `max_input_tokens`,
        // `estimation_budgets.bytes_per_token_conservative` and `thread_summary_prompt` are read
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-configuration:p1:inst-cf-catalog
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-no-quota
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return
        Ok(ResolvedModel {
            model: entry.clone(),
            provider: provider.clone(),
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-return
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-model-resolution:p1:inst-mr-no-quota
    }

    /// Resolve the summary model; `Err` carries the outcome of a missing or disabled model and of
    /// a resolution error.
    async fn resolve_model(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
    ) -> Result<ResolvedModel, HandlerOutcome> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-model
        let resolved = self.resolve(work.tenant_id).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-model
        match resolved {
            Ok(resolved) => Ok(resolved),
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-model-unavailable
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-model-unavailable
            Err(ResolveError::Unavailable) => {
                tracing::error!(
                    chat_id = %work.chat_id,
                    system_request_id = %work.system_request_id,
                    model_id = %self.deps.settings.model_id(),
                    "the summary model is missing or disabled"
                );
                self.deps.metrics.execution(RESULT_MODEL_UNAVAILABLE);
                Err(HandlerOutcome::Reject(
                    "thread summary: the summary model is unavailable".to_owned(),
                ))
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-model-unavailable
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-model-unavailable
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-resolution-error
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-resolution-error
            Err(ResolveError::Failed) => Err(self.infrastructure_failure(
                work,
                attempts,
                "model resolution",
                &"the policy snapshot or the provider is unavailable",
            )),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-resolution-error
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-resolution-error
        }
    }

    /// Build the prompt and call the summary model; `Err` carries the outcome of a failed call.
    async fn call_model(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
        resolved: &ResolvedModel,
        base_summary: Option<&str>,
        messages: &[RangeMessage],
    ) -> Result<SummaryCompletion, HandlerOutcome> {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-prompt
        let prompt = build_prompt(&resolved.model, &d.settings, base_summary, messages);
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-prompt
        let caller = SummaryCaller {
            transport: Arc::clone(&d.transport),
            timer: Arc::clone(&d.timer),
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-call
        let attempt_result = caller
            .call(&ProviderCall {
                provider: &resolved.provider,
                model: &resolved.model,
                tenant_id: work.tenant_id,
                chat_id: work.chat_id,
                settings: &d.settings,
                base_summary,
                prompt: &prompt,
            })
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-call
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-call-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-call-failed
        attempt_result.map_err(|failure| {
            // the previous summary stays unchanged, no frontier moves and no message is marked
            // compressed; the failure holds a sanitized cause only
            tracing::warn!(
                chat_id = %work.chat_id,
                system_request_id = %work.system_request_id,
                ?failure,
                "the summary provider call failed"
            );
            d.metrics.execution(RESULT_PROVIDER_ERROR);
            d.metrics.fallback();
            self.budget(work, attempts, RESULT_PROVIDER_ERROR)
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-return-call-failed
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-call-failed
    }

    /// The compare-and-set commit with the system-task usage event, and the outcome of its result.
    async fn commit(
        &self,
        work: &ThreadSummaryWork,
        attempts: i16,
        model: &ModelCatalogEntry,
        parsed: ParsedSummary,
        usage: UsageTokens,
    ) -> HandlerOutcome {
        let d = &self.deps;
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-content
        // the summary text is customer content: it stays tenant-scoped and is never logged, never
        // placed in a metric label, an outbox payload or a reject reason, and the content of a
        // deleted chat is never sent to the provider again
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-retention:p1:inst-rt-content
        let now = d.clock.now();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-build
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-dedupe
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-stable-id
        // the request id is the payload's, unchanged on every retry and replay: the handler never
        // generates a new one; the usage event holds neither a user id nor a turn id
        let usage_event = build_system_task_usage_event(&SystemTaskUsageInput {
            tenant_id: work.tenant_id,
            chat_id: work.chat_id,
            summary_model: &model.id,
            usage,
            system_request_id: work.system_request_id,
            timestamp: now,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-stable-id
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-dedupe
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-system-task-usage:p1:inst-su-build
        let commit = SummaryCommit {
            work: work.clone(),
            summary_text: parsed.text,
            token_estimate: parsed.token_estimate,
            usage_event,
            now,
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-commit
        // duplicate provider calls for the same frozen range (retry, replay, lease expiry) are a
        // P1 operational side effect: correctness rests on the CAS commit, not on the partition
        // order
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-duplicates-accepted
        let committed = d.store.commit(&commit, Arc::clone(&d.usage)).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-duplicates-accepted
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-commit
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-commit-match
        match committed {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-committed
            Ok(CommitOutcome::Committed(wake)) => {
                // the usage outbox wake fires after the commit
                wake.fire();
                d.metrics.execution(RESULT_SUCCESS);
                tracing::info!(
                    chat_id = %work.chat_id,
                    system_request_id = %work.system_request_id,
                    "the thread summary is committed"
                );
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-committed
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-target-deleted
            Ok(CommitOutcome::TargetDeleted) => {
                d.metrics.execution(RESULT_FRONTIER_DELETED);
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-target-deleted
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-base-missing
            Ok(CommitOutcome::BaseMissing) => {
                d.metrics.execution(RESULT_BASE_MISSING);
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-base-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-conflict
            Ok(CommitOutcome::FrontierMoved) => {
                // no second commit attempt for this frozen range
                d.metrics.cas_conflict();
                HandlerOutcome::Ok
            }
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-conflict
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-db
            Err(error) => self.infrastructure_failure(work, attempts, "commit", &error),
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-case-commit-db
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-handler:p1:inst-sh-commit-match
    }
}

#[cfg(test)]
#[path = "summary_worker_tests.rs"]
mod summary_worker_tests;
