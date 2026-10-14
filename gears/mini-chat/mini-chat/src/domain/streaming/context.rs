//! Context plan assembly and truncation
//! (`cpt-cf-mini-chat-algo-message-streaming-context-assembly`).

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::quota::estimation::text_tokens;
use crate::domain::streaming::budget::compute_budget;
use crate::domain::streaming::tools::ToolSet;

/// The preamble of the thread summary item (DESIGN B.5.5), followed by the summary text.
pub const THREAD_SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// The stored thread summary of the chat.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummaryRow {
    pub summary_text: String,
    /// The stored `thread_summaries.token_estimate`.
    pub token_estimate: i32,
    /// The inclusive frontier: `(summarized_up_to_created_at, summarized_up_to_message_id)`.
    pub frontier_created_at: OffsetDateTime,
    pub frontier_message_id: Uuid,
}

/// The role of a context item.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRole {
    User,
    Assistant,
}

/// A recent message selected for the context, in chronological order.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextMessage {
    pub id: Uuid,
    pub role: ContextRole,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// An image of the current turn: the internal file ids of the attachment.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the attachment column names of DESIGN 3.7"
)]
pub struct ImageInput {
    pub attachment_id: Uuid,
    /// The internal `provider_file_id` of the primary (RAG) provider, used by the `OpenAI` family
    /// adapters.
    pub provider_file_id: String,
    /// The file id of the uploaded secondary Anthropic copy, used by the Anthropic Messages
    /// adapter; `None` when the attachment has no usable secondary copy.
    pub secondary_file_id: Option<String>,
}

/// The current user message with its images (in `attachment_ids` order).
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentMessage {
    pub content: String,
    pub images: Vec<ImageInput>,
}

/// An item of the plan with its token estimate.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    /// The source message of a recent item, `None` for the summary item.
    pub message_id: Option<Uuid>,
    pub role: ContextRole,
    pub text: String,
    pub tokens: i64,
}

/// The current user item with its images.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserItem {
    pub text: String,
    pub images: Vec<ImageInput>,
    pub tokens: i64,
}

/// The `ContextPlan` in the normative order: system instructions, thread summary item, recent
/// messages, current user item with its images.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// The catalog `system_prompt` followed by the guards of the tool set; never persisted.
    pub system_instructions: String,
    /// One `user`-role item: the preamble followed by the summary text.
    pub thread_summary: Option<ContextItem>,
    /// Chronological, starting with a `user` message.
    pub recent: Vec<ContextItem>,
    pub user: UserItem,
}

/// The result of the assembly.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextAssembly {
    pub plan: ContextPlan,
    /// At least one recent message was dropped.
    pub messages_truncated: bool,
    /// The sum of the estimates of the kept items.
    pub assembled_context_tokens: i64,
    /// The `effective_budget` of the context budget.
    pub effective_budget: i64,
    /// The `token_estimate` of the summary, only when the summary was kept.
    pub summary_token_estimate: Option<i32>,
}

