//! CAS-guarded turn finalization, settlement and outbox emission (OWNER: streaming core).
//!
//! DESIGN §5.7–5.9: one transaction per terminal outcome performs the CAS
//! (`UPDATE chat_turns ... WHERE id AND state = 'running'`), the quota settlement and the
//! usage / audit outbox enqueue (plus the assistant message and the thread-summary task on
//! completion). A lost CAS writes nothing. Wakes are fired after the commit.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::UsageTokens;
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbError;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, stream_codes};
use crate::domain::service::Deps;
use crate::domain::service::billing::{
    TerminalState, TurnAuditInput, UsageEventInput, build_turn_audit_event, build_usage_event,
    derive_billing,
};
use crate::domain::service::quota::{
    DowngradeReason, QuotaPeriods, QuotaService, SettlementInput, SettlementMethod,
};
use crate::domain::service::summary::{self, SummaryTrigger};
use crate::infra::db::entity::{chat_turn, message};
use crate::infra::llm::LlmCompletion;
use crate::infra::llm::sanitize::sanitize_provider_message;

/// Immutable facts about a running turn needed by its finalization.
#[derive(Debug, Clone)]
pub struct TurnContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// Effective model id of the preflight.
    pub effective_model: String,
    pub downgrade_reason: Option<DowngradeReason>,
    /// Preflight periods (settlement buckets).
    pub periods: QuotaPeriods,
    pub started: Instant,
    /// Thread-summary trigger inputs (completed turns only; `None` when disabled).
    pub summary_trigger: Option<SummaryTrigger>,
}

/// Completed tool calls of the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolCounters {
    pub web_search: u32,
    pub code_interpreter: u32,
    /// Completed file searches: provider-native `file_search` done events and successful
    /// `search_knowledge` retrievals (`chat_turns.file_search_completed_count`).
    pub file_search: u32,
    /// In-memory count of `search_knowledge` calls whose retrieval ran (incremented before
    /// the retrieval, so failed retrievals are included).
    pub knowledge_search_calls: u32,
}

impl ToolCounters {
    /// `file_search_calls` of the usage / audit events: the `search_knowledge` call count
    /// when knowledge search ran, otherwise the provider-native `file_search` count (the two
    /// tools are never sent in the same request).
    #[must_use]
    pub const fn file_search_calls(&self) -> u32 {
        if self.knowledge_search_calls > 0 {
            self.knowledge_search_calls
        } else {
            self.file_search
        }
    }
}

/// Terminal outcome reported by the provider task.
#[derive(Debug, Clone)]
pub enum Terminal {
    Completed {
        assistant_message_id: Uuid,
        /// Accumulated text deltas (only; the completion's output text is not used).
        text: String,
        completion: LlmCompletion,
    },
    Failed {
        code: String,
        /// Sanitized diagnostic stored in `error_detail`.
        detail: String,
        usage: Option<UsageTokens>,
        response_id: Option<String>,
    },
    Cancelled {
        assistant_message_id: Uuid,
        /// Partial text (persisted best-effort when non-empty).
        text: String,
    },
}

/// Result of a finalization attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeOutcome {
    /// This finalizer won the CAS; the committed state and error code.
    Committed {
        state: TerminalState,
        error_code: Option<String>,
    },
    /// The turn was no longer running; nothing was written.
    Lost,
}

/// Error of a finalization transaction (distinguishes the assistant message insert).
#[derive(Debug)]
enum TxError {
    Message(DomainError),
    Other(DomainError),
}

impl From<DbError> for TxError {
    fn from(e: DbError) -> Self {
        Self::Other(e.into())
    }
}

impl From<DomainError> for TxError {
    fn from(e: DomainError) -> Self {
        Self::Other(e)
    }
}

/// Builds an assistant `messages` row.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn assistant_message(
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    content: String,
    model: &str,
    usage: Option<UsageTokens>,
    provider_response_id: Option<String>,
) -> message::ActiveModel {
    let u = usage.unwrap_or_default();
    message::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set("assistant".to_owned()),
        content: Set(content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(provider_response_id),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(u.input_tokens),
        output_tokens: Set(u.output_tokens),
        cache_read_input_tokens: Set(u.cache_read_input_tokens),
        cache_write_input_tokens: Set(u.cache_write_input_tokens),
        reasoning_tokens: Set(u.reasoning_tokens),
        model: Set(Some(model.to_owned())),
        is_compressed: Set(false),
        created_at: Set(OffsetDateTime::now_utc()),
        deleted_at: Set(None),
    }
}

