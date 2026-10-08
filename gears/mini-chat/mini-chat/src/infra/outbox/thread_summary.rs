//! Thread summary queue handler (DESIGN "Thread Summary Update", B.9.4): summarizes the frozen
//! range `(base_frontier, frozen_target]` of one chat with the summary model and commits the
//! result with a compare-and-set on the stored frontier.
//!
//! Outcomes (`mini_chat_thread_summary_execution_total{result}` in brackets):
//! - malformed payload: `Reject`;
//! - no S2S context yet (start-up): `Retry` [`retry`] that never becomes `Reject`;
//! - the stored summary the task was based on is gone: `Ok` [`base_missing`]; the frontier moved
//!   (at the pre-check or at the commit): `Ok` (`thread_summary_cas_conflicts`);
//! - the target message was deleted (found when the range is loaded, before any provider call,
//!   or at the commit): `Ok` [`frontier_deleted`];
//! - summary model missing from the catalog or disabled: `Reject` [`model_unavailable`]; other
//!   model or provider resolution failures and database errors: `Retry` [`retry`];
//! - the summary call failed (after up to two context-length retries with fewer messages):
//!   `Retry` [`provider_error`] (+ `summary_fallback`); an empty parsed summary: `Retry`
//!   [`empty_summary`];
//! - committed: `Ok` [`success`].
//!
//! The frontier checks run before the model is resolved, so a redelivery of a committed or
//! obsolete task ends `Ok` whatever the catalog says. A `Retry` on the
//! `thread_summary_worker.max_attempts`-th delivery becomes `Reject`.

use std::sync::Arc;

use async_trait::async_trait;
use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use opentelemetry::KeyValue;
use toolkit_db::DBProvider;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;

use super::payloads::ThreadSummaryTask;
use super::{OutboxEnqueuer, OutboxRecord};
use crate::api::state::AppServices;
use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, ThreadSummaryWorkerConfig};
use crate::domain::error::DomainError;
use crate::domain::thread_summary::{
    SYSTEM_TASK_TYPE, SummaryEntry, drop_step, fit, input_budget, parse_summary,
    resolve_summary_model, summary_model_id, token_estimate, user_prompt,
};
use crate::infra::db::repo::messages::{self as message_repo, Position};
use crate::infra::db::repo::thread_summaries::{self, NewSummary};
use crate::infra::db::ts::db_now;
use crate::infra::db::tx::write_tx_with_wakes;
use crate::infra::db::{MessageRole, entity::messages};
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::llm::{
    ChatTarget, CompletionResult, ContentPart, InputItem, LlmClient, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderResolver, ProviderUsage, RequestMetadata, Role,
    S2sContext,
};
use crate::metrics::Metrics;

/// Context-length retries of the summary call, each with ~20 % fewer messages.
const MAX_CONTEXT_RETRIES: usize = 2;

/// Generates and commits the summary of one frozen message range; see the module docs.
pub struct ThreadSummaryHandler {
    cfg: ThreadSummaryWorkerConfig,
    s2s: S2sContext,
    db: Arc<DBProvider<DomainError>>,
    policy: Arc<dyn PolicyGateway>,
    providers: Arc<ProviderResolver>,
    llm: Arc<LlmClient>,
    outbox: Arc<OutboxEnqueuer>,
    metrics: Arc<Metrics>,
}

/// Result of one delivery before the `max_attempts` cap.
enum Step {
    Ok,
    /// Counts toward `max_attempts`.
    Retry,
    /// Retry that never becomes `Reject` (the S2S context is not available yet).
    Defer,
    Reject(String),
}

/// What the commit transaction did.
enum Commit {
    Done,
    FrontierDeleted,
    Conflict,
}

/// Everything the summary call and the commit of one delivery need.
struct Work {
    base: Option<Position>,
    model: ModelCatalogEntry,
    target: ChatTarget,
    scope: AccessScope,
    existing: Option<String>,
    range: Vec<messages::Model>,
}

/// The stored state the task was frozen against, and the range to summarize.
enum Loaded {
    BaseMissing,
    Advanced,
    Range {
        existing: Option<String>,
        messages: Vec<messages::Model>,
    },
}

impl ThreadSummaryHandler {
    #[must_use]
    pub fn from_services(services: &AppServices) -> Self {
        Self {
            cfg: services.cfg.thread_summary_worker.clone(),
            s2s: services.s2s.clone(),
            db: Arc::clone(&services.db),
            policy: Arc::clone(&services.policy),
            providers: Arc::clone(&services.providers),
            llm: Arc::clone(&services.llm),
            outbox: Arc::clone(&services.outbox),
            metrics: Arc::clone(&services.metrics),
        }
    }

    fn count(&self, result: &'static str) {
        self.metrics
            .thread_summary_execution
            .add(1, &[KeyValue::new("result", result)]);
    }

    /// A `Retry` counted as `result = retry`, logging `err`.
    fn retry(&self, task: &ThreadSummaryTask, what: &str, err: &DomainError) -> Step {
        tracing::warn!(chat_id = %task.chat_id, error = %err, "thread summary {what} failed; retrying");
        self.count("retry");
        Step::Retry
    }