/// Assemble the plan within the budget of the effective model: mandatory items (system
/// instructions, current message, images) never truncated; the summary kept when it fits; the
/// recent messages kept newest to oldest until the first that does not fit; a kept range that
/// starts with an `assistant` message loses it (repeatedly). Deterministic. `recent` is
/// chronological (already limited and bounded by the snapshot boundary and the frontier).
///
/// # Errors
///
/// [`DomainError::ContextBudgetExceeded`] when the context budget rejects or the mandatory items
/// do not fit the token budget.
pub fn assemble_context(
    entry: &ModelCatalogEntry,
    tools: &ToolSet,
    max_output_tokens_applied: i64,
    summary: Option<&ThreadSummaryRow>,
    recent: &[ContextMessage],
    current: &CurrentMessage,
) -> Result<ContextAssembly, DomainError> {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-budget
    // the budgets of the effective model; a budget rejection is returned as it is
    let budget = compute_budget(entry, max_output_tokens_applied, tools)?;
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-budget
    let budgets = &entry.estimation_budgets;
    // the text estimate of the token estimation algorithm: no provider tokenizer is used
    let estimate = |text: &str| text_tokens(budgets, text.len() as u64);

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-system
    // the catalog system prompt followed by the guards of the tool set, which the set keeps in
    // the order `file_search`, `web_search`, `search_knowledge`; configuration, never a
    // `messages` row
    let mut system_instructions = entry.system_prompt.clone();
    for guard in &tools.guards {
        if !system_instructions.is_empty() {
            system_instructions.push_str("\n\n");
        }
        system_instructions.push_str(guard);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-system
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-estimate
    let system_tokens = estimate(&system_instructions);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-estimate

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-user-item
    // the current message and one image input per referenced image, in `attachment_ids`
    // order; each image costs `image_token_budget`
    let image_tokens = i64::try_from(current.images.len())
        .unwrap_or(i64::MAX)
        .saturating_mul(i64::from(budgets.image_token_budget));
    let user = UserItem {
        text: current.content.clone(),
        images: current.images.clone(),
        tokens: estimate(&current.content).saturating_add(image_tokens),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-user-item

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-mandatory-overflow
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-return-mandatory
    // the system prompt, the guards, the message and its images are never truncated
    let mandatory = system_tokens.saturating_add(user.tokens);
    if mandatory > budget.token_budget {
        return Err(DomainError::ContextBudgetExceeded);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-return-mandatory
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-mandatory-overflow
    let mut remaining = budget.token_budget - mandatory;

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-summary-item
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-summary-fit
    // one `user`-role item: the preamble (which counts toward the estimate) followed by the
    // summary text; kept when it fits after the mandatory items, otherwise dropped entirely
    let thread_summary = summary.and_then(|row| {
        let text = format!("{THREAD_SUMMARY_PREAMBLE}\n\n{}", row.summary_text);
        let tokens = estimate(&text);
        (tokens <= remaining).then(|| {
            remaining -= tokens;
            (
                ContextItem {
                    message_id: None,
                    role: ContextRole::User,
                    text,
                    tokens,
                },
                row.token_estimate,
            )
        })
    });
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-summary-fit
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-summary-item

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-walk-recent
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-keep-or-drop
    // newest to oldest: a message is kept while it fits; the first that does not fit is dropped
    // together with every older one
    let mut kept: Vec<ContextItem> = Vec::new();
    for message in recent.iter().rev() {
        let tokens = estimate(&message.content);
        if tokens > remaining {
            break;
        }
        remaining -= tokens;
        kept.push(ContextItem {
            message_id: Some(message.id),
            role: message.role,
            text: message.content.clone(),
            tokens,
        });
    }
    kept.reverse();
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-keep-or-drop
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-walk-recent
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-whole-turns
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-drop-leading-assistant
    // an answer is never sent without its question: truncation removes whole turns
    let leading_assistants = kept
        .iter()
        .take_while(|item| item.role == ContextRole::Assistant)
        .count();
    kept.drain(..leading_assistants);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-drop-leading-assistant
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-whole-turns
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-truncated-flag
    let messages_truncated = kept.len() < recent.len();
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-truncated-flag

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-assembled-tokens
    let assembled_context_tokens = [
        system_tokens,
        thread_summary.as_ref().map_or(0, |(item, _)| item.tokens),
        kept.iter().map(|item| item.tokens).sum::<i64>(),
        user.tokens,
    ]
    .into_iter()
    .fold(0_i64, i64::saturating_add);
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-assembled-tokens
    let summary_token_estimate = thread_summary.as_ref().map(|(_, estimate)| *estimate);

    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-order
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-deterministic
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-return
    // system instructions, thread summary item, recent messages, current user item with its
    // images; a pure function of its inputs, so the same boundary, history, summary, catalog
    // entry and tool set always yield the same plan
    Ok(ContextAssembly {
        plan: ContextPlan {
            system_instructions,
            thread_summary: thread_summary.map(|(item, _)| item),
            recent: kept,
            user,
        },
        messages_truncated,
        assembled_context_tokens,
        effective_budget: budget.effective_budget,
        summary_token_estimate,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-return
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-deterministic
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-context-assembly:p1:inst-ctx-order
}
