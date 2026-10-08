//! Context plan assembly and deterministic truncation (DESIGN §4 "Context
//! Plan Assembly and Truncation").

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use sea_orm::{ColumnTrait, Condition};
#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use time::OffsetDateTime;
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::quota::ToolPlan;
use crate::config::MiniChatConfig;
use crate::domain::billing;
use crate::domain::error::{DomainError, DomainResult};
use crate::infra::llm::types::{ContentPart, InputMessage, InputRole};
use crate::infra::storage::entity::{message, thread_summary};

/// Preamble prepended to the thread summary in the next turn's context.
pub const SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// A message order key `(created_at, id)`.
pub type OrderKey = (OffsetDateTime, Uuid);

/// Result of the assembly.
#[derive(Debug, Clone)]
pub struct ContextPlan {
    pub instructions: String,
    pub input: Vec<InputMessage>,
    /// Estimated size of the assembled request.
    pub assembled_tokens: i64,
    /// At least one recent message was dropped by truncation.
    pub messages_truncated: bool,
    /// Token estimate of the summary when it was included.
    pub summary_applied: Option<i32>,
    /// The summary row (included or not).
    pub summary: Option<thread_summary::Model>,
    /// `min(max_input_tokens, context_window - max_output)` (thread summary
    /// threshold base).
    pub effective_budget: i64,
}

fn est(text: &str, budgets: &EstimationBudgets) -> i64 {
    billing::estimate_text_tokens(text.len(), budgets)
}

/// System instructions: the model's system prompt plus the tool guards of
/// the tools sent.
#[must_use]
pub fn instructions(cfg: &MiniChatConfig, model: &ModelCatalogEntry, tools: ToolPlan) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !model.system_prompt.trim().is_empty() {
        parts.push(model.system_prompt.as_str());
    }
    if tools.file_search && !cfg.context.file_search_guard.is_empty() {
        parts.push(cfg.context.file_search_guard.as_str());
    }
    if tools.web_search && !cfg.context.web_search_guard.is_empty() {
        parts.push(cfg.context.web_search_guard.as_str());
    }
    parts.join("\n\n")
}

/// Latest non-deleted message of a chat (the snapshot boundary), optionally
/// excluding one message.
pub async fn latest_message<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    chat_id: Uuid,
    exclude_request_id: Option<Uuid>,
) -> DomainResult<Option<message::Model>> {
    let mut cond = Condition::all()
        .add(message::Column::ChatId.eq(chat_id))
        .add(message::Column::DeletedAt.is_null());
    if let Some(r) = exclude_request_id {
        cond = cond.add(
            Condition::any()
                .add(message::Column::RequestId.ne(r))
                .add(message::Column::RequestId.is_null()),
        );
    }
    Ok(message::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(cond)
        .order_by(message::Column::CreatedAt, sea_orm::Order::Desc)
        .order_by(message::Column::Id, sea_orm::Order::Desc)
        .limit(1)
        .one(runner)
        .await?)
}

/// Order-key condition `(created_at, id) <= key` (or `>`).
fn key_cmp(key: OrderKey, greater: bool) -> Condition {
    let (ts, id) = key;
    if greater {
        Condition::any().add(message::Column::CreatedAt.gt(ts)).add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(ts))
                .add(message::Column::Id.gt(id)),
        )
    } else {
        Condition::any().add(message::Column::CreatedAt.lt(ts)).add(
            Condition::all()
                .add(message::Column::CreatedAt.eq(ts))
                .add(message::Column::Id.lte(id)),
        )
    }
}

/// Inputs of the assembly.
pub struct AssemblyInputs<'a> {
    pub cfg: &'a MiniChatConfig,
    pub model: &'a ModelCatalogEntry,
    pub tools: ToolPlan,
    pub max_output_applied: i64,
    pub chat_id: Uuid,
    pub boundary: Option<OrderKey>,
    pub user_content: &'a str,
    pub images: Vec<ContentPart>,
}