    fn cas_conflict(&self, task: &ThreadSummaryTask) -> Step {
        tracing::info!(chat_id = %task.chat_id, "thread summary frontier already advanced");
        self.metrics.thread_summary_cas_conflicts.add(1, &[]);
        Step::Ok
    }

    async fn process(&self, task: &ThreadSummaryTask) -> Step {
        let work = match self.prepare(task).await {
            Ok(work) => work,
            Err(step) => return step,
        };
        let (text, usage) = match self.generate(task, &work).await {
            Ok(generated) => generated,
            Err(step) => return step,
        };
        let committed = self
            .commit(&work.scope, task, work.base, &work.model, text, usage)
            .await;
        match committed {
            Ok(Commit::Done) => {
                self.count("success");
                Step::Ok
            }
            Ok(Commit::FrontierDeleted) => {
                tracing::info!(chat_id = %task.chat_id, "thread summary target deleted; commit skipped");
                self.count("frontier_deleted");
                Step::Ok
            }
            Ok(Commit::Conflict) | Err(DomainError::Conflict { .. }) => self.cas_conflict(task),
            Err(err) => self.retry(task, "commit", &err),
        }
    }

    /// Resolves the summary model and its provider, checks the base frontier and loads the
    /// range; `Err` carries the outcome of a task that ends here.
    async fn prepare(&self, task: &ThreadSummaryTask) -> Result<Work, Step> {
        let base =
            base_of(task).ok_or_else(|| Step::Reject("base frontier is half set".to_owned()))?;
        if let Err(err) = self.s2s.get() {
            // Start-up: the S2S exchange has not finished. Retried without costing an attempt.
            tracing::debug!(chat_id = %task.chat_id, error = %err, "thread summary deferred");
            self.count("retry");
            return Err(Step::Defer);
        }
        let scope = AccessScope::for_tenant(task.tenant_id);
        let (existing, range) = self.load_range(&scope, task, base).await?;
        let model = match self.summary_model().await {
            Ok(Some(model)) => model,
            Ok(None) => {
                tracing::error!(model = %self.model_id(), "thread summary model is missing or disabled");
                self.count("model_unavailable");
                return Err(Step::Reject(format!(
                    "summary model {} unavailable",
                    self.model_id()
                )));
            }
            Err(err) => return Err(self.retry(task, "model resolution", &err)),
        };
        let target = self
            .providers
            .chat_target(&model.provider_id, task.tenant_id)
            .map_err(|err| self.retry(task, "provider resolution", &err))?;
        Ok(Work {
            base,
            model,
            target,
            scope,
            existing,
            range,
        })
    }

    /// The existing summary and the range to summarize, after the base frontier check; `Err`
    /// ends the task (base gone, frontier advanced, target already deleted, database error).
    async fn load_range(
        &self,
        scope: &AccessScope,
        task: &ThreadSummaryTask,
        base: Option<Position>,
    ) -> Result<(Option<String>, Vec<messages::Model>), Step> {
        let loaded = self
            .load(scope, task, base)
            .await
            .map_err(|err| self.retry(task, "range load", &err))?;
        let (existing, range) = match loaded {
            Loaded::BaseMissing => {
                tracing::info!(chat_id = %task.chat_id, "thread summary base no longer exists");
                self.count("base_missing");
                return Err(Step::Ok);
            }
            Loaded::Advanced => return Err(self.cas_conflict(task)),
            Loaded::Range { existing, messages } => (existing, messages),
        };
        // The target ends a live range; without it the turn it belonged to was replaced.
        if range.last().map(|m| m.id) != Some(task.frozen_target_message_id) {
            tracing::info!(chat_id = %task.chat_id, "thread summary target deleted; nothing to summarize");
            self.count("frontier_deleted");
            return Err(Step::Ok);
        }
        Ok((existing, range))
    }

    /// The summary text (parsed, not empty) and the usage of the call.
    async fn generate(
        &self,
        task: &ThreadSummaryTask,
        work: &Work,
    ) -> Result<(String, Option<ProviderUsage>), Step> {
        let completion = self
            .summarize(
                &work.model,
                &work.target,
                task,
                work.existing.as_deref(),
                &work.range,
            )
            .await
            .map_err(|err| {
                tracing::warn!(chat_id = %task.chat_id, error = %err, "thread summary call failed");
                self.count("provider_error");
                self.metrics.summary_fallback.add(1, &[]);
                Step::Retry
            })?;
        let text = parse_summary(&completion.text);
        if text.is_empty() {
            tracing::warn!(chat_id = %task.chat_id, "thread summary response is empty");
            self.count("empty_summary");
            return Err(Step::Retry);
        }
        Ok((text, completion.usage))
    }

    fn model_id(&self) -> &str {
        summary_model_id(&self.cfg)
    }

    /// The enabled catalog entry of the summary model; `None` when missing or disabled.
    async fn summary_model(&self) -> Result<Option<ModelCatalogEntry>, DomainError> {
        resolve_summary_model(self.policy.as_ref(), &self.cfg).await
    }

