//! Thread-summary outbox handler (DESIGN §3.6 "Thread Summary Update", §3.2 "System Task
//! Attribution Rules", B.5.5, B.9.4).
//!
//! Runs as a system task: never writes `chat_turns` nor `quota_usage`; commits the summary, the
//! frontier, the `is_compressed` range and a `system_task` usage event in one CAS-guarded transaction.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue, ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage, Wake};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::clock;
use crate::domain::context::{HistoryMessage, after, at_or_before, load_summary};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::domain::summary::SYSTEM_TASK_TYPE;
use crate::domain::summary::prompt;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::llm::responses::{
    ChatRequest, CompletionResult, InputItem, InputRole, ProviderFailure, RequestMetadata, build_request_body,
    parse_completion, provider_user_field,
};
use crate::infra::llm::transport::{HttpResponse, OutgoingBody};
use crate::infra::metrics;
use crate::infra::outbox::PAYLOAD_USAGE;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// Maximum number of context-length-exceeded retries (each drops ~20% of the oldest messages).
const MAX_PTL_RETRIES: usize = 2;

pub struct ThreadSummaryHandler {
    app: Arc<AppServices>,
}

impl ThreadSummaryHandler {
    #[must_use]
    pub fn new(app: Arc<AppServices>) -> Self {
        Self { app }
    }
}

#[async_trait::async_trait]
impl LeasedMessageHandler for ThreadSummaryHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let payload: ThreadSummaryPayload = match serde_json::from_slice(&msg.payload) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(error = %e, "thread summary: invalid payload");
                return MessageResult::Reject(format!("invalid thread summary payload: {e}"));
            }
        };
        let attempt = u32::try_from(i32::from(msg.attempts).max(0)).unwrap_or(0) + 1;
        let run = SummaryRun { app: &self.app, payload: &payload, attempt };
        run.execute().await
    }
}

fn record_execution(result: &str) {
    metrics::incr("mini_chat_thread_summary_execution", 1, &[("result", result.to_owned())]);
}

fn record_cas_conflict() {
    metrics::incr("mini_chat_thread_summary_cas_conflicts", 1, &[]);
}

/// Outcome of the commit transaction.
enum Commit {
    Done(Wake),
    FrontierDeleted,
    Conflict,
}

/// Outcome of the provider call.
enum CallError {
    /// Provider / transport failure (`provider_error`).
    Provider(String),
    /// Resolution failure before the call (`retry`).
    Resolution(String),
}

struct SummaryRun<'a> {
    app: &'a AppServices,
    payload: &'a ThreadSummaryPayload,
    attempt: u32,
}

