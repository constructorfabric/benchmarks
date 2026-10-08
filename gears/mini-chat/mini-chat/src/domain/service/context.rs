//! Context plan assembly and truncation (OWNER: streaming core).
//!
//! DESIGN "Context Plan Assembly and Truncation", "ContextPlan Determinism and Snapshot
//! Boundary" and the context window budget constraint. The assembly itself is a pure,
//! deterministic function ([`assemble`]); [`load_history`] reads the inputs (thread summary and
//! recent messages up to the snapshot boundary).

use mini_chat_sdk::EstimationBudgets;
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, reasons, resource_types};
use crate::domain::service::quota::ToolGates;
use crate::infra::db::entity::{message, thread_summary};
use crate::infra::llm::{InputMessage, InputRole};

/// Preamble prepended to the thread summary (B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Conservative token estimate of a text of `bytes` UTF-8 bytes (DESIGN 5.5.4):
/// `ceil((ceil(bytes / bpt) + fixed_overhead) * (100 + margin) / 100)`.
#[must_use]
pub fn estimate_text_tokens(b: &EstimationBudgets, bytes: usize) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let base = bytes
        .div_ceil(bpt)
        .saturating_add(u64::from(b.fixed_overhead_tokens));
    let with_margin = base
        .saturating_mul(100 + u64::from(b.safety_margin_pct))
        .div_ceil(100);
    i64::try_from(with_margin).unwrap_or(i64::MAX)
}

/// Estimate of the current user message: text + `image_count × image_token_budget`.
#[must_use]
pub fn estimate_message_tokens(b: &EstimationBudgets, text: &str, image_count: u32) -> i64 {
    estimate_text_tokens(b, text.len())
        .saturating_add(i64::from(image_count).saturating_mul(i64::from(b.image_token_budget)))
}

/// `min(max_input_tokens (0 = none), context_window - max_output_tokens_applied)`.
#[must_use]
pub fn input_limit(
    context_window: u32,
    max_input_tokens: u32,
    max_output_tokens_applied: i32,
) -> i64 {
    let room = i64::from(context_window) - i64::from(max_output_tokens_applied);
    if max_input_tokens > 0 {
        room.min(i64::from(max_input_tokens))
    } else {
        room
    }
}

/// Inputs of a context assembly (all values of the effective model).
pub struct ContextInput<'a> {
    pub budgets: &'a EstimationBudgets,
    pub context_window: u32,
    pub max_input_tokens: u32,
    pub max_output_tokens_applied: i32,
    /// Tools actually sent with the request (their surcharges are deducted).
    pub tools: ToolGates,
    /// System prompt + tool guards.
    pub instructions: &'a str,
    /// Current user message text.
    pub current_text: &'a str,
    /// Images of the current user message.
    pub image_count: u32,
    pub summary: Option<&'a thread_summary::Model>,
    /// Recent messages in chronological order.
    pub recent: &'a [message::Model],
}

/// Result of a context assembly.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// Summary message (if kept) followed by the kept recent messages; the current user
    /// message is NOT included.
    pub messages: Vec<InputMessage>,
    /// Estimated size of the assembled request (mandatory + summary + kept history).
    pub assembled_tokens: i64,
    /// At least one recent message was dropped for the budget.
    pub messages_truncated: bool,
    /// `token_estimate` of the thread summary when it was included.
    pub summary_applied: Option<i32>,
    /// Final token budget used for the assembly.
    pub token_budget: i64,
}

fn budget_exceeded() -> DomainError {
    DomainError::out_of_range(
        resource_types::CHAT,
        "context",
        reasons::CONTEXT_BUDGET_EXCEEDED,
        "The message and the mandatory context do not fit the model's context window",
    )
}

/// Summary message as sent to the model (one user message: preamble + summary).
#[must_use]
pub fn summary_message_text(summary_text: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary_text}")
}

fn role_of(m: &message::Model) -> Option<InputRole> {
    match m.role.as_str() {
        "user" => Some(InputRole::User),
        "assistant" => Some(InputRole::Assistant),
        _ => None,
    }
}