    /// Checks the stored frontier against the task's base and loads the range to summarize.
    async fn load(
        &self,
        scope: &AccessScope,
        task: &ThreadSummaryTask,
        base: Option<Position>,
    ) -> Result<Loaded, DomainError> {
        let conn = self.db.conn()?;
        let stored = thread_summaries::find_for_chat(&conn, scope, task.chat_id).await?;
        let frontier = stored.as_ref().map(|s| Position {
            created_at: s.summarized_up_to_created_at,
            id: s.summarized_up_to_message_id,
        });
        if base.is_some() && stored.is_none() {
            return Ok(Loaded::BaseMissing);
        }
        if frontier != base {
            return Ok(Loaded::Advanced);
        }
        let messages =
            message_repo::summary_range(&conn, scope, task.chat_id, base, target_of(task)).await?;
        Ok(Loaded::Range {
            existing: stored.map(|s| s.summary_text),
            messages,
        })
    }

    /// The summary call, fitted to the model's input budget; a context-length error is retried
    /// up to [`MAX_CONTEXT_RETRIES`] times with the oldest ~20 % of the messages dropped.
    async fn summarize(
        &self,
        model: &ModelCatalogEntry,
        target: &ChatTarget,
        task: &ThreadSummaryTask,
        existing: Option<&str>,
        range: &[messages::Model],
    ) -> Result<CompletionResult, ProviderError> {
        let system = self.system_prompt(model);
        let entries: Vec<SummaryEntry> = range.iter().filter_map(entry).collect();
        let limit = self.cfg.message_content_limit;
        let budget = input_budget(
            model.context_window,
            model.max_output_tokens,
            model.max_input_tokens,
        );
        let mut start = fit(
            &system,
            existing,
            &entries,
            limit,
            budget,
            &model.estimation_budgets,
        );
        let adapter = self.llm.adapter(target.kind);
        let mut retries = 0;
        loop {
            let prompt = user_prompt(existing, &entries[start..], limit);
            let req = summary_request(model, &system, prompt, task);
            match adapter.complete(target, req).await {
                Err(err)
                    if err.kind == ProviderErrorKind::ContextLengthExceeded
                        && retries < MAX_CONTEXT_RETRIES
                        && drop_step(entries.len() - start) > 0 =>
                {
                    tracing::info!(chat_id = %task.chat_id, "thread summary prompt too long; dropping older messages");
                    start += drop_step(entries.len() - start);
                    retries += 1;
                }
                other => return other,
            }
        }
    }

    /// The catalog `thread_summary_prompt`, else the configured prompt, else the built-in one.
    fn system_prompt(&self, model: &ModelCatalogEntry) -> String {
        [
            model.thread_summary_prompt.as_str(),
            self.cfg.summary_system_prompt.as_str(),
        ]
        .into_iter()
        .find(|p| !p.is_empty())
        .unwrap_or(DEFAULT_SUMMARY_SYSTEM_PROMPT)
        .to_owned()
    }

    /// The compare-and-set commit: target still live (locked), frontier still `base`, summary
    /// saved, range compressed, system usage event enqueued.
    async fn commit(
        &self,
        scope: &AccessScope,
        task: &ThreadSummaryTask,
        base: Option<Position>,
        model: &ModelCatalogEntry,
        text: String,
        usage: Option<ProviderUsage>,
    ) -> Result<Commit, DomainError> {
        let now = db_now();
        let target = target_of(task);
        let summary = NewSummary {
            tenant_id: task.tenant_id,
            chat_id: task.chat_id,
            frontier: target,
            token_estimate: token_estimate(usage, &text),
            text,
            now,
        };
        let event = OutboxRecord::usage(&usage_event(task, &model.id, usage, now))?;
        let (scope, task, outbox) = (scope.clone(), task.clone(), Arc::clone(&self.outbox));
        write_tx_with_wakes(&self.db, move |tx, wakes| {
            let (scope, task, outbox) = (scope.clone(), task.clone(), Arc::clone(&outbox));
            let (summary, event) = (summary.clone(), event.clone());
            Box::pin(async move {
                if message_repo::lock_live(tx, &scope, task.chat_id, target.id)
                    .await?
                    .is_none()
                {
                    return Ok(Commit::FrontierDeleted);
                }
                if !thread_summaries::compare_and_set(tx, &scope, base, summary).await? {
                    return Ok(Commit::Conflict);
                }
                message_repo::mark_compressed(tx, &scope, task.chat_id, base, target).await?;
                wakes.add(outbox.enqueue(tx, event).await?);
                Ok(Commit::Done)
            })
        })
        .await
    }
}

/// The task's base frontier; `None` inside when there is none, `None` outside when only one of
/// its two fields is set.
#[allow(clippy::option_option)] // "no base" and "malformed base" are different outcomes
fn base_of(task: &ThreadSummaryTask) -> Option<Option<Position>> {
    match (task.base_frontier_created_at, task.base_frontier_message_id) {
        (Some(created_at), Some(id)) => Some(Some(Position { created_at, id })),
        (None, None) => Some(None),
        _ => None,
    }
}

fn target_of(task: &ThreadSummaryTask) -> Position {
    Position {
        created_at: task.frozen_target_created_at,
        id: task.frozen_target_message_id,
    }
}

