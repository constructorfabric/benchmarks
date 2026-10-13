//! Thread summary: trigger + enqueue, mutation invalidation, outbox handler logic (DESIGN §3.6).
//!
//! CONTRACT (implemented by the context/summary work package; signatures fixed).

use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::{SecureDeleteExt, SecureEntityExt, SecureUpdateExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::context::load_summary;
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::metrics;
use crate::infra::outbox::PAYLOAD_THREAD_SUMMARY;
use crate::infra::outbox::payloads::ThreadSummaryPayload;

/// `system_task_type` of the thread summary.
pub const SYSTEM_TASK_TYPE: &str = "thread_summary_update";

/// Trigger inputs computed from the turn's context plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryTrigger {
    pub messages_truncated: bool,
    pub assembled_tokens: i64,
    pub effective_budget: i64,
    pub summary_exists: bool,
}

impl SummaryTrigger {
    /// Urgent (truncated) or proactive (no summary and `assembled >= pct% of effective_budget`).
    #[must_use]
    pub fn fires(&self, compression_threshold_pct: u32) -> bool {
        if self.messages_truncated {
            return true;
        }
        !self.summary_exists
            && i128::from(self.assembled_tokens) * 100
                >= i128::from(compression_threshold_pct) * i128::from(self.effective_budget)
    }
}

/// Evaluates the trigger for a completed turn and, when it fires, enqueues the thread-summary
/// message in `tx` (frozen target = latest live message not belonging to `causing_request_id`).
/// Returns `None` when nothing was enqueued.
///
/// # Errors
/// DB / outbox errors.
pub async fn maybe_enqueue(
    app: &AppServices,
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    causing_request_id: Uuid,
    trigger: &SummaryTrigger,
) -> Result<Option<Wake>, DomainError> {
    let cfg = &app.cfg.thread_summary_worker;
    if !cfg.enabled || !trigger.fires(cfg.compression_threshold_pct) {
        return Ok(None);
    }
    let scope = AccessScope::for_tenant(tenant_id);

    // Frozen target: latest live message that does not belong to the causing turn.
    let target = message::Entity::find()
        .secure()
        .scope_with(&scope)
        .filter(
            Condition::all()
                .add(message::Column::ChatId.eq(chat_id))
                .add(message::Column::DeletedAt.is_null())
                .add(
                    Condition::any()
                        .add(message::Column::RequestId.is_null())
                        .add(message::Column::RequestId.ne(causing_request_id)),
                ),
        )
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(tx)
        .await?;
    let Some(target) = target else {
        record_trigger("not_needed");
        return Ok(None);
    };
    let base = load_summary(tx, tenant_id, chat_id)
        .await?
        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    if base.is_some_and(|b| b >= (target.created_at, target.id)) {
        record_trigger("not_needed");
        return Ok(None);
    }
    let payload = ThreadSummaryPayload {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        system_task_type: SYSTEM_TASK_TYPE.to_owned(),
        base_frontier_created_at: base.map(|b| b.0),
        base_frontier_message_id: base.map(|b| b.1),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
    };
    let wake = app
        .outbox
        .enqueue_json(tx, app.outbox.thread_summary_queue(), chat_id, PAYLOAD_THREAD_SUMMARY, &payload)
        .await?;
    record_trigger("scheduled");
    Ok(Some(wake))
}

fn record_trigger(result: &str) {
    metrics::incr("mini_chat_thread_summary_trigger", 1, &[("result", result.to_owned())]);
}

/// Retry/edit/delete: deletes the chat's summary and clears `is_compressed` when the frontier is at
/// or after the mutated turn's user message `(created_at, id)`.
///
/// # Errors
/// DB errors.
pub async fn invalidate_for_mutation(
    tx: &DbTx<'_>,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_message_created_at: OffsetDateTime,
    user_message_id: Uuid,
) -> Result<(), DomainError> {
    let Some(row) = load_summary(tx, tenant_id, chat_id).await? else {
        return Ok(());
    };
    if (row.summarized_up_to_created_at, row.summarized_up_to_message_id) < (user_message_created_at, user_message_id) {
        return Ok(());
    }
    let scope = AccessScope::for_tenant(tenant_id);
    thread_summary::Entity::delete_many()
        .filter(Condition::all().add(thread_summary::Column::Id.eq(row.id)))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    message::Entity::update_many()
        .col_expr(message::Column::IsCompressed, sea_orm::sea_query::Expr::value(false))
        .filter(Condition::all().add(message::Column::ChatId.eq(chat_id)).add(message::Column::IsCompressed.eq(true)))
        .secure()
        .scope_with(&scope)
        .exec(tx)
        .await?;
    Ok(())
}

pub mod prompt;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
