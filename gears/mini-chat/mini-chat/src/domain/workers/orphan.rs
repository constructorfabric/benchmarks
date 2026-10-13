//! Orphan turn watchdog (DESIGN §4 "Orphan Turn Watchdog", B.9.1).

use mini_chat_sdk::{
    AuditUsage, MiniChatAuditEvent, ModelTier, PolicyDecisions, QuotaPolicyDecision, ToolCalls,
    TurnAuditEvent, UsageEvent,
};
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::billing::{Reserve, Terminal, classify, settle};
use crate::domain::error::DomainError;
use crate::domain::quota::{PeriodStarts, ToolCounts, apply_settlement};
use crate::domain::service::MiniChat;
use crate::domain::stream::finalize::dedupe_key;
use crate::infra::db::entity::{chat_turns, chats};
use crate::infra::db::now;
use crate::infra::outbox::Queue;

const SCAN_LIMIT: u64 = 100;

fn stale_cond(cutoff: OffsetDateTime) -> Condition {
    Condition::all()
        .add(chat_turns::Column::State.eq("running"))
        .add(chat_turns::Column::DeletedAt.is_null())
        .add(
            Condition::any()
                .add(chat_turns::Column::LastProgressAt.lte(cutoff))
                .add(
                    Condition::all()
                        .add(chat_turns::Column::LastProgressAt.is_null())
                        .add(chat_turns::Column::StartedAt.lte(cutoff)),
                ),
        )
}

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Settlement inputs resolved before the transaction.
#[derive(Clone)]
struct Pricing {
    in_mult: i64,
    out_mult: i64,
    premium: bool,
}