/// A user or assistant message as a prompt entry (other roles are not summarized).
fn entry(m: &messages::Model) -> Option<SummaryEntry> {
    let role = match MessageRole::parse(&m.role)? {
        MessageRole::User => Role::User,
        MessageRole::Assistant => Role::Assistant,
        MessageRole::System => return None,
    };
    Some(SummaryEntry {
        role,
        content: m.content.clone(),
    })
}

/// The non-streaming summary request, made with the system identity.
fn summary_request(
    model: &ModelCatalogEntry,
    system: &str,
    prompt: String,
    task: &ThreadSummaryTask,
) -> ProviderRequest {
    ProviderRequest {
        model: model.provider_model_id.clone(),
        instructions: system.to_owned(),
        input: vec![InputItem::Message {
            role: Role::User,
            parts: vec![ContentPart::Text(prompt)],
        }],
        tools: Vec::new(),
        max_output_tokens: model.max_output_tokens,
        max_tool_calls: model.max_tool_calls,
        api_params: model.general_config.api_params.clone(),
        user: format!("{}{}", task.tenant_id.simple(), DEFAULT_SUBJECT_ID.simple()),
        metadata: RequestMetadata {
            tenant_id: task.tenant_id,
            user_id: DEFAULT_SUBJECT_ID,
            chat_id: Some(task.chat_id),
            request_type: "summary",
            feature: "none".to_owned(),
        },
        stream: false,
    }
}

/// The zero-credit usage event of the summary call (System Task Attribution Rules).
fn usage_event(
    task: &ThreadSummaryTask,
    model_id: &str,
    usage: Option<ProviderUsage>,
    now: time::OffsetDateTime,
) -> UsageEvent {
    UsageEvent {
        tenant_id: task.tenant_id,
        user_id: None,
        chat_id: task.chat_id,
        turn_id: None,
        request_id: task.system_request_id,
        effective_model: model_id.to_owned(),
        selected_model: model_id.to_owned(),
        terminal_state: "completed".to_owned(),
        billing_outcome: "system_task".to_owned(),
        usage: usage.map(UsageTokens::from),
        actual_credits_micro: 0,
        settlement_method: "none".to_owned(),
        policy_version_applied: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        file_search_calls: 0,
        timestamp: now,
        requester_type: "system".to_owned(),
        dedupe_key: format!(
            "{}/{SYSTEM_TASK_TYPE}/{}",
            task.tenant_id.simple(),
            task.system_request_id.simple()
        ),
        system_task_type: Some(SYSTEM_TASK_TYPE.to_owned()),
    }
}

