//! CAS-guarded turn finalization (DESIGN §5.7): one transaction per terminal outcome performs the
//! terminal CAS, the assistant message insert (completed / partial cancelled), the quota
//! settlement and the usage + audit (+ thread summary) outbox enqueue.

use std::sync::Arc;
use std::time::Instant;

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, MiniChatAuditEvent, TurnAuditEvent, UsageTokens, UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::clock;
use crate::domain::error::DomainError;
use crate::domain::quota::{self, PeriodStarts, QuotaWarning, SettlementInput};
use crate::domain::services::AppServices;
use crate::domain::stream::queries::{self, STATE_CANCELLED, STATE_COMPLETED, STATE_FAILED, STATE_RUNNING};
use crate::domain::summary::{self, SummaryTrigger};
use crate::infra::db::entities::{chat_turn, message};
use crate::infra::outbox::PAYLOAD_AUDIT;

/// Terminal outcome reported by the provider task.
#[derive(Debug, Clone)]
pub enum Terminal {
    Completed { usage: Option<UsageTokens>, response_id: Option<String>, incomplete_reason: Option<String> },
    Failed { code: String, detail: String, usage: Option<UsageTokens> },
    Cancelled,
}

/// Everything the finalization needs besides the terminal outcome.
#[derive(Debug, Clone)]
pub struct FinalizeContext {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub turn_id: Uuid,
    pub request_id: Uuid,
    /// Pre-allocated assistant message id.
    pub message_id: Uuid,
    pub selected_model: String,
    pub effective_model: String,
    /// `allow` | `downgrade`
    pub quota_decision: String,
    pub downgrade_reason: Option<String>,
    pub periods: PeriodStarts,
    pub limits: UserLimits,
    pub text: String,
    pub web_search_completed: u32,
    pub code_interpreter_completed: u32,
    pub file_search_completed: u32,
    pub started: Instant,
    pub summary_trigger: Option<SummaryTrigger>,
}

/// Result of a finalization attempt.
#[derive(Debug)]
pub enum FinalizeResult {
    /// The CAS was won and the transaction committed.
    Committed { warnings: Vec<QuotaWarning> },
    /// Another finalizer already moved the turn out of `running`.
    CasLost,
    /// The assistant message could not be persisted; the turn was finalized as `failed`.
    MessagePersistenceFailed,
    /// The finalization transaction failed; the turn stays `running`.
    Failed(DomainError),
}

enum TxError {
    CasLost,
    MessageInsert(DomainError),
    Other(DomainError),
}

impl From<DomainError> for TxError {
    fn from(e: DomainError) -> Self {
        Self::Other(e)
    }
}

/// Finalizes a turn. Never panics; errors are reported through `FinalizeResult`.
pub async fn finalize(app: &Arc<AppServices>, fctx: &FinalizeContext, terminal: &Terminal) -> FinalizeResult {
    let with_message = match terminal {
        Terminal::Completed { .. } => true,
        Terminal::Cancelled => !fctx.text.is_empty(),
        Terminal::Failed { .. } => false,
    };
    match run_tx(app, fctx, terminal, with_message).await {
        Ok((warnings, wake)) => {
            wake.fire();
            FinalizeResult::Committed { warnings }
        }
        Err(TxError::CasLost) => FinalizeResult::CasLost,
        Err(TxError::MessageInsert(e)) => {
            tracing::warn!(error = %e, turn_id = %fctx.turn_id, "assistant message persistence failed");
            match terminal {
                Terminal::Completed { usage, .. } => {
                    let failed = Terminal::Failed {
                        code: "message_persistence_failed".to_owned(),
                        detail: "assistant message could not be persisted".to_owned(),
                        usage: *usage,
                    };
                    match run_tx(app, fctx, &failed, false).await {
                        Ok((_, wake)) => {
                            wake.fire();
                            FinalizeResult::MessagePersistenceFailed
                        }
                        Err(TxError::CasLost) => FinalizeResult::CasLost,
                        Err(TxError::MessageInsert(e) | TxError::Other(e)) => FinalizeResult::Failed(e),
                    }
                }
                // Partial content of a cancelled turn is best effort.
                _ => match run_tx(app, fctx, terminal, false).await {
                    Ok((w, wake)) => {
                        wake.fire();
                        FinalizeResult::Committed { warnings: w }
                    }
                    Err(TxError::CasLost) => FinalizeResult::CasLost,
                    Err(TxError::MessageInsert(e) | TxError::Other(e)) => FinalizeResult::Failed(e),
                },
            }
        }
        Err(TxError::Other(e)) => {
            tracing::error!(error = %e, turn_id = %fctx.turn_id, "turn finalization failed");
            FinalizeResult::Failed(e)
        }
    }
}

