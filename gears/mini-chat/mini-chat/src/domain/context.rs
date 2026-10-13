//! Context plan assembly and deterministic truncation (DESIGN §4 "Context Plan Assembly").
//!
//! CONTRACT (implemented by the context/summary work package; signatures fixed).

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order};
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::error::{DomainError, Resource};
use crate::domain::services::AppServices;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::llm::responses::{InputItem, InputRole};

/// Preamble sent before the thread summary (DESIGN B.5.5).
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A persisted conversation message used for context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: Uuid,
    /// `user` | `assistant` | `system`
    pub role: String,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// The chat's committed thread summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryContext {
    pub text: String,
    pub token_estimate: i64,
    pub frontier_created_at: OffsetDateTime,
    pub frontier_message_id: Uuid,
}

/// Inputs of the assembly.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    /// Effective model (budget, estimation budgets, `system_prompt`).
    pub model: ModelCatalogEntry,
    pub max_output_tokens_applied: i64,
    /// Guard texts for the tools that are sent, appended to the system prompt in order.
    pub guards: Vec<String>,
    pub summary: Option<SummaryContext>,
    /// Recent messages in chronological order (already limited to `recent_messages_limit`).
    pub recent: Vec<HistoryMessage>,
    pub user_message: String,
    pub image_file_ids: Vec<String>,
    /// Surcharges of the tools that are sent (file search / web search / code interpreter).
    pub surcharge_tokens: i64,
}

/// Assembled context.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    /// System prompt + guards.
    pub instructions: String,
    /// Summary (as a user message with the preamble), kept recent messages, current user message.
    pub input: Vec<InputItem>,
    /// `token_estimate` of the summary when it was kept.
    pub summary_applied: Option<i64>,
    pub messages_truncated: bool,
    /// Estimated tokens of everything assembled (instructions + summary + kept history + message).
    pub assembled_tokens: i64,
    /// `min(max_input_tokens (0 = none), context_window - max_output_tokens_applied)`.
    pub effective_budget: i64,
}

/// Reason of the context budget rejection.
pub const CONTEXT_BUDGET_EXCEEDED: &str = "CONTEXT_BUDGET_EXCEEDED";

/// Estimated tokens of a text of `bytes` bytes with the model's estimation budgets:
/// `ceil((ceil(bytes / bytes_per_token) + fixed_overhead) * (100 + safety_margin_pct) / 100)`.
#[must_use]
pub fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = u64::from(b.bytes_per_token_conservative.max(1));
    let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
    let base = bytes.div_ceil(bpt).saturating_add(u64::from(b.fixed_overhead_tokens));
    let factor = 100 + u64::from(b.safety_margin_pct);
    i64::try_from(base.saturating_mul(factor).div_ceil(100)).unwrap_or(i64::MAX)
}

/// `min(max_input_tokens (0 = none), context_window - max_output_tokens_applied)`.
#[must_use]
pub fn input_limit(model: &ModelCatalogEntry, max_output_tokens_applied: i64) -> i64 {
    let by_window = i64::from(model.context_window) - max_output_tokens_applied;
    if model.max_input_tokens > 0 { by_window.min(i64::from(model.max_input_tokens)) } else { by_window }
}

fn budget_exceeded(description: impl Into<String>) -> DomainError {
    DomainError::out_of_range(Resource::Chat, "context", CONTEXT_BUDGET_EXCEEDED, description)
}