impl SummaryRun<'_> {
    /// `Retry` until the attempt budget is exhausted, then `Reject`.
    fn retry_or_reject(&self, reason: &str) -> MessageResult {
        let max = self.app.cfg.thread_summary_worker.max_attempts.max(1);
        if self.attempt >= max {
            tracing::error!(
                chat_id = %self.payload.chat_id,
                attempt = self.attempt,
                reason,
                "thread summary: attempts exhausted, dead-lettering"
            );
            MessageResult::Reject(format!("thread summary failed after {} attempts: {reason}", self.attempt))
        } else {
            tracing::warn!(chat_id = %self.payload.chat_id, attempt = self.attempt, reason, "thread summary: retry");
            MessageResult::Retry
        }
    }

    fn base(&self) -> Option<(OffsetDateTime, Uuid)> {
        match (self.payload.base_frontier_created_at, self.payload.base_frontier_message_id) {
            (Some(t), Some(id)) => Some((t, id)),
            _ => None,
        }
    }

    fn target(&self) -> (OffsetDateTime, Uuid) {
        (self.payload.frozen_target_created_at, self.payload.frozen_target_message_id)
    }

    fn scope(&self) -> AccessScope {
        AccessScope::for_tenant(self.payload.tenant_id)
    }

    /// `retry` metric + `retry_or_reject`.
    fn retry(&self, reason: &str) -> MessageResult {
        record_execution("retry");
        self.retry_or_reject(reason)
    }

    async fn execute(&self) -> MessageResult {
        match self.run().await {
            Ok(r) | Err(r) => r,
        }
    }

    /// Steps of one attempt; `Err` carries an early handler result.
    async fn run(&self) -> Result<MessageResult, MessageResult> {
        let cfg = &self.app.cfg.thread_summary_worker;
        let model = self.resolve_model().await?;
        let (existing_summary, messages) = self.precheck_and_load().await?;
        let mut entries = prompt::entries(&messages, cfg.message_content_limit);
        if entries.is_empty() {
            tracing::debug!(chat_id = %self.payload.chat_id, "thread summary: nothing to summarize");
            return Ok(MessageResult::Ok);
        }
        let system = prompt::system_prompt(&model, cfg);
        let dropped = prompt::fit_entries(&mut entries, existing_summary.as_deref(), &system, &model);
        if dropped > 0 {
            tracing::info!(chat_id = %self.payload.chat_id, dropped, "thread summary: prompt fitted to the summary model budget");
        }
        let (summary, completion) = self.summarize(&model, &system, existing_summary.as_deref(), entries).await?;
        let estimate = prompt::token_estimate(completion.usage.as_ref(), &summary);
        Ok(self.finish(self.commit(&model, summary, estimate, completion.usage).await))
    }

    /// Summary model from the current snapshot (enabled filter). Missing/disabled → `Reject`.
    async fn resolve_model(&self) -> Result<ModelCatalogEntry, MessageResult> {
        let model_id = self.app.cfg.thread_summary_worker.effective_summary_model_id();
        let snapshot = self
            .app
            .policy
            .current_snapshot(DEFAULT_SUBJECT_ID)
            .await
            .map_err(|e| self.retry(&format!("policy snapshot: {e}")))?;
        snapshot.enabled_model(model_id).cloned().ok_or_else(|| {
            tracing::error!(model_id, "thread summary: summary model missing or disabled");
            record_execution("model_unavailable");
            MessageResult::Reject(format!("summary model {model_id} is missing or disabled"))
        })
    }

    /// Checks that the stored frontier still equals the base frontier and loads the frozen range.
    /// Returns the existing summary text and the messages of `(base, target]`.
    async fn precheck_and_load(&self) -> Result<(Option<String>, Vec<HistoryMessage>), MessageResult> {
        let p = self.payload;
        let conn = self.app.db.conn().map_err(|e| self.retry(&format!("db: {e}")))?;
        let current = load_summary(&conn, p.tenant_id, p.chat_id)
            .await
            .map_err(|e| self.retry(&format!("load summary: {e}")))?;
        let base = self.base();
        if base.is_some() && current.is_none() {
            tracing::info!(chat_id = %p.chat_id, "thread summary: base summary no longer exists");
            record_execution("base_missing");
            return Err(MessageResult::Ok);
        }
        let current_frontier = current.as_ref().map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
        if current_frontier != base {
            tracing::info!(chat_id = %p.chat_id, "thread summary: frontier already advanced (pre-check)");
            record_cas_conflict();
            return Err(MessageResult::Ok);
        }
        let messages = self.load_range(&conn).await.map_err(|e| self.retry(&format!("load messages: {e}")))?;
        Ok((current.map(|s| s.summary_text), messages))
    }

    /// Provider call + response parsing. Provider failure / empty summary → retry (previous
    /// summary kept).
    async fn summarize(
        &self,
        model: &ModelCatalogEntry,
        system: &str,
        existing_summary: Option<&str>,
        entries: Vec<String>,
    ) -> Result<(String, CompletionResult), MessageResult> {
        let completion = match self.call_with_ptl_retries(model, system, existing_summary, entries).await {
            Ok(c) => c,
            Err(CallError::Resolution(e)) => return Err(self.retry(&e)),
            Err(CallError::Provider(e)) => {
                tracing::warn!(chat_id = %self.payload.chat_id, error = %e, "thread summary: provider call failed; keeping previous summary");
                record_execution("provider_error");
                metrics::incr("mini_chat_summary_fallback", 1, &[]);
                return Err(self.retry_or_reject(&format!("provider error: {e}")));
            }
        };
        let summary = prompt::parse_summary(&completion.text);
        if summary.is_empty() {
            record_execution("empty_summary");
            return Err(self.retry_or_reject("empty summary"));
        }
        Ok((summary, completion))
    }

    /// Maps the commit outcome to the handler result.
    fn finish(&self, outcome: Result<Commit, DomainError>) -> MessageResult {
        let chat_id = self.payload.chat_id;
        // A concurrent first summary for the chat (UNIQUE chat_id) is a lost CAS.
        let outcome = match outcome {
            Err(e) if e.is_unique_violation() => Ok(Commit::Conflict),
            other => other,
        };
        match outcome {
            Ok(Commit::Done(wake)) => {
                wake.fire();
                record_execution("success");
                MessageResult::Ok
            }
            Ok(Commit::FrontierDeleted) => {
                tracing::info!(%chat_id, "thread summary: target message deleted; skipping commit");
                record_execution("frontier_deleted");
                MessageResult::Ok
            }
            Ok(Commit::Conflict) => {
                tracing::info!(%chat_id, "thread summary: CAS conflict at commit");
                record_cas_conflict();
                MessageResult::Ok
            }
            Err(e) => self.retry(&format!("commit: {e}")),
        }
    }

    async fn load_range(&self, runner: &impl toolkit_db::secure::DBRunner) -> Result<Vec<HistoryMessage>, DomainError> {
        use message::Column as M;
        let (tt, tid) = self.target();
        let mut cond = Condition::all()
            .add(M::ChatId.eq(self.payload.chat_id))
            .add(M::DeletedAt.is_null())
            .add(M::IsCompressed.eq(false))
            .add(at_or_before(M::CreatedAt, M::Id, tt, tid));
        if let Some((bt, bid)) = self.base() {
            cond = cond.add(after(M::CreatedAt, M::Id, bt, bid));
        }
        let rows = message::Entity::find()
            .secure()
            .scope_with(&self.scope())
            .filter(cond)
            .order_by(M::CreatedAt, Order::Asc)
            .order_by(M::Id, Order::Asc)
            .all(runner)
            .await?;
        Ok(rows
            .into_iter()
            .map(|m| HistoryMessage { id: m.id, role: m.role, content: m.content, created_at: m.created_at })
            .collect())
    }

    /// Calls the summary model; on a context-length-exceeded error retries up to twice, dropping ~20%
    /// of the oldest messages each time.
    async fn call_with_ptl_retries(
        &self,
        model: &ModelCatalogEntry,
        system: &str,
        existing_summary: Option<&str>,
        mut entries: Vec<String>,
    ) -> Result<CompletionResult, CallError> {
        let p = self.payload;
        let provider = self
            .app
            .providers
            .resolve(&model.provider_id, p.tenant_id)
            .map_err(|e| CallError::Resolution(format!("provider resolution: {e}")))?;
        let uri = provider.chat_uri(&model.provider_model_id);
        let ctx = SecurityContext::builder()
            .subject_id(DEFAULT_SUBJECT_ID)
            .subject_tenant_id(p.tenant_id)
            .build()
            .map_err(|e| CallError::Resolution(format!("security context: {e}")))?;

        let mut ptl_retries = 0;
        loop {
            let user_prompt = prompt::user_prompt(existing_summary, &entries);
            let req = ChatRequest {
                model: model.provider_model_id.clone(),
                instructions: system.to_owned(),
                input: vec![InputItem { role: InputRole::User, text: user_prompt, image_file_ids: Vec::new() }],
                max_output_tokens: i64::from(model.max_output_tokens),
                tools: Vec::new(),
                user: provider_user_field(p.tenant_id, DEFAULT_SUBJECT_ID),
                metadata: RequestMetadata {
                    tenant_id: p.tenant_id.to_string(),
                    user_id: DEFAULT_SUBJECT_ID.to_string(),
                    chat_id: p.chat_id.to_string(),
                    request_type: "summary".to_owned(),
                    feature: "none".to_owned(),
                },
                api_params: model.general_config.api_params.clone(),
                max_tool_calls: model.max_tool_calls,
                stream: false,
            };
            let body = build_request_body(provider.kind, &req);
            let resp = self
                .app
                .transport
                .request(&ctx, http::Method::POST, &uri, OutgoingBody::Json(body))
                .await
                .map_err(|e| CallError::Provider(e.to_string()))?;
            match parse_completion(provider.kind, &resp) {
                Ok(c) => return Ok(c),
                Err(f) => {
                    let step = prompt::drop_step(entries.len(), 1);
                    if is_context_length_exceeded(&resp, &f) && ptl_retries < MAX_PTL_RETRIES && step > 0 {
                        ptl_retries += 1;
                        tracing::info!(chat_id = %p.chat_id, dropped = step, "thread summary: prompt too long, dropping oldest messages");
                        entries.drain(..step);
                        continue;
                    }
                    return Err(CallError::Provider(format!("{}: {}", f.code, f.message)));
                }
            }
        }
    }

    async fn commit(
        &self,
        model: &ModelCatalogEntry,
        summary: String,
        estimate: i64,
        usage: Option<UsageTokens>,
    ) -> Result<Commit, DomainError> {
        let p = self.payload.clone();
        let base = self.base();
        let model_id = model.id.clone();
        let outbox = Arc::clone(&self.app.outbox);
        let estimate = i32::try_from(estimate).unwrap_or(i32::MAX);
        self.app
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    use message::Column as M;
                    use thread_summary::Column as S;
                    let scope = AccessScope::for_tenant(p.tenant_id);
                    let now = clock::now();

                    // The target frontier message must still be live. A no-op conditional update both
                    // checks it and locks the row (PostgreSQL), ordering this commit with a concurrent
                    // retry/edit/delete of the turn.
                    let locked = message::Entity::update_many()
                        .col_expr(M::IsCompressed, Expr::col(M::IsCompressed))
                        .filter(
                            Condition::all()
                                .add(M::Id.eq(p.frozen_target_message_id))
                                .add(M::ChatId.eq(p.chat_id))
                                .add(M::DeletedAt.is_null()),
                        )
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if locked.rows_affected == 0 {
                        return Ok(Commit::FrontierDeleted);
                    }

                    // CAS on the base frontier.
                    let current = load_summary(tx, p.tenant_id, p.chat_id).await?;
                    match (base, current) {
                        (None, None) => {
                            let am = thread_summary::ActiveModel {
                                id: ActiveValue::Set(Uuid::new_v4()),
                                tenant_id: ActiveValue::Set(p.tenant_id),
                                chat_id: ActiveValue::Set(p.chat_id),
                                summary_text: ActiveValue::Set(summary),
                                summarized_up_to_created_at: ActiveValue::Set(p.frozen_target_created_at),
                                summarized_up_to_message_id: ActiveValue::Set(p.frozen_target_message_id),
                                token_estimate: ActiveValue::Set(estimate),
                                created_at: ActiveValue::Set(now),
                                updated_at: ActiveValue::Set(now),
                            };
                            thread_summary::Entity::insert(am.clone())
                                .secure()
                                .scope_with_model(&scope, &am)?
                                .exec(tx)
                                .await?;
                        }
                        (Some((bt, bid)), Some(_)) => {
                            let res = thread_summary::Entity::update_many()
                                .col_expr(S::SummaryText, Expr::value(summary))
                                .col_expr(S::SummarizedUpToCreatedAt, Expr::value(p.frozen_target_created_at))
                                .col_expr(S::SummarizedUpToMessageId, Expr::value(p.frozen_target_message_id))
                                .col_expr(S::TokenEstimate, Expr::value(estimate))
                                .col_expr(S::UpdatedAt, Expr::value(now))
                                .filter(
                                    Condition::all()
                                        .add(S::ChatId.eq(p.chat_id))
                                        .add(S::SummarizedUpToCreatedAt.eq(bt))
                                        .add(S::SummarizedUpToMessageId.eq(bid)),
                                )
                                .secure()
                                .scope_with(&scope)
                                .exec(tx)
                                .await?;
                            if res.rows_affected == 0 {
                                return Ok(Commit::Conflict);
                            }
                        }
                        _ => return Ok(Commit::Conflict),
                    }

                    // Mark exactly the summarized range.
                    let mut range = Condition::all()
                        .add(M::ChatId.eq(p.chat_id))
                        .add(M::DeletedAt.is_null())
                        .add(M::IsCompressed.eq(false))
                        .add(at_or_before(M::CreatedAt, M::Id, p.frozen_target_created_at, p.frozen_target_message_id));
                    if let Some((bt, bid)) = base {
                        range = range.add(after(M::CreatedAt, M::Id, bt, bid));
                    }
                    message::Entity::update_many()
                        .col_expr(M::IsCompressed, Expr::value(true))
                        .filter(range)
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;

                    // System usage event (requester_type = system, zero credits).
                    let event = UsageEvent {
                        tenant_id: p.tenant_id,
                        user_id: None,
                        chat_id: p.chat_id,
                        turn_id: None,
                        request_id: p.system_request_id,
                        effective_model: model_id.clone(),
                        selected_model: model_id,
                        terminal_state: "completed".to_owned(),
                        billing_outcome: "system_task".to_owned(),
                        usage,
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
                            p.tenant_id.simple(),
                            p.system_request_id.simple()
                        ),
                        system_task_type: Some(SYSTEM_TASK_TYPE.to_owned()),
                    };
                    let wake = outbox.enqueue_json(tx, outbox.usage_queue(), p.tenant_id, PAYLOAD_USAGE, &event).await?;
                    Ok(Commit::Done(wake))
                })
            })
            .await
    }
}

/// `true` when the provider rejected the request because the prompt exceeds the context length.
fn is_context_length_exceeded(resp: &HttpResponse, failure: &ProviderFailure) -> bool {
    let body = resp.json();
    let code = body
        .pointer("/error/code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| body.pointer("/code").and_then(serde_json::Value::as_str))
        .unwrap_or_default();
    if code == "context_length_exceeded" || failure.code == "context_length_exceeded" {
        return true;
    }
    let msg = failure.message.to_ascii_lowercase();
    msg.contains("context_length_exceeded")
        || msg.contains("maximum context length")
        || msg.contains("context length")
        || msg.contains("prompt is too long")
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod tests;