async fn run_tx(
    app: &Arc<AppServices>,
    fctx: &FinalizeContext,
    terminal: &Terminal,
    with_message: bool,
) -> Result<(Vec<QuotaWarning>, Wake), TxError> {
    let res = crate::domain::tx::retry_contention(|| {
        let app2 = Arc::clone(app);
        let fctx = fctx.clone();
        let terminal = terminal.clone();
        async move {
            let app3 = Arc::clone(&app2);
            app2.db
                .transaction(move |tx| {
                    Box::pin(async move {
                        match finalize_in_tx(&app3, tx, &fctx, &terminal, with_message).await {
                            Ok(v) => Ok(Ok(v)),
                            // Returning Err rolls the transaction back.
                            Err(TxError::CasLost) => Err(DomainError::internal("__cas_lost__")),
                            Err(TxError::MessageInsert(e)) => Err(DomainError::internal(format!("__msg_insert__{e}"))),
                            Err(TxError::Other(e)) => Err(e),
                        }
                    })
                })
                .await
        }
    })
    .await;
    match res {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e),
        Err(DomainError::Internal(m)) if m == "__cas_lost__" => Err(TxError::CasLost),
        Err(DomainError::Internal(m)) if m.starts_with("__msg_insert__") => {
            Err(TxError::MessageInsert(DomainError::internal(m.trim_start_matches("__msg_insert__").to_owned())))
        }
        Err(e) => Err(TxError::Other(e)),
    }
}