/// Deterministic assembly with budget truncation.
///
/// # Errors
/// 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED` when the budget is invalid or the mandatory
/// items alone exceed it.
pub fn assemble(i: &ContextInput<'_>) -> Result<ContextPlan, DomainError> {
    let b = i.budgets;
    if i64::from(i.max_output_tokens_applied) >= i64::from(i.context_window) {
        return Err(budget_exceeded());
    }
    let limit = input_limit(
        i.context_window,
        i.max_input_tokens,
        i.max_output_tokens_applied,
    );
    let mut deductions = i64::from(b.fixed_overhead_tokens);
    if i.tools.file_search {
        deductions += i64::from(b.tool_surcharge_tokens);
    }
    if i.tools.web_search {
        deductions += i64::from(b.web_search_surcharge_tokens);
    }
    if i.tools.code_interpreter {
        deductions += i64::from(b.code_interpreter_surcharge_tokens);
    }
    let token_budget = limit - deductions;
    if token_budget <= 0 {
        return Err(budget_exceeded());
    }

    let mandatory = estimate_text_tokens(b, i.instructions.len())
        .saturating_add(estimate_message_tokens(b, i.current_text, i.image_count));
    if mandatory > token_budget {
        return Err(budget_exceeded());
    }
    let mut remaining = token_budget - mandatory;
    let mut assembled = mandatory;

    let mut messages = Vec::new();
    let mut summary_applied = None;
    if let Some(s) = i.summary {
        let text = summary_message_text(&s.summary_text);
        let est = estimate_text_tokens(b, text.len());
        if est <= remaining {
            remaining -= est;
            assembled += est;
            summary_applied = Some(s.token_estimate);
            messages.push(InputMessage::text(InputRole::User, text));
        }
    }

    let candidates: Vec<&message::Model> =
        i.recent.iter().filter(|m| role_of(m).is_some()).collect();
    let mut kept_rev: Vec<(&message::Model, i64)> = Vec::new();
    for m in candidates.iter().rev() {
        let est = estimate_text_tokens(b, m.content.len());
        if est > remaining {
            break;
        }
        remaining -= est;
        kept_rev.push((m, est));
    }
    let mut kept: Vec<(&message::Model, i64)> = kept_rev.into_iter().rev().collect();
    // A message was dropped for the budget (drives the urgent summary trigger).
    let budget_dropped = kept.len() < candidates.len();
    // Never start the kept range with an answer: truncation removes whole turns.
    let lead = kept
        .iter()
        .take_while(|(m, _)| m.role == "assistant")
        .count();
    kept.drain(..lead);
    // A leading answer dropped after a budget truncation is part of that truncation (the
    // flag is set); one dropped only because the `recent_messages_limit` window starts with
    // an answer (nothing dropped for the budget) does not set it.
    let messages_truncated = budget_dropped;
    for (m, est) in kept {
        assembled += est;
        if let Some(role) = role_of(m) {
            messages.push(InputMessage::text(role, m.content.clone()));
        }
    }

    Ok(ContextPlan {
        messages,
        assembled_tokens: assembled,
        messages_truncated,
        summary_applied,
        token_budget,
    })
}

/// Thread summary and recent messages loaded for a context assembly.
#[derive(Debug, Clone, Default)]
pub struct History {
    pub summary: Option<thread_summary::Model>,
    /// Chronological order `(created_at ASC, id ASC)`.
    pub recent: Vec<message::Model>,
}

/// `(created_at, id) <= (at, id)` on messages.
#[must_use]
pub fn msg_key_le(at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(message::Column::CreatedAt.lt(at)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(at))
            .add(message::Column::Id.lte(id)),
    )
}

/// `(created_at, id) > (at, id)` on messages.
#[must_use]
pub fn msg_key_gt(at: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(message::Column::CreatedAt.gt(at)).add(
        Condition::all()
            .add(message::Column::CreatedAt.eq(at))
            .add(message::Column::Id.gt(id)),
    )
}

/// Loads the chat's thread summary and its recent messages (newest `limit` by
/// `(created_at DESC, id DESC)`, reversed) up to the snapshot boundary. Messages of
/// `current_request_id` (the turn being assembled) are excluded from the boundary.
///
/// `scope` must be the tenant-only child scope of an authorized chat.
///
/// # Errors
/// Database failure.
pub async fn load_history(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
    current_request_id: Uuid,
    limit: u32,
) -> Result<History, DomainError> {
    let summary = thread_summary::Entity::find()
        .filter(thread_summary::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?;

    let visible = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null())
        .add(message::Column::RequestId.is_not_null())
        .add(message::Column::RequestId.ne(current_request_id));
    let boundary = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(visible.clone())
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(1)
        .one(runner)
        .await?;
    let Some(boundary) = boundary else {
        return Ok(History {
            summary,
            recent: Vec::new(),
        });
    };
    if limit == 0 {
        return Ok(History {
            summary,
            recent: Vec::new(),
        });
    }
    let mut cond = visible
        .add(message::Column::IsCompressed.eq(false))
        .add(msg_key_le(boundary.created_at, boundary.id));
    if let Some(s) = &summary {
        cond = cond.add(msg_key_gt(
            s.summarized_up_to_created_at,
            s.summarized_up_to_message_id,
        ));
    }
    let mut recent = message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(cond)
        .order_by(message::Column::CreatedAt, Order::Desc)
        .order_by(message::Column::Id, Order::Desc)
        .limit(u64::from(limit))
        .all(runner)
        .await?;
    recent.reverse();
    Ok(History { summary, recent })
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod context_tests;