/// Builds a user `messages` row.
#[must_use]
pub fn user_message(
    tenant_id: Uuid,
    chat_id: Uuid,
    id: Uuid,
    request_id: Uuid,
    content: String,
    created_at: OffsetDateTime,
) -> message::ActiveModel {
    message::ActiveModel {
        id: Set(id),
        tenant_id: Set(tenant_id),
        chat_id: Set(chat_id),
        request_id: Set(Some(request_id)),
        role: Set("user".to_owned()),
        content: Set(content),
        content_type: Set("text".to_owned()),
        token_estimate: Set(0),
        provider_response_id: Set(None),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(0),
        output_tokens: Set(0),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(None),
        is_compressed: Set(false),
        created_at: Set(created_at),
        deleted_at: Set(None),
    }
}

/// One finalization transaction shape.
#[derive(Debug, Clone)]
struct TxPlan {
    state: TerminalState,
    error_code: Option<String>,
    error_detail: Option<String>,
    /// Assistant message to insert first (completed / cancelled with text).
    message: Option<(Uuid, String, Option<UsageTokens>, Option<String>)>,
    /// Message insert failure is fatal (completed) or ignored by a retry without it (cancelled).
    usage: Option<UsageTokens>,
    provider_response_id: Option<String>,
}

/// Finalizes a running turn (stream paths: completed / failed / cancelled).
///
/// # Errors
/// The finalization transaction failed (the turn stays `running`).
pub async fn finalize(
    deps: &Arc<Deps>,
    quota: &Arc<QuotaService>,
    turn: &TurnContext,
    counters: ToolCounters,
    terminal: Terminal,
) -> Result<FinalizeOutcome, DomainError> {
    match terminal {
        Terminal::Completed {
            assistant_message_id,
            text,
            completion,
        } => {
            if let Some(reason) = &completion.incomplete_reason {
                tracing::warn!(turn_id = %turn.turn_id, reason = %reason, "stream incomplete");
            }
            let plan = TxPlan {
                state: TerminalState::Completed,
                error_code: None,
                error_detail: None,
                message: Some((
                    assistant_message_id,
                    text,
                    completion.usage,
                    completion.response_id.clone(),
                )),
                usage: completion.usage,
                provider_response_id: completion.response_id.clone(),
            };
            match run_tx(deps, quota, turn, counters, plan).await {
                Ok(o) => Ok(o),
                Err(TxError::Message(e)) => {
                    tracing::error!(turn_id = %turn.turn_id, error = %e, "assistant message persistence failed");
                    let plan = TxPlan {
                        state: TerminalState::Failed,
                        error_code: Some(stream_codes::MESSAGE_PERSISTENCE_FAILED.to_owned()),
                        error_detail: Some("assistant message could not be persisted".to_owned()),
                        message: None,
                        usage: completion.usage,
                        provider_response_id: completion.response_id,
                    };
                    run_tx(deps, quota, turn, counters, plan)
                        .await
                        .map_err(tx_err)
                }
                Err(TxError::Other(e)) => Err(e),
            }
        }
        Terminal::Failed {
            code,
            detail,
            usage,
            response_id,
        } => {
            let plan = TxPlan {
                state: TerminalState::Failed,
                error_code: Some(code),
                error_detail: Some(sanitize_provider_message(&detail)),
                message: None,
                usage,
                provider_response_id: response_id,
            };
            run_tx(deps, quota, turn, counters, plan)
                .await
                .map_err(tx_err)
        }
        Terminal::Cancelled {
            assistant_message_id,
            text,
        } => {
            let with_message = !text.is_empty();
            let plan = TxPlan {
                state: TerminalState::Cancelled,
                error_code: None,
                error_detail: None,
                message: with_message.then(|| (assistant_message_id, text, None, None)),
                usage: None,
                provider_response_id: None,
            };
            match run_tx(deps, quota, turn, counters, plan.clone()).await {
                Ok(o) => Ok(o),
                Err(TxError::Message(e)) => {
                    tracing::warn!(turn_id = %turn.turn_id, error = %e, "partial assistant message not persisted");
                    let plan = TxPlan {
                        message: None,
                        ..plan
                    };
                    run_tx(deps, quota, turn, counters, plan)
                        .await
                        .map_err(tx_err)
                }
                Err(TxError::Other(e)) => Err(e),
            }
        }
    }
}

fn tx_err(e: TxError) -> DomainError {
    match e {
        TxError::Message(e) | TxError::Other(e) => e,
    }
}