impl MiniChat {
    /// One watchdog scan; returns the number of finalized turns.
    ///
    /// # Errors
    /// Database failure of the candidate scan.
    // reason: score inflated by tracing macro expansion; logic is a single scan loop
    #[allow(clippy::cognitive_complexity)]
    pub async fn orphan_scan(&self) -> Result<usize, DomainError> {
        let timeout = time::Duration::seconds(i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300));
        let cutoff = now() - timeout;
        let conn = self.db.conn()?;
        let candidates = chat_turns::Entity::find()
            .filter(stale_cond(cutoff))
            .order_by_asc(chat_turns::Column::StartedAt)
            .limit(SCAN_LIMIT)
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await?;
        let mut finalized = 0;
        for t in candidates {
            tracing::info!(turn_id = %t.id, reason = "stale_progress", "orphan turn detected");
            match self.finalize_orphan(&t, cutoff).await {
                Ok(true) => {
                    finalized += 1;
                    tracing::info!(turn_id = %t.id, reason = "stale_progress", "orphan turn finalized");
                }
                Ok(false) => {}
                Err(e) => tracing::error!(turn_id = %t.id, error = %e, "orphan finalization failed"),
            }
        }
        Ok(finalized)
    }

    async fn orphan_pricing(&self, t: &chat_turns::Model) -> Option<Pricing> {
        let user = t.requester_user_id?;
        let version = u64::try_from(t.policy_version_applied?).ok()?;
        let model = t.effective_model.clone()?;
        let snap = self.policy.snapshot(user, version).await.ok()?;
        let m = snap.find(&model)?;
        Some(Pricing {
            in_mult: m.input_tokens_credit_multiplier_micro,
            out_mult: m.output_tokens_credit_multiplier_micro,
            premium: m.tier == ModelTier::Premium,
        })
    }

    /// Orphan finalization with its own CAS (re-checks the stale predicate).
    async fn finalize_orphan(&self, t: &chat_turns::Model, cutoff: OffsetDateTime) -> Result<bool, DomainError> {
        let pricing = self.orphan_pricing(t).await;
        let reserve = match (
            t.reserve_tokens,
            t.max_output_tokens_applied,
            t.reserved_credits_micro,
            t.minimal_generation_floor_applied,
            t.requester_user_id,
        ) {
            (Some(tokens), Some(max_out), Some(reserved_credits), Some(floor), Some(_)) => Some(Reserve {
                reserve_tokens: tokens,
                max_output_tokens_applied: i64::from(max_out),
                reserved_credits_micro: reserved_credits,
                minimal_generation_floor_applied: i64::from(floor),
            }),
            _ => None,
        };
        let conn = self.db.conn()?;
        let chat = chats::Entity::find()
            .filter(chats::Column::Id.eq(t.chat_id))
            .secure()
            .scope_with(&AccessScope::for_tenant(t.tenant_id))
            .one(&conn)
            .await?;
        let selected_model = t.effective_model.clone().unwrap_or_default();
        let _ = chat;
        let t2 = t.clone();
        let outbox = self.outbox.clone();
        let tolerance = self.cfg.quota.overshoot_tolerance_factor;
        let res = crate::infra::db::tx_retry(&self.db, move |tx| {
                let outbox = outbox.clone();
                let pricing = pricing.clone();
                let selected_model = selected_model.clone();
                let t2 = t2.clone();
                Box::pin(async move {
                    let ts = now();
                    let scope = AccessScope::for_tenant(t2.tenant_id);
                    let cas = chat_turns::Entity::update_many()
                        .col_expr(chat_turns::Column::State, Expr::value("failed"))
                        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout".to_owned())))
                        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(ts)))
                        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(ts))
                        .filter(Condition::all().add(chat_turns::Column::Id.eq(t2.id)).add(stale_cond(cutoff)))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if cas.rows_affected == 0 {
                        return Ok(None);
                    }
                    let class = classify(&Terminal::Orphan, None);
                    let mut credits = 0_i64;
                    let settled = if let (Some(r), Some(p), Some(user)) = (reserve, pricing.as_ref(), t2.requester_user_id) {
                        let s = settle(class.method, r, None, p.in_mult, p.out_mult, tolerance)
                            .map_err(|e| DomainError::Internal(format!("orphan settlement: {e}")))?;
                        apply_settlement(
                            tx,
                            t2.tenant_id,
                            user,
                            PeriodStarts::of(t2.started_at),
                            p.premium,
                            r.reserved_credits_micro,
                            &s,
                            ToolCounts {
                                web_search: t2.web_search_completed_count,
                                code_interpreter: t2.code_interpreter_completed_count,
                            },
                        )
                        .await?;
                        credits = s.committed_credits_micro;
                        true
                    } else {
                        tracing::warn!(turn_id = %t2.id, "orphan turn without reserve fields; settlement skipped");
                        false
                    };
                    let (eff, sel, version) = if settled || reserve.is_some() {
                        (
                            t2.effective_model.clone().unwrap_or_default(),
                            selected_model.clone(),
                            t2.policy_version_applied.and_then(|v| u64::try_from(v).ok()).unwrap_or(0),
                        )
                    } else {
                        (String::new(), String::new(), 0)
                    };
                    let event = UsageEvent {
                        tenant_id: t2.tenant_id,
                        user_id: t2.requester_user_id,
                        chat_id: t2.chat_id,
                        turn_id: Some(t2.id),
                        request_id: t2.request_id,
                        effective_model: eff.clone(),
                        selected_model: sel.clone(),
                        terminal_state: "failed".into(),
                        billing_outcome: class.billing_outcome.into(),
                        usage: None,
                        actual_credits_micro: credits,
                        settlement_method: class.method.into(),
                        policy_version_applied: version,
                        web_search_calls: u32::try_from(t2.web_search_completed_count).unwrap_or(0),
                        code_interpreter_calls: u32::try_from(t2.code_interpreter_completed_count).unwrap_or(0),
                        file_search_calls: u32::try_from(t2.file_search_completed_count).unwrap_or(0),
                        timestamp: rfc3339(ts),
                        requester_type: t2.requester_type.clone(),
                        dedupe_key: dedupe_key(t2.tenant_id, t2.id, t2.request_id),
                        system_task_type: None,
                    };
                    let mut wake = outbox.enqueue(tx, Queue::Usage, t2.tenant_id, &event).await?;
                    let audit = TurnAuditEvent {
                        event_type: "turn_failed".into(),
                        tenant_id: t2.tenant_id,
                        user_id: t2.requester_user_id,
                        requester_type: t2.requester_type.clone(),
                        chat_id: t2.chat_id,
                        turn_id: t2.id,
                        request_id: t2.request_id,
                        selected_model: sel,
                        effective_model: eff,
                        terminal_state: "failed".into(),
                        error_code: Some("orphan_timeout".into()),
                        usage: AuditUsage::default(),
                        latency_ms: u64::try_from((ts - t2.started_at).whole_milliseconds()).unwrap_or(0),
                        tool_calls: ToolCalls {
                            web_search_calls: u32::try_from(t2.web_search_completed_count).unwrap_or(0),
                            file_search_calls: u32::try_from(t2.file_search_completed_count).unwrap_or(0),
                        },
                        policy_decisions: PolicyDecisions {
                            quota: QuotaPolicyDecision {
                                decision: "unknown".into(),
                                downgrade_from: None,
                                downgrade_reason: None,
                            },
                            license: String::new(),
                            quota_scope: String::new(),
                        },
                        prompt: String::new(),
                        response: String::new(),
                        attachments: Vec::new(),
                        trace_id: None,
                        timestamp: rfc3339(ts),
                    };
                    wake += outbox
                        .enqueue(tx, Queue::Audit, t2.tenant_id, &MiniChatAuditEvent::Turn(Box::new(audit)))
                        .await?;
                    Ok(Some(wake))
                })
            })
            .await?;
        Ok(match res {
            Some(w) => {
                w.fire();
                true
            }
            None => false,
        })
    }
}
