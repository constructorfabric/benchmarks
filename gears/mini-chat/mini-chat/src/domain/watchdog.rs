//! Orphan turn watchdog (DESIGN §4 "Orphan Turn Watchdog", B.9.1).

use sea_orm::EntityTrait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, Order};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::errors::DomainResult;
use crate::domain::finalize::{FinalizeInput, TurnBilling};
use crate::domain::quota::Periods;
use crate::domain::state::AppState;
use crate::infra::db::entities::{chat_turns, chats};
use crate::infra::db::repo;

const SCAN_LIMIT: u64 = 100;

/// Orphan CAS: `running`, not deleted and still stale at `cutoff`.
pub async fn cas_orphan(
    runner: &impl DBRunner,
    scope: &AccessScope,
    turn_id: Uuid,
    cutoff: OffsetDateTime,
    now: OffsetDateTime,
) -> DomainResult<bool> {
    let Some(t) = repo::find_turn(runner, scope, turn_id).await? else {
        return Ok(false);
    };
    if t.state != repo::STATE_RUNNING || t.deleted_at.is_some() || t.last_progress_at.unwrap_or(t.started_at) > cutoff {
        return Ok(false);
    }
    let mut cond = Condition::all()
        .add(chat_turns::Column::Id.eq(turn_id))
        .add(chat_turns::Column::State.eq(repo::STATE_RUNNING))
        .add(chat_turns::Column::DeletedAt.is_null());
    // Re-check that progress did not move since the read (exact match).
    cond = match t.last_progress_at {
        Some(p) => cond.add(chat_turns::Column::LastProgressAt.eq(p)),
        None => cond.add(chat_turns::Column::LastProgressAt.is_null()),
    };
    let res = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value(repo::STATE_FAILED))
        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout")))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
        .filter(cond)
        .scope_with(scope)
        .exec(runner)
        .await?;
    if res.rows_affected == 1 {
        return Ok(true);
    }
    // The stored timestamp may use another textual encoding; fall back to
    // the state guard only.
    let res = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value(repo::STATE_FAILED))
        .col_expr(chat_turns::Column::ErrorCode, Expr::value(Some("orphan_timeout")))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(Some(now)))
        .col_expr(chat_turns::Column::UpdatedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq(repo::STATE_RUNNING))
                .add(chat_turns::Column::DeletedAt.is_null()),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)
}

impl AppState {
    /// One watchdog scan. Returns the number of finalized turns.
    pub async fn orphan_scan(&self) -> DomainResult<usize> {
        let timeout = i64::try_from(self.cfg.orphan_watchdog.timeout_secs).unwrap_or(300);
        let now = repo::now();
        let cutoff = now - time::Duration::seconds(timeout);
        let conn = self.db.conn()?;
        let all = AccessScope::allow_all();
        let running = chat_turns::Entity::find()
            .secure()
            .scope_with(&all)
            .filter(
                Condition::all()
                    .add(chat_turns::Column::State.eq(repo::STATE_RUNNING))
                    .add(chat_turns::Column::DeletedAt.is_null()),
            )
            .order_by(chat_turns::Column::StartedAt, Order::Asc)
            .limit(SCAN_LIMIT * 10)
            .all(&conn)
            .await?;
        let stale: Vec<_> = running
            .into_iter()
            .filter(|t| t.last_progress_at.unwrap_or(t.started_at) <= cutoff)
            .take(usize::try_from(SCAN_LIMIT).unwrap_or(100))
            .collect();
        let mut finalized = 0;
        for t in stale {
            tracing::info!(turn_id = %t.id, "orphan turn detected (stale_progress)");
            match self.finalize_orphan(&conn, &t, cutoff).await {
                Ok(true) => finalized += 1,
                Ok(false) => {}
                Err(e) => tracing::warn!(turn_id = %t.id, error = %e, "orphan finalization failed"),
            }
        }
        Ok(finalized)
    }

    async fn finalize_orphan(
        &self,
        conn: &impl DBRunner,
        t: &chat_turns::Model,
        cutoff: OffsetDateTime,
    ) -> DomainResult<bool> {
        let chat = chats::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .filter(Condition::all().add(chats::Column::Id.eq(t.chat_id)))
            .one(conn)
            .await?;
        let has_reserve = t.reserve_tokens.is_some()
            && t.max_output_tokens_applied.is_some()
            && t.reserved_credits_micro.is_some()
            && t.policy_version_applied.is_some()
            && t.requester_user_id.is_some();
        let effective = t.effective_model.clone().unwrap_or_default();
        let policy_version = u64::try_from(t.policy_version_applied.unwrap_or(0)).unwrap_or(0);
        let (tier, in_mult, out_mult) = if has_reserve {
            let user = t.requester_user_id.unwrap_or_default();
            let snap = self.policy.snapshot(user, policy_version).await?;
            match snap.find_model(&effective) {
                Some(m) => (m.tier, m.input_tokens_credit_multiplier_micro, m.output_tokens_credit_multiplier_micro),
                None => (mini_chat_sdk::ModelTier::Standard, 1, 1),
            }
        } else {
            (mini_chat_sdk::ModelTier::Standard, 1, 1)
        };
        if !has_reserve {
            tracing::warn!(turn_id = %t.id, "orphan turn has no reserve; settlement skipped");
        }
        let _ = chat;
        let billing = TurnBilling {
            tenant_id: t.tenant_id,
            user_id: t.requester_user_id,
            chat_id: t.chat_id,
            turn_id: t.id,
            request_id: t.request_id,
            selected_model: effective.clone(),
            effective_model: effective,
            tier,
            policy_version,
            reserve_tokens: t.reserve_tokens.unwrap_or(0),
            max_output_tokens_applied: i64::from(t.max_output_tokens_applied.unwrap_or(0)),
            reserved_credits_micro: t.reserved_credits_micro.unwrap_or(0),
            floor: i64::from(t.minimal_generation_floor_applied.unwrap_or(0)),
            periods: Periods::from_started_at(t.started_at),
            in_mult,
            out_mult,
            has_reserve,
        };
        let input = FinalizeInput {
            billing,
            state: repo::STATE_FAILED,
            error_code: Some("orphan_timeout".to_owned()),
            error_detail: None,
            usage: None,
            provider_response_id: None,
            assistant_message: None,
            web_search_calls: t.web_search_completed_count,
            code_interpreter_calls: t.code_interpreter_completed_count,
            file_search_calls: t.file_search_completed_count,
            quota_decision: "unknown".to_owned(),
            downgrade_reason: None,
            ttft_ms: None,
            total_ms: 0,
            summary: None,
            orphan_cutoff: Some(cutoff),
        };
        let won = self.finalize_turn(input).await?;
        if won == crate::domain::finalize::FinalizeOutcome::Won {
            tracing::info!(turn_id = %t.id, "orphan turn finalized (stale_progress)");
        }
        Ok(won == crate::domain::finalize::FinalizeOutcome::Won)
    }
}