fn i32_of(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

/// True for `SQLite` lock contention (busy, or a shared-cache table lock) reported by the
/// driver. Such a transaction was rolled back and can be retried as a whole.
#[must_use]
pub fn is_lock_contention(e: &DomainError) -> bool {
    matches!(e, DomainError::Internal(m) if m.contains("database is locked")
        || m.contains("database is deadlocked")
        || m.contains("(code: 5)")
        || m.contains("(code: 6)")
        || m.contains("(code: 517)"))
}

/// Maximum attempts of a transaction that failed on lock contention.
const LOCK_RETRY_ATTEMPTS: u32 = 8;

fn lock_backoff(attempt: u32) -> Duration {
    Duration::from_millis(u64::from(attempt) * 15)
}

/// Runs a whole transaction again when it failed on lock contention (`SQLite`).
///
/// # Errors
/// The last error of `f`.
pub async fn retry_locked<T, F, Fut>(mut f: F) -> Result<T, DomainError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DomainError>>,
{
    let mut attempt = 0;
    loop {
        match f().await {
            Err(e) if is_lock_contention(&e) && attempt + 1 < LOCK_RETRY_ATTEMPTS => {
                attempt += 1;
                tracing::debug!(attempt, error = %e, "transaction lock contention; retrying");
                tokio::time::sleep(lock_backoff(attempt)).await;
            }
            other => return other,
        }
    }
}

async fn run_tx(
    deps: &Arc<Deps>,
    quota: &Arc<QuotaService>,
    turn: &TurnContext,
    counters: ToolCounters,
    plan: TxPlan,
) -> Result<FinalizeOutcome, TxError> {
    let mut attempt = 0;
    let res = loop {
        let deps2 = Arc::clone(deps);
        let quota = Arc::clone(quota);
        let turn2 = turn.clone();
        let plan = plan.clone();
        let res = deps
            .db
            .db()
            .transaction_ref_mapped::<_, _, TxError>(move |tx| {
                Box::pin(
                    async move { finalize_in_tx(tx, &deps2, &quota, &turn2, counters, plan).await },
                )
            })
            .await;
        match res {
            Err(TxError::Message(e) | TxError::Other(e))
                if is_lock_contention(&e) && attempt + 1 < LOCK_RETRY_ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(lock_backoff(attempt)).await;
            }
            other => break other,
        }
    };
    let res = match res {
        Err(TxError::Other(e)) if is_lost_marker(&e) => return Ok(FinalizeOutcome::Lost),
        other => other?,
    };
    match res {
        Some((outcome, wake)) => {
            wake.fire();
            Ok(outcome)
        }
        None => Ok(FinalizeOutcome::Lost),
    }
}