#[async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let task: ThreadSummaryTask = match serde_json::from_slice(&msg.payload) {
            Ok(task) => task,
            Err(err) => {
                return MessageResult::Reject(format!("malformed thread summary payload: {err}"));
            }
        };
        match self.process(&task).await {
            Step::Ok => MessageResult::Ok,
            Step::Defer => MessageResult::Retry,
            Step::Reject(reason) => MessageResult::Reject(reason),
            Step::Retry => {
                let delivery = u32::try_from(msg.attempts).unwrap_or(0).saturating_add(1);
                if delivery >= self.cfg.max_attempts {
                    tracing::error!(chat_id = %task.chat_id, attempts = delivery,
                        "thread summary failed on its last attempt; dead-lettering");
                    MessageResult::Reject(format!(
                        "thread summary: max attempts ({}) reached",
                        self.cfg.max_attempts
                    ))
                } else {
                    MessageResult::Retry
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use toolkit_security::constants::DEFAULT_SUBJECT_ID;
    use uuid::Uuid;

    use super::*;
    use crate::config::MiniChatConfig;
    use crate::infra::db::entity::messages;
    use crate::infra::outbox::{
        LoggingAckHandler, QueueKind, THREAD_SUMMARY_PAYLOAD_TYPE, ThreadSummaryTask,
    };
    use crate::test_support::app::{TestApp, ctx, test_config};
    use crate::test_support::catalog::{standard_model, test_catalog};
    use crate::test_support::gateway::Responder;
    use crate::test_support::outbox::outbox_message;
    use crate::test_support::stream::{
        RESPONSES_PATH, SUMMARY_QUEUE, USAGE_QUEUE, answer, chat_calls, create_chat, frame,
        messages_of, script_provider, script_summary, seed_thread_summary, soft_delete_message,
        stream_uri, summary_calls, summary_response, thread_summary_of,
    };

    const SUMMARY_PROMPT: &str = "You summarize test conversations.";
    const PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

    /// Config with `gpt-standard` as summary model and [`SUMMARY_PROMPT`] as system prompt.
    fn summary_config() -> MiniChatConfig {
        let mut cfg = test_config();
        cfg.thread_summary_worker.summary_model_id = "gpt-standard".to_owned();
        cfg.thread_summary_worker.summary_system_prompt = SUMMARY_PROMPT.to_owned();
        cfg
    }

    /// An app whose thread summary queue only acknowledges: the tests drive the handler.
    async fn quiet_app(cfg: MiniChatConfig) -> TestApp {
        TestApp::builder()
            .config(cfg)
            .outbox_handler(QueueKind::ThreadSummary, Arc::new(LoggingAckHandler))
            .build()
            .await
    }

    struct Chat {
        tenant: Uuid,
        chat: Uuid,
    }

    /// A chat with the given turns (question, answer), sent through the stream pipeline.
    async fn chat_with_turns(app: &TestApp, model: Option<&str>, turns: &[(&str, &str)]) -> Chat {
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(app, &ctx(tenant, user), model).await;
        for (question, reply) in turns {
            script_provider(app, answer(&[reply], 10, 10));
            send(app, tenant, user, chat, question).await;
        }
        Chat { tenant, chat }
    }

    async fn send(
        app: &TestApp,
        tenant: Uuid,
        user: Uuid,
        chat: Uuid,
        content: &str,
    ) -> Vec<crate::test_support::app::SseFrame> {
        let frames = app
            .stream(
                "POST",
                &stream_uri(chat),
                &ctx(tenant, user),
                json!({ "content": content }),
            )
            .await
            .unwrap_or_else(|res| panic!("send rejected: {} {}", res.status, res.json));
        assert_eq!(
            frames.last().map(|f| f.event.as_str()),
            Some("done"),
            "{frames:?}"
        );
        frames
    }

    /// The task of summarizing `(base, target]`.
    fn task(
        c: &Chat,
        base: Option<&messages::Model>,
        target: &messages::Model,
    ) -> ThreadSummaryTask {
        ThreadSummaryTask {
            tenant_id: c.tenant,
            chat_id: c.chat,
            system_request_id: Uuid::new_v4(),
            base_frontier_created_at: base.map(|m| m.created_at),
            base_frontier_message_id: base.map(|m| m.id),
            frozen_target_created_at: target.created_at,
            frozen_target_message_id: target.id,
            system_task_type: "thread_summary_update".to_owned(),
        }
    }

    async fn deliver(app: &TestApp, task: &ThreadSummaryTask, attempts: i16) -> MessageResult {
        ThreadSummaryHandler::from_services(&app.services)
            .handle(&outbox_message(THREAD_SUMMARY_PAYLOAD_TYPE, task, attempts))
            .await
    }

    fn compressed(rows: &[messages::Model]) -> Vec<bool> {
        rows.iter().map(|m| m.is_compressed).collect()
    }

    fn system_usage(app: &TestApp) -> Vec<Value> {
        app.outbox_payloads(USAGE_QUEUE)
            .into_iter()
            .filter(|p| p["billing_outcome"] == "system_task")
            .collect()
    }

    fn input_text(body: &Value, idx: usize) -> String {
        body["input"][idx]["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("no text at input[{idx}] in {body}"))
            .to_owned()
    }

    /// The test catalog plus `gpt-tiny-ctx` (`context_window 4096`, `max_output_tokens 1024`,
    /// `max_input_tokens 3072`).
    fn tiny_catalog() -> Vec<mini_chat_sdk::ModelCatalogEntry> {
        let mut tiny = standard_model("gpt-tiny-ctx");
        tiny.context_window = 4096;
        tiny.max_output_tokens = 1024;
        tiny.max_input_tokens = 3072;
        let mut catalog = test_catalog();
        catalog.push(tiny);
        catalog
    }

    /// Marks the end of the tasks enqueued so far for one chat.
    const SENTINEL: Uuid = Uuid::max();

    /// Every thread summary task of `chat` enqueued before this call: a sentinel task for the
    /// same chat (same partition, delivered in order) is enqueued and waited for. Handlers end
    /// the sentinel at once (no stored summary to match it: frontier advanced, or no live target).
    async fn settled_summary_tasks(app: &TestApp, tenant: Uuid, chat: Uuid) -> Vec<Value> {
        let sentinel = ThreadSummaryTask {
            tenant_id: tenant,
            chat_id: chat,
            system_request_id: SENTINEL,
            base_frontier_created_at: None,
            base_frontier_message_id: None,
            frozen_target_created_at: time::OffsetDateTime::UNIX_EPOCH,
            frozen_target_message_id: Uuid::nil(),
            system_task_type: "thread_summary_update".to_owned(),
        };
        let rec = crate::infra::outbox::OutboxRecord::thread_summary(&sentinel).unwrap();
        let outbox = Arc::clone(&app.services.outbox);
        crate::infra::db::tx::write_tx_with_wakes(&app.services.db, move |tx, wakes| {
            let (outbox, rec) = (Arc::clone(&outbox), rec.clone());
            Box::pin(async move {
                wakes.add(outbox.enqueue(tx, rec).await?);
                Ok(())
            })
        })
        .await
        .unwrap();
        let is_sentinel = |p: &Value| p["system_request_id"] == SENTINEL.to_string();
        TestApp::wait_until("the sentinel task is delivered", || async {
            app.outbox_payloads(SUMMARY_QUEUE).iter().any(is_sentinel)
        })
        .await;
        app.outbox_payloads(SUMMARY_QUEUE)
            .into_iter()
            .take_while(|p| !is_sentinel(p))
            .filter(|p| p["chat_id"] == chat.to_string())
            .collect()
    }

    #[tokio::test]
    async fn long_chat_gets_summary_and_next_turn_uses_it() {
        let catalog = tiny_catalog();
        let mut cfg = summary_config();
        cfg.thread_summary_worker.message_content_limit = 50;
        let app = TestApp::builder()
            .config(cfg)
            .catalog(catalog)
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), Some("gpt-tiny-ctx")).await;
        let long_answer = "a".repeat(2000);
        script_provider(&app, answer(&[&long_answer], 10, 10));
        script_summary(
            &app,
            summary_response(
                "<analysis>notes</analysis>\n<summary>Earlier: four questions.</summary>",
                40,
                0,
            ),
        );

        // Turns 1-3 stay below 80 % of the 3072-token budget; turn 4 reaches it.
        for n in 1..=4 {
            send(&app, tenant, user, chat, &format!("question {n}")).await;
        }
        TestApp::wait_until("the summary is committed", || async {
            thread_summary_of(&app, chat).await.is_some()
        })
        .await;

        let tasks = app.outbox_payloads(SUMMARY_QUEUE);
        assert_eq!(tasks.len(), 1, "only turn 4 triggers: {tasks:?}");
        let rows = messages_of(&app, chat).await;
        assert_eq!(rows.len(), 8);
        let target = &rows[5]; // the answer of turn 3, the last message before turn 4
        assert_eq!(tasks[0]["frozen_target_message_id"], target.id.to_string());
        assert!(tasks[0]["base_frontier_message_id"].is_null());
        let system_request_id: Uuid = tasks[0]["system_request_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();

        let calls = summary_calls(&app);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call["instructions"], SUMMARY_PROMPT);
        assert_eq!(call["stream"], false);
        assert_eq!(call["model"], "gpt-standard");
        assert_eq!(call["max_output_tokens"], 4096);
        assert!(call.get("tools").is_none(), "{call}");
        assert_eq!(call["metadata"]["request_type"], "summary");
        assert_eq!(call["metadata"]["feature"], "none");
        assert_eq!(call["metadata"]["user_id"], DEFAULT_SUBJECT_ID.to_string());
        assert_eq!(call["metadata"]["chat_id"], chat.to_string());
        assert_eq!(
            call["user"],
            format!("{}{}", tenant.simple(), DEFAULT_SUBJECT_ID.simple())
        );
        let prompt = input_text(call, 0);
        let cut = format!("Assistant: {}...", "a".repeat(50));
        let expected_entries = [
            "User: question 1",
            &cut,
            "User: question 2",
            &cut,
            "User: question 3",
            &cut,
        ]
        .join("\n\n");
        assert!(
            prompt.starts_with(&format!(
                "Summarize the following conversation:\n\n{expected_entries}\n\nBefore providing your final summary, wrap your analysis in <analysis> tags."
            )),
            "{prompt}"
        );
        assert!(
            prompt.ends_with("Respond with an <analysis> block followed by a <summary> block."),
            "{prompt}"
        );

        let summary = thread_summary_of(&app, chat).await.unwrap();
        assert_eq!(summary.summary_text, "Earlier: four questions.");
        assert_eq!(summary.summarized_up_to_message_id, target.id);
        assert_eq!(summary.summarized_up_to_created_at, target.created_at);
        assert_eq!(summary.token_estimate, 40);
        assert_eq!(
            compressed(&rows),
            [true, true, true, true, true, true, false, false]
        );

        TestApp::wait_until("the system usage event is delivered", || async {
            !system_usage(&app).is_empty()
        })
        .await;
        let usage = system_usage(&app);
        assert_eq!(usage.len(), 1, "{usage:?}");
        let ev = &usage[0];
        assert!(
            ev.get("user_id").is_none() && ev.get("turn_id").is_none(),
            "{ev}"
        );
        assert_eq!(ev["tenant_id"], tenant.to_string());
        assert_eq!(ev["chat_id"], chat.to_string());
        assert_eq!(ev["request_id"], system_request_id.to_string());
        assert_eq!(ev["requester_type"], "system");
        assert_eq!(ev["settlement_method"], "none");
        assert_eq!(ev["actual_credits_micro"], 0);
        assert_eq!(ev["terminal_state"], "completed");
        assert_eq!(ev["policy_version_applied"], 0);
        assert_eq!(ev["system_task_type"], "thread_summary_update");
        assert_eq!(ev["effective_model"], "gpt-standard");
        assert_eq!(ev["selected_model"], "gpt-standard");
        assert_eq!(ev["usage"]["output_tokens"], 40);
        assert_eq!(
            ev["dedupe_key"],
            format!(
                "{}/thread_summary_update/{}",
                tenant.simple(),
                system_request_id.simple()
            )
        );

        // A redelivery of the committed task finds the frontier advanced: no call, no change.
        let task: ThreadSummaryTask = serde_json::from_value(tasks[0].clone()).unwrap();
        assert!(matches!(deliver(&app, &task, 1).await, MessageResult::Ok));
        assert_eq!(summary_calls(&app).len(), 1);
        assert_eq!(thread_summary_of(&app, chat).await.unwrap(), summary);

        // The next turn starts from the summary and the uncompressed turn 4.
        let frames = send(&app, tenant, user, chat, "question 5").await;
        let started = &frame(&frames, "stream_started").data;
        assert_eq!(started["thread_summary_applied"]["token_estimate"], 40);
        let chats = chat_calls(&app);
        let last = chats.last().unwrap();
        assert_eq!(
            input_text(last, 0),
            format!("{PREAMBLE}\n\nEarlier: four questions.")
        );
        assert_eq!(input_text(last, 1), "question 4");
        assert_eq!(input_text(last, 3), "question 5");
        assert_eq!(last["input"].as_array().unwrap().len(), 4, "{last}");
        // A summary exists and nothing was truncated: turn 5 scheduled nothing.
        assert_eq!(settled_summary_tasks(&app, tenant, chat).await.len(), 1);
    }

    #[tokio::test]
    async fn disabled_worker_schedules_nothing() {
        let mut cfg = summary_config();
        cfg.thread_summary_worker.enabled = false;
        let app = TestApp::builder()
            .config(cfg)
            .catalog(tiny_catalog())
            .outbox_handler(QueueKind::ThreadSummary, Arc::new(LoggingAckHandler))
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let chat = create_chat(&app, &ctx(tenant, user), Some("gpt-tiny-ctx")).await;
        script_provider(&app, answer(&[&"a".repeat(2000)], 10, 10));
        // Turn 4 would trigger (see `long_chat_gets_summary_and_next_turn_uses_it`).
        for n in 1..=4 {
            send(&app, tenant, user, chat, &format!("question {n}")).await;
        }
        assert!(settled_summary_tasks(&app, tenant, chat).await.is_empty());
    }

    #[tokio::test]
    async fn existing_summary_is_merged_and_frontier_advanced() {
        let app = quiet_app(summary_config()).await;
        let c = chat_with_turns(&app, None, &[("q1", "a1"), ("q2", "a2"), ("q3", "a3")]).await;
        let rows = messages_of(&app, c.chat).await;

        // No stored summary although the task has a base: nothing to merge into.
        let orphan = task(&c, Some(&rows[1]), &rows[3]);
        assert!(matches!(deliver(&app, &orphan, 0).await, MessageResult::Ok));
        assert!(summary_calls(&app).is_empty());

        seed_thread_summary(&app, &rows[1]).await; // "earlier conversation", up to a1
        script_summary(&app, summary_response("<summary>Merged.</summary>", 0, 0));
        let t = task(&c, Some(&rows[1]), &rows[3]);
        assert!(matches!(deliver(&app, &t, 0).await, MessageResult::Ok));

        let calls = summary_calls(&app);
        assert_eq!(calls.len(), 1);
        let prompt = input_text(&calls[0], 0);
        assert!(
            prompt.starts_with(
                "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise."
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains(
                "unboundedly.\n\n<existing_summary>\nearlier conversation\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: q2\n\nAssistant: a2\n\nBefore providing"
            ),
            "{prompt}"
        );
        let summary = thread_summary_of(&app, c.chat).await.unwrap();
        assert_eq!(summary.summary_text, "Merged.");
        assert_eq!(summary.summarized_up_to_message_id, rows[3].id);
        // No usable output token count: ceil(7 bytes / 4).
        assert_eq!(summary.token_estimate, 2);
        // Only the new range is marked; the seeded summary's range was never compressed here.
        assert_eq!(
            compressed(&messages_of(&app, c.chat).await),
            [false, false, true, true, false, false]
        );

        // The same task again, with the summary model disabled meanwhile: the frontier moved
        // past its base, so it ends `Ok` before the model is looked at.
        let mut catalog = test_catalog();
        catalog[1].enabled = false; // gpt-standard
        app.usage.set_catalog(catalog);
        assert!(matches!(deliver(&app, &t, 2).await, MessageResult::Ok));
        assert_eq!(summary_calls(&app).len(), 1);
    }

    #[tokio::test]
    async fn summary_provider_failure_keeps_previous_state() {
        let app = quiet_app(summary_config()).await;
        let c = chat_with_turns(&app, None, &[("q1", "a1"), ("q2", "a2")]).await;
        let rows = messages_of(&app, c.chat).await;
        app.gateway.on(
            http::Method::POST,
            RESPONSES_PATH,
            Responder::json(500, json!({"error": {"message": "upstream exploded"}})),
        );
        let t = task(&c, None, &rows[1]);

        assert!(matches!(deliver(&app, &t, 0).await, MessageResult::Retry));
        assert_eq!(summary_calls(&app).len(), 1);
        assert!(thread_summary_of(&app, c.chat).await.is_none());
        assert_eq!(compressed(&messages_of(&app, c.chat).await), [false; 4]);

        // The delivery that reaches `max_attempts` (3) is rejected instead of retried.
        assert!(matches!(deliver(&app, &t, 1).await, MessageResult::Retry));
        assert!(matches!(
            deliver(&app, &t, 2).await,
            MessageResult::Reject(_)
        ));
        assert!(thread_summary_of(&app, c.chat).await.is_none());
        assert_eq!(compressed(&messages_of(&app, c.chat).await), [false; 4]);
        assert!(system_usage(&app).is_empty());
    }

    #[tokio::test]
    async fn frontier_deleted_skips_commit() {
        let app = quiet_app(summary_config()).await;
        let c = chat_with_turns(&app, None, &[("q1", "a1"), ("q2", "a2")]).await;
        let rows = messages_of(&app, c.chat).await;
        // Slow enough for the test to delete the target while the call is in flight.
        script_summary(
            &app,
            Responder::Delayed(
                Duration::from_millis(300),
                Box::new(summary_response("<summary>S</summary>", 5, 0)),
            ),
        );

        // Deleted while the summary call runs: generated, but the commit is skipped.
        let t = task(&c, None, &rows[1]);
        let handler = ThreadSummaryHandler::from_services(&app.services);
        let msg = outbox_message(THREAD_SUMMARY_PAYLOAD_TYPE, &t, 0);
        let delivery = tokio::spawn(async move { handler.handle(&msg).await });
        TestApp::wait_until("the summary call is in flight", || async {
            summary_calls(&app).len() == 1
        })
        .await;
        soft_delete_message(&app, rows[1].id).await;
        assert!(matches!(delivery.await.unwrap(), MessageResult::Ok));
        assert!(thread_summary_of(&app, c.chat).await.is_none());
        assert_eq!(compressed(&messages_of(&app, c.chat).await), [false; 4]);
        assert!(system_usage(&app).is_empty());

        // Already deleted when the task runs: no provider call at all.
        soft_delete_message(&app, rows[3].id).await;
        let t = task(&c, None, &rows[3]);
        assert!(matches!(deliver(&app, &t, 0).await, MessageResult::Ok));
        assert_eq!(summary_calls(&app).len(), 1, "no new call");
        assert!(thread_summary_of(&app, c.chat).await.is_none());
    }

    #[tokio::test]
    async fn missing_s2s_context_defers_without_dead_lettering() {
        let app = quiet_app(summary_config()).await;
        let c = chat_with_turns(&app, None, &[("q1", "a1")]).await;
        let rows = messages_of(&app, c.chat).await;
        script_summary(&app, summary_response("<summary>S</summary>", 5, 0));
        let mut handler = ThreadSummaryHandler::from_services(&app.services);
        handler.s2s = S2sContext::new();
        let t = task(&c, None, &rows[1]);

        // Even the `max_attempts`-th delivery stays a retry.
        for attempts in [0, 2, 5] {
            let msg = outbox_message(THREAD_SUMMARY_PAYLOAD_TYPE, &t, attempts);
            let result = handler.handle(&msg).await;
            assert!(
                matches!(result, MessageResult::Retry),
                "{attempts}: {result:?}"
            );
        }
        assert!(summary_calls(&app).is_empty());
        assert!(thread_summary_of(&app, c.chat).await.is_none());
    }

    #[tokio::test]
    async fn disabled_summary_model_rejected() {
        for model in ["gpt-disabled", "gpt-not-in-catalog"] {
            let mut cfg = summary_config();
            cfg.thread_summary_worker.summary_model_id = model.to_owned();
            let app = quiet_app(cfg).await;
            let c = chat_with_turns(&app, None, &[("q1", "a1")]).await;
            let rows = messages_of(&app, c.chat).await;
            script_summary(&app, summary_response("<summary>S</summary>", 5, 0));

            let result = deliver(&app, &task(&c, None, &rows[1]), 0).await;
            assert!(
                matches!(result, MessageResult::Reject(_)),
                "{model}: {result:?}"
            );
            assert!(summary_calls(&app).is_empty(), "{model}");
            assert!(thread_summary_of(&app, c.chat).await.is_none(), "{model}");
        }
    }

    #[tokio::test]
    async fn context_length_error_retries_with_fewer_messages() {
        let app = quiet_app(summary_config()).await;
        let turns: Vec<(String, String)> = (1..=5)
            .map(|n| (format!("q{n}"), format!("a{n}")))
            .collect();
        let turns: Vec<(&str, &str)> = turns
            .iter()
            .map(|(q, a)| (q.as_str(), a.as_str()))
            .collect();
        let c = chat_with_turns(&app, None, &turns).await;
        let rows = messages_of(&app, c.chat).await;
        let too_long = Responder::json(
            400,
            json!({"error": {"code": "context_length_exceeded", "message": "too long"}}),
        );
        app.gateway.on_sequence_matching(
            http::Method::POST,
            RESPONSES_PATH,
            Arc::new(crate::test_support::stream::is_summary_call),
            vec![
                too_long.clone(),
                too_long,
                summary_response("<summary>S</summary>", 5, 0),
            ],
        );

        let t = task(&c, None, &rows[9]);
        assert!(matches!(deliver(&app, &t, 0).await, MessageResult::Ok));
        // 10 messages; each retry drops the oldest ceil(n/5): 10 -> 8 -> 6 (q1, q2 turns gone).
        let first_user = |call: &Value| {
            let prompt = input_text(call, 0);
            let start = prompt.find("User: ").unwrap();
            prompt[start..start + 8].to_owned()
        };
        let calls = summary_calls(&app);
        assert_eq!(calls.len(), 3);
        assert_eq!(
            calls.iter().map(first_user).collect::<Vec<_>>(),
            ["User: q1", "User: q2", "User: q3"]
        );
        let summary = thread_summary_of(&app, c.chat).await.unwrap();
        assert_eq!(summary.summarized_up_to_message_id, rows[9].id);
        // The whole frozen range is covered by the committed frontier.
        assert_eq!(compressed(&messages_of(&app, c.chat).await), [true; 10]);
    }
}