/// Assemble the context plan.
///
/// # Errors
/// `ContextBudgetExceeded` when the mandatory items do not fit.
pub async fn assemble<R: DBRunner>(
    runner: &R,
    scope: &AccessScope,
    inputs: AssemblyInputs<'_>,
) -> DomainResult<ContextPlan> {
    let model = inputs.model;
    let budgets = &model.estimation_budgets;
    let context_window = i64::from(model.context_window);
    let max_out = inputs.max_output_applied;
    if context_window > 0 && max_out >= context_window {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let window_limit = if context_window > 0 {
        context_window - max_out
    } else {
        i64::MAX >> 2
    };
    let input_limit = if model.max_input_tokens > 0 {
        window_limit.min(i64::from(model.max_input_tokens))
    } else {
        window_limit
    };
    let mut deductions = i64::from(budgets.fixed_overhead_tokens);
    if inputs.tools.file_search {
        deductions += i64::from(budgets.tool_surcharge_tokens);
    }
    if inputs.tools.web_search {
        deductions += i64::from(budgets.web_search_surcharge_tokens);
    }
    if inputs.tools.code_interpreter {
        deductions += i64::from(budgets.code_interpreter_surcharge_tokens);
    }
    if deductions >= input_limit {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let token_budget = input_limit - deductions;

    let instructions = instructions(inputs.cfg, model, inputs.tools);
    let image_count = i64::try_from(inputs.images.len()).unwrap_or(i64::MAX);
    let mandatory = est(&instructions, budgets)
        + est(inputs.user_content, budgets)
        + image_count * i64::from(budgets.image_token_budget);
    if mandatory > token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    let mut used = mandatory;

    // Thread summary.
    let summary = thread_summary::Entity::find()
        .secure()
        .scope_with(scope)
        .filter(Condition::all().add(thread_summary::Column::ChatId.eq(inputs.chat_id)))
        .one(runner)
        .await?;
    let mut summary_msg = None;
    let mut summary_applied = None;
    if let Some(s) = &summary {
        let text = format!("{SUMMARY_PREAMBLE}\n\n{}", s.summary_text);
        let t = est(&text, budgets);
        if used + t <= token_budget {
            used += t;
            summary_msg = Some(InputMessage::text(InputRole::User, text));
            summary_applied = Some(s.token_estimate);
        }
    }

    // Recent messages.
    let limit = u64::from(inputs.cfg.context.recent_messages_limit);
    let recent: Vec<message::Model> = if let Some(boundary) = inputs.boundary
        && limit > 0
    {
        let mut cond = Condition::all()
            .add(message::Column::ChatId.eq(inputs.chat_id))
            .add(message::Column::RequestId.is_not_null())
            .add(message::Column::DeletedAt.is_null())
            .add(message::Column::IsCompressed.eq(false))
            .add(key_cmp(boundary, false));
        if let Some(s) = &summary {
            cond = cond.add(key_cmp(
                (s.summarized_up_to_created_at, s.summarized_up_to_message_id),
                true,
            ));
        }
        message::Entity::find()
            .secure()
            .scope_with(scope)
            .filter(cond)
            .order_by(message::Column::CreatedAt, sea_orm::Order::Desc)
            .order_by(message::Column::Id, sea_orm::Order::Desc)
            .limit(limit)
            .all(runner)
            .await?
    } else {
        Vec::new()
    };
    // newest first; keep while fits
    let mut kept: Vec<message::Model> = Vec::new();
    let mut truncated = false;
    for m in recent {
        if truncated {
            if m.role != "system" {
                // already dropped; just flag
            }
            continue;
        }
        let t = est(&m.content, budgets);
        if used + t <= token_budget {
            used += t;
            kept.push(m);
        } else {
            truncated = true;
        }
    }
    kept.reverse();
    // never start with an assistant answer without its question
    while kept.first().is_some_and(|m| m.role == "assistant") && truncated {
        let m = kept.remove(0);
        used -= est(&m.content, budgets);
    }

    let mut input = Vec::new();
    if let Some(s) = summary_msg {
        input.push(s);
    }
    for m in kept {
        let role = if m.role == "assistant" {
            InputRole::Assistant
        } else {
            InputRole::User
        };
        input.push(InputMessage::text(role, m.content));
    }
    let mut current = vec![ContentPart::Text(inputs.user_content.to_owned())];
    current.extend(inputs.images);
    input.push(InputMessage {
        role: InputRole::User,
        content: current,
    });

    Ok(ContextPlan {
        instructions,
        input,
        assembled_tokens: used,
        messages_truncated: truncated,
        summary_applied,
        summary,
        effective_budget: input_limit,
    })
}