async fn finalize_in_tx(
    tx: &DbTx<'_>,
    deps: &Deps,
    quota: &QuotaService,
    turn: &TurnContext,
    counters: ToolCounters,
    plan: TxPlan,
) -> Result<Option<(FinalizeOutcome, Wake)>, TxError> {
    let scope = AccessScope::for_tenant(turn.tenant_id);
    let now = OffsetDateTime::now_utc();

    // 1. Assistant message first (completed / cancelled with partial text).
    let mut assistant_message_id = None;
    if let Some((id, content, usage, resp_id)) = plan.message.clone() {
        let am = assistant_message(
            turn.tenant_id,
            turn.chat_id,
            id,
            turn.request_id,
            content,
            &turn.effective_model,
            usage,
            resp_id,
        );
        secure_insert::<message::Entity>(am, &scope, tx)
            .await
            .map_err(|e| TxError::Message(e.into()))?;
        assistant_message_id = Some(id);
    }

    // 2. CAS.
    let mut upd = chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::State, Expr::value(plan.state.as_str()))
        .col_expr(
            chat_turn::Column::ErrorCode,
            Expr::value(plan.error_code.clone()),
        )
        .col_expr(
            chat_turn::Column::ErrorDetail,
            Expr::value(plan.error_detail.clone()),
        )
        .col_expr(
            chat_turn::Column::AssistantMessageId,
            Expr::value(assistant_message_id),
        )
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(Some(now)))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .col_expr(
            chat_turn::Column::WebSearchCompletedCount,
            Expr::value(i32_of(counters.web_search)),
        )
        .col_expr(
            chat_turn::Column::CodeInterpreterCompletedCount,
            Expr::value(i32_of(counters.code_interpreter)),
        )
        .col_expr(
            chat_turn::Column::FileSearchCompletedCount,
            Expr::value(i32_of(counters.file_search)),
        );
    if plan.provider_response_id.is_some() {
        upd = upd.col_expr(
            chat_turn::Column::ProviderResponseId,
            Expr::value(plan.provider_response_id.clone()),
        );
    }
    let rows = upd
        .filter(
            Condition::all()
                .add(chat_turn::Column::Id.eq(turn.turn_id))
                .add(chat_turn::Column::State.eq("running")),
        )
        .secure()
        .scope_with(&scope)
        .exec_with_returning(tx)
        .await
        .map_err(|e| TxError::Other(e.into()))?;
    let Some(row) = rows.into_iter().next() else {
        if assistant_message_id.is_some() {
            // Roll back the message insert of a lost CAS (mapped to `Lost` by the caller).
            return Err(TxError::Other(lost_marker()));
        }
        return Ok(None);
    };

    // 3. Settlement.
    let (outcome, method) =
        derive_billing(plan.state, plan.error_code.as_deref(), plan.usage.as_ref());
    let credits = match settlement_input(&row, turn, method, plan.usage, counters) {
        Some(input) => {
            quota
                .settle_in_tx(tx, &input)
                .await?
                .committed_credits_micro
        }
        None => {
            tracing::warn!(turn_id = %turn.turn_id, "turn has no reserve fields; settlement skipped");
            0
        }
    };

    // 4. Usage + audit events.
    let usage_ev = build_usage_event(&UsageEventInput {
        tenant_id: turn.tenant_id,
        user_id: Some(turn.user_id),
        chat_id: turn.chat_id,
        turn_id: turn.turn_id,
        request_id: turn.request_id,
        effective_model: turn.effective_model.clone(),
        selected_model: turn.selected_model.clone(),
        terminal_state: plan.state,
        outcome,
        method,
        usage: plan.usage,
        actual_credits_micro: credits,
        policy_version_applied: row
            .policy_version_applied
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0),
        web_search_calls: counters.web_search,
        code_interpreter_calls: counters.code_interpreter,
        file_search_calls: counters.file_search_calls(),
    });
    let mut wake = deps.outbox.enqueue_usage(tx, &usage_ev).await?;
    let audit = build_turn_audit_event(&TurnAuditInput {
        tenant_id: turn.tenant_id,
        requester_user_id: Some(turn.user_id),
        chat_id: turn.chat_id,
        turn_id: turn.turn_id,
        request_id: turn.request_id,
        selected_model: turn.selected_model.clone(),
        effective_model: turn.effective_model.clone(),
        terminal_state: plan.state,
        error_code: plan.error_code.clone(),
        usage: plan.usage,
        latency_ms: u64::try_from(turn.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        web_search_calls: counters.web_search,
        file_search_calls: counters.file_search_calls(),
        quota_decision: if turn.downgrade_reason.is_some() {
            "downgrade".to_owned()
        } else {
            "allow".to_owned()
        },
        downgrade_from: turn.downgrade_reason.map(|_| turn.selected_model.clone()),
        downgrade_reason: turn.downgrade_reason.map(|r| r.as_str().to_owned()),
    });
    wake += deps.outbox.enqueue_audit(tx, &audit).await?;

    // 5. Thread-summary trigger (completed turns).
    if plan.state == TerminalState::Completed
        && let Some(trigger) = &turn.summary_trigger
        && let Some(w) = summary::maybe_enqueue(
            tx,
            deps,
            turn.tenant_id,
            turn.chat_id,
            turn.request_id,
            trigger,
        )
        .await?
    {
        wake += w;
    }

    Ok(Some((
        FinalizeOutcome::Committed {
            state: plan.state,
            error_code: plan.error_code,
        },
        wake,
    )))
}

fn lost_marker() -> DomainError {
    DomainError::Internal(LOST_MARKER.to_owned())
}

const LOST_MARKER: &str = "mini-chat: finalization CAS lost";

fn settlement_input(
    row: &chat_turn::Model,
    turn: &TurnContext,
    method: SettlementMethod,
    usage: Option<UsageTokens>,
    counters: ToolCounters,
) -> Option<SettlementInput> {
    Some(SettlementInput {
        tenant_id: turn.tenant_id,
        user_id: row.requester_user_id?,
        effective_model: row.effective_model.clone()?,
        policy_version: u64::try_from(row.policy_version_applied?).ok()?,
        reserve_tokens: row.reserve_tokens?,
        max_output_tokens_applied: i64::from(row.max_output_tokens_applied?),
        reserved_credits_micro: row.reserved_credits_micro?,
        minimal_generation_floor_applied: i64::from(row.minimal_generation_floor_applied?),
        periods: turn.periods,
        method,
        usage: if method == SettlementMethod::Actual {
            usage
        } else {
            None
        },
        web_search_calls: counters.web_search,
        code_interpreter_calls: counters.code_interpreter,
    })
}

/// True when `e` is the internal marker of a lost CAS after a message insert.
#[must_use]
pub fn is_lost_marker(e: &DomainError) -> bool {
    matches!(e, DomainError::Internal(m) if m == LOST_MARKER)
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod finalization_tests;