/// System prompt followed by the guards, separated by blank lines (empty parts skipped).
fn build_instructions(system_prompt: &str, guards: &[String]) -> String {
    std::iter::once(system_prompt)
        .chain(guards.iter().map(String::as_str))
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Summary text as sent to the model (preamble + blank line + summary).
#[must_use]
pub fn summary_message_text(summary: &str) -> String {
    format!("{SUMMARY_PREAMBLE}\n\n{summary}")
}

/// Assembles and truncates the context. Rejects with 400 `out_of_range` `CONTEXT_BUDGET_EXCEEDED`
/// when the budget is not positive or the mandatory items do not fit.
///
/// # Errors
/// `CONTEXT_BUDGET_EXCEEDED`.
pub fn assemble(req: &ContextRequest) -> Result<ContextPlan, DomainError> {
    let model = &req.model;
    let eb = &model.estimation_budgets;
    if req.max_output_tokens_applied >= i64::from(model.context_window) {
        return Err(budget_exceeded("max output tokens leave no room for input in the model context window"));
    }
    let limit = input_limit(model, req.max_output_tokens_applied);
    let budget = limit - req.surcharge_tokens - i64::from(eb.fixed_overhead_tokens);
    if budget <= 0 {
        return Err(budget_exceeded("no input token budget left for the model context window"));
    }

    // 1. Mandatory items: instructions, current user message, images.
    let instructions = build_instructions(&model.system_prompt, &req.guards);
    let images = i64::try_from(req.image_file_ids.len()).unwrap_or(i64::MAX);
    let mandatory = estimate_text_tokens(instructions.len(), eb)
        .saturating_add(estimate_text_tokens(req.user_message.len(), eb))
        .saturating_add(images.saturating_mul(i64::from(eb.image_token_budget)));
    if mandatory > budget {
        return Err(budget_exceeded("system prompt and current message exceed the model context budget"));
    }
    let mut remaining = budget - mandatory;
    let mut assembled = mandatory;

    // 2. Thread summary: kept if it fits.
    let mut input = Vec::with_capacity(req.recent.len() + 2);
    let mut summary_applied = None;
    if let Some(summary) = &req.summary {
        let text = summary_message_text(&summary.text);
        let est = estimate_text_tokens(text.len(), eb);
        if est <= remaining {
            remaining -= est;
            assembled += est;
            summary_applied = Some(summary.token_estimate);
            input.push(InputItem { role: InputRole::User, text, image_file_ids: Vec::new() });
        }
    }

    // 3. Recent messages: newest to oldest while they fit (system messages are never sent).
    let candidates: Vec<&HistoryMessage> = req.recent.iter().filter(|m| m.role != "system").collect();
    let mut kept: Vec<(&HistoryMessage, i64)> = Vec::new();
    for m in candidates.iter().rev() {
        let est = estimate_text_tokens(m.content.len(), eb);
        if est > remaining {
            break;
        }
        remaining -= est;
        kept.push((m, est));
    }
    kept.reverse();
    // Never start with an answer whose question was dropped: truncation removes whole turns.
    let lead = kept.iter().take_while(|(m, _)| m.role == "assistant").count();
    kept.drain(..lead);
    let messages_truncated = kept.len() < candidates.len();
    for (m, est) in kept {
        assembled += est;
        let role = if m.role == "assistant" { InputRole::Assistant } else { InputRole::User };
        input.push(InputItem { role, text: m.content.clone(), image_file_ids: Vec::new() });
    }

    // 4. Current user message with its images.
    input.push(InputItem { role: InputRole::User, text: req.user_message.clone(), image_file_ids: req.image_file_ids.clone() });

    Ok(ContextPlan { instructions, input, summary_applied, messages_truncated, assembled_tokens: assembled, effective_budget: limit })
}

/// `(created_at, id) > (t, id)` on the given columns.
pub(crate) fn after<C: ColumnTrait>(created: C, id_col: C, t: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(created.gt(t)).add(Condition::all().add(created.eq(t)).add(id_col.gt(id)))
}

/// `(created_at, id) <= (t, id)` on the given columns.
pub(crate) fn at_or_before<C: ColumnTrait>(created: C, id_col: C, t: OffsetDateTime, id: Uuid) -> Condition {
    Condition::any().add(created.lt(t)).add(Condition::all().add(created.eq(t)).add(id_col.lte(id)))
}

/// Loads the chat's committed summary (tenant scope).
///
/// # Errors
/// DB errors.
pub(crate) async fn load_summary(
    runner: &impl toolkit_db::secure::DBRunner,
    tenant_id: Uuid,
    chat_id: Uuid,
) -> Result<Option<thread_summary::Model>, DomainError> {
    use toolkit_db::secure::SecureEntityExt;
    Ok(thread_summary::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(chat_id)))
        .one(runner)
        .await?)
}

/// Loads the committed summary and up to `limit` recent non-deleted, non-compressed messages with
/// non-null `request_id` after the summary frontier and at or before `boundary`, excluding
/// `exclude_message_id`, in chronological order.
///
/// # Errors
/// DB errors.
pub async fn load_history(
    app: &AppServices,
    tenant_id: Uuid,
    chat_id: Uuid,
    boundary: Option<(OffsetDateTime, Uuid)>,
    exclude_message_id: Option<Uuid>,
    limit: u32,
) -> Result<(Option<SummaryContext>, Vec<HistoryMessage>), DomainError> {
    use toolkit_db::secure::SecureEntityExt;
    use message::Column as M;

    let conn = app.db.conn()?;
    let summary = load_summary(&conn, tenant_id, chat_id).await?.map(|s| SummaryContext {
        text: s.summary_text,
        token_estimate: i64::from(s.token_estimate),
        frontier_created_at: s.summarized_up_to_created_at,
        frontier_message_id: s.summarized_up_to_message_id,
    });
    if limit == 0 {
        return Ok((summary, Vec::new()));
    }

    let mut filter = Condition::all()
        .add(M::ChatId.eq(chat_id))
        .add(M::RequestId.is_not_null())
        .add(M::DeletedAt.is_null())
        .add(M::IsCompressed.eq(false));
    if let Some((t, id)) = boundary {
        filter = filter.add(at_or_before(M::CreatedAt, M::Id, t, id));
    }
    if let Some(s) = &summary {
        filter = filter.add(after(M::CreatedAt, M::Id, s.frontier_created_at, s.frontier_message_id));
    }
    if let Some(ex) = exclude_message_id {
        filter = filter.add(M::Id.ne(ex));
    }
    let rows = message::Entity::find()
        .secure()
        .scope_with(&AccessScope::for_tenant(tenant_id))
        .filter(filter)
        .order_by(M::CreatedAt, Order::Desc)
        .order_by(M::Id, Order::Desc)
        .limit(u64::from(limit))
        .all(&conn)
        .await?;
    let mut msgs: Vec<HistoryMessage> = rows
        .into_iter()
        .map(|m| HistoryMessage { id: m.id, role: m.role, content: m.content, created_at: m.created_at })
        .collect();
    msgs.reverse();
    Ok((summary, msgs))
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "context_test_support.rs"]
pub(crate) mod test_support;