#[allow(clippy::too_many_lines)]
async fn finalize_in_tx(
    app: &AppServices,
    tx: &DbTx<'_>,
    fctx: &FinalizeContext,
    terminal: &Terminal,
    with_message: bool,
) -> Result<(Vec<QuotaWarning>, Wake), TxError> {
    let now = clock::now();
    let scope = AccessScope::for_tenant(fctx.tenant_id);
    let Some(turn) = queries::turn_by_id(tx, fctx.tenant_id, fctx.turn_id).await? else {
        return Err(TxError::CasLost);
    };
    if turn.state != STATE_RUNNING {
        return Err(TxError::CasLost);
    }

    let (state, error_code, usage, response_id) = match terminal {
        Terminal::Completed { usage, response_id, .. } => (STATE_COMPLETED, None, *usage, response_id.clone()),
        Terminal::Failed { code, usage, .. } => (STATE_FAILED, Some(code.clone()), *usage, None),
        Terminal::Cancelled => (STATE_CANCELLED, None, None, None),
    };

    let mut assistant_message_id = None;
    if with_message {
        let u = usage.unwrap_or_default();
        let am = message::ActiveModel {
            id: Set(fctx.message_id),
            tenant_id: Set(fctx.tenant_id),
            chat_id: Set(fctx.chat_id),
            request_id: Set(Some(fctx.request_id)),
            role: Set("assistant".to_owned()),
            content: Set(fctx.text.clone()),
            content_type: Set("text".to_owned()),
            token_estimate: Set(0),
            provider_response_id: Set(response_id.clone()),
            request_kind: Set("chat".to_owned()),
            features_used: Set(serde_json::json!([])),
            input_tokens: Set(u.input_tokens),
            output_tokens: Set(u.output_tokens),
            cache_read_input_tokens: Set(u.cache_read_input_tokens),
            cache_write_input_tokens: Set(u.cache_write_input_tokens),
            reasoning_tokens: Set(u.reasoning_tokens),
            model: Set(Some(fctx.effective_model.clone())),
            is_compressed: Set(false),
            created_at: Set(now),
            deleted_at: Set(None),
        };
        secure_insert::<message::Entity>(am, &scope, tx).await.map_err(|e| {
            let e = DomainError::from(e);
            // Lock contention is retried as a whole transaction, not treated as a persistence failure.
            if e.is_contention() { TxError::Other(e) } else { TxError::MessageInsert(e) }
        })?;
        assistant_message_id = Some(fctx.message_id);
    }

    // Terminal CAS: exactly one finalizer wins.
    let mut upd = chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::State, Expr::value(state))
        .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
        .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
        .col_expr(chat_turn::Column::LastProgressAt, Expr::value(now))
        .col_expr(chat_turn::Column::WebSearchCompletedCount, Expr::value(i32::try_from(fctx.web_search_completed).unwrap_or(i32::MAX)))
        .col_expr(
            chat_turn::Column::CodeInterpreterCompletedCount,
            Expr::value(i32::try_from(fctx.code_interpreter_completed).unwrap_or(i32::MAX)),
        )
        .col_expr(chat_turn::Column::FileSearchCompletedCount, Expr::value(i32::try_from(fctx.file_search_completed).unwrap_or(i32::MAX)));
    if let Some(id) = assistant_message_id {
        upd = upd.col_expr(chat_turn::Column::AssistantMessageId, Expr::value(id));
    }
    if let Some(rid) = &response_id {
        upd = upd.col_expr(chat_turn::Column::ProviderResponseId, Expr::value(rid.clone()));
    }
    if let Some(code) = &error_code {
        upd = upd.col_expr(chat_turn::Column::ErrorCode, Expr::value(code.clone()));
    }
    if let Terminal::Failed { detail, .. } = terminal {
        upd = upd.col_expr(chat_turn::Column::ErrorDetail, Expr::value(crate::domain::sanitize::sanitize_provider_message(detail)));
    }
    let rows = upd
        .filter(Condition::all().add(chat_turn::Column::Id.eq(fctx.turn_id)).add(chat_turn::Column::State.eq(STATE_RUNNING)))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await
        .map_err(DomainError::from)?
        .rows_affected;
    if rows != 1 {
        return Err(TxError::CasLost);
    }

    // Settlement (actual / estimated) + usage event.
    let (billing, method) = quota::derive_billing(state, error_code.as_deref(), usage.as_ref());
    let input = SettlementInput {
        tenant_id: fctx.tenant_id,
        user_id: fctx.user_id,
        turn: turn.clone(),
        billing_outcome: billing,
        method,
        usage,
        web_search_calls: fctx.web_search_completed,
        code_interpreter_calls: fctx.code_interpreter_completed,
        periods: fctx.periods,
    };
    let settlement = quota::settle(app, tx, &input).await?;
    let event = quota::usage_event(&input, &settlement, &fctx.selected_model, fctx.file_search_completed, state, now);
    let mut wake = quota::enqueue_usage(app, tx, &event).await?;

    // Turn audit event.
    let audit = MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: if state == STATE_COMPLETED { "turn_completed" } else { "turn_failed" }.to_owned(),
        tenant_id: fctx.tenant_id,
        user_id: Some(fctx.user_id),
        chat_id: fctx.chat_id,
        turn_id: fctx.turn_id,
        request_id: fctx.request_id,
        selected_model: fctx.selected_model.clone(),
        effective_model: fctx.effective_model.clone(),
        terminal_state: state.to_owned(),
        error_code: error_code.clone(),
        usage,
        latency_ms: u64::try_from(fctx.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        tool_calls: AuditToolCalls { web_search_calls: fctx.web_search_completed, file_search_calls: fctx.file_search_completed },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: fctx.quota_decision.clone(),
                downgrade_from: (fctx.quota_decision == "downgrade").then(|| fctx.selected_model.clone()),
                downgrade_reason: fctx.downgrade_reason.clone(),
            },
            license: None,
            quota_scope: None,
        },
        prompt: String::new(),
        response: String::new(),
        attachments: Vec::new(),
        trace_id: None,
        timestamp: now,
    });
    wake += app
        .outbox
        .enqueue_json(tx, app.outbox.audit_queue(), fctx.tenant_id, PAYLOAD_AUDIT, &audit)
        .await?;

    let mut warnings = Vec::new();
    if state == STATE_COMPLETED {
        if let Some(trigger) = &fctx.summary_trigger
            && let Some(w) = summary::maybe_enqueue(app, tx, fctx.tenant_id, fctx.chat_id, fctx.request_id, trigger).await?
        {
            wake += w;
        }
        warnings = quota::quota_warnings(app, tx, fctx.tenant_id, fctx.user_id, &fctx.limits, now).await?;
    }
    Ok((warnings, wake))
}

/// Marks an unstarted retry/edit turn `failed` (no reserve, no settlement, no outbox event).
///
/// # Errors
/// DB errors.
pub async fn fail_unstarted_turn(app: &AppServices, tenant_id: Uuid, turn_id: Uuid, error_code: &str) -> Result<(), DomainError> {
    let now = clock::now();
    crate::domain::tx::retry_contention(|| {
        let code = error_code.to_owned();
        app.db.transaction(move |tx| {
            Box::pin(async move {
                chat_turn::Entity::update_many()
                    .col_expr(chat_turn::Column::State, Expr::value(STATE_FAILED))
                    .col_expr(chat_turn::Column::ErrorCode, Expr::value(code))
                    .col_expr(chat_turn::Column::CompletedAt, Expr::value(now))
                    .col_expr(chat_turn::Column::UpdatedAt, Expr::value(now))
                    .filter(Condition::all().add(chat_turn::Column::Id.eq(turn_id)).add(chat_turn::Column::State.eq(STATE_RUNNING)))
                    .secure()
                    .scope_with(&AccessScope::for_tenant(tenant_id))
                    .exec(tx)
                    .await?;
                Ok(())
            })
        })
    })
    .await
}
