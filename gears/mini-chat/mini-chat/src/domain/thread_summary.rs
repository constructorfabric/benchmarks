//! Thread summary (DESIGN "Thread Summary Update", B.5.5): the trigger evaluated in the
//! finalization of a completed turn, the durable scheduling of the work, and the pure parts of the
//! summary request (prompt, fitting, response parsing). The outbox handler that runs the work is
//! `crate::infra::outbox::thread_summary`.

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use toolkit_db::DbTx;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::AccessScope;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use super::error::DomainError;
use super::stream::{SummaryTriggerInfo, TurnContext};
use crate::config::ThreadSummaryWorkerConfig;
use crate::infra::db::repo::messages::{self as message_repo, Position};
use crate::infra::db::repo::thread_summaries;
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::llm::{ProviderUsage, Role};
use crate::infra::outbox::{OutboxEnqueuer, OutboxRecord, ThreadSummaryTask};

/// `system_task_type` of the work item and of its usage event.
pub const SYSTEM_TASK_TYPE: &str = "thread_summary_update";
/// Summary model when `thread_summary_worker.summary_model_id` is empty.
pub const DEFAULT_SUMMARY_MODEL: &str = "gpt-4.1-mini";

/// The summary model id: `thread_summary_worker.summary_model_id`, [`DEFAULT_SUMMARY_MODEL`]
/// when empty.
#[must_use]
pub fn summary_model_id(cfg: &ThreadSummaryWorkerConfig) -> &str {
    if cfg.summary_model_id.is_empty() {
        DEFAULT_SUMMARY_MODEL
    } else {
        &cfg.summary_model_id
    }
}

/// The enabled entry of the summary model in the current policy catalog; `None` when it is
/// missing or disabled.
///
/// # Errors
/// Returns the policy lookup error (catalog not available).
pub async fn resolve_summary_model(
    policy: &dyn PolicyGateway,
    cfg: &ThreadSummaryWorkerConfig,
) -> Result<Option<ModelCatalogEntry>, DomainError> {
    let snapshot = policy.current_snapshot(DEFAULT_SUBJECT_ID).await?;
    let id = summary_model_id(cfg);
    Ok(snapshot
        .model_catalog
        .into_iter()
        .find(|m| m.id == id && m.enabled))
}

/// Opening of the request when the chat has no summary yet.
const OPENING_NEW: &str = "Summarize the following conversation:";
/// Opening of the request that merges an existing summary (followed by `<existing_summary>`).
const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
/// Follows the `<existing_summary>` block.
const NEW_MESSAGES: &str = "New messages to incorporate:";
/// Analysis instruction at the end of the request.
const ANALYSIS_INSTRUCTION: &str =
    "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:
1. Chronologically review each exchange, identifying:
   - The user's requests and questions
   - Key decisions, answers, and information shared
   - Any follow-up actions or commitments
   - Specific names, dates, numbers, URLs, or references mentioned
2. Verify accuracy and completeness.

Your summary MUST include these sections:

1. Conversation Purpose: The user's primary goals and recurring themes
2. Key Information Exchanged: Important facts, decisions, recommendations, and answers
3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections
4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit
5. Current Topic: What was being discussed most recently, with enough detail to continue naturally

Respond with an <analysis> block followed by a <summary> block.";

/// Whether the finalization of a completed turn evaluates to "summarize": the context was
/// truncated, or there is no summary yet and the assembled context reached `threshold_pct` % of
/// the effective budget.
#[must_use]
#[allow(clippy::integer_division)] // the threshold is `budget * pct / 100`, rounded down
pub fn evaluate_trigger(info: &SummaryTriggerInfo, threshold_pct: u32) -> bool {
    let threshold = info
        .effective_budget
        .saturating_mul(i64::from(threshold_pct))
        / 100;
    info.messages_truncated || (!info.summary_exists && info.assembled_tokens >= threshold)
}

/// Schedules the summary of the messages before `turn` inside the finalization transaction `tx`.
///
/// The frozen target is the latest live message before the turn's user message `(created_at,
/// id)`; the base is the stored frontier. No target, or a target not after the base: nothing is
/// enqueued (`(false, None)`). Otherwise a [`ThreadSummaryTask`] with a new `system_request_id`
/// is enqueued and its wake returned (fire it after the commit).
///
/// # Errors
/// Database or outbox errors (the finalization transaction fails with them).
pub async fn enqueue_if_needed(
    tx: &DbTx<'_>,
    enq: &OutboxEnqueuer,
    turn: &TurnContext,
) -> Result<(bool, Option<Wake>), DomainError> {
    schedule(tx, enq, turn.tenant_id, turn.chat_id, turn.user_message_id).await
}

/// [`enqueue_if_needed`] for the turn whose user message is `user_message_id`.
async fn schedule(
    tx: &DbTx<'_>,
    enq: &OutboxEnqueuer,
    tenant_id: Uuid,
    chat_id: Uuid,
    user_message_id: Uuid,
) -> Result<(bool, Option<Wake>), DomainError> {
    let scope = AccessScope::for_tenant(tenant_id);
    let Some(user_message) = message_repo::find(tx, &scope, chat_id, user_message_id).await? else {
        return Ok((false, None));
    };
    let Some(target) =
        message_repo::latest_before(tx, &scope, chat_id, Position::of(&user_message)).await?
    else {
        return Ok((false, None));
    };
    let base = thread_summaries::find_for_chat(tx, &scope, chat_id)
        .await?
        .map(|s| Position {
            created_at: s.summarized_up_to_created_at,
            id: s.summarized_up_to_message_id,
        });
    if base.is_some_and(|base| position_key(target) <= position_key(base)) {
        return Ok((false, None));
    }
    let task = ThreadSummaryTask {
        tenant_id,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: base.map(|b| b.created_at),
        base_frontier_message_id: base.map(|b| b.id),
        frozen_target_created_at: target.created_at,
        frozen_target_message_id: target.id,
        system_task_type: SYSTEM_TASK_TYPE.to_owned(),
    };
    let wake = enq
        .enqueue(tx, OutboxRecord::thread_summary(&task)?)
        .await?;
    Ok((true, Some(wake)))
}

/// The `(created_at, id)` message order.
fn position_key(p: Position) -> (time::OffsetDateTime, Uuid) {
    (p.created_at, p.id)
}

/// One message of the summarized range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryEntry {
    pub role: Role,
    pub content: String,
}

/// The user prompt of the summary request: the opening (with the existing summary when there is
/// one), one `User: …` / `Assistant: …` entry per message separated by blank lines (contents
/// longer than `content_limit` characters are cut and end with `...`; 0 = no limit), then the
/// analysis instruction.
#[must_use]
pub fn user_prompt(
    existing: Option<&str>,
    entries: &[SummaryEntry],
    content_limit: usize,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(entries.len() + 4);
    match existing {
        Some(summary) => {
            parts.push(OPENING_MERGE.to_owned());
            parts.push(format!(
                "<existing_summary>\n{summary}\n</existing_summary>"
            ));
            parts.push(NEW_MESSAGES.to_owned());
        }
        None => parts.push(OPENING_NEW.to_owned()),
    }
    parts.extend(entries.iter().map(|e| {
        let speaker = match e.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
        };
        format!("{speaker}: {}", cut(&e.content, content_limit))
    }));
    parts.push(ANALYSIS_INSTRUCTION.to_owned());
    parts.join("\n\n")
}

/// `text` cut to `limit` characters plus `...` when it is longer; 0 = no limit.
fn cut(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((end, _)) if limit > 0 => format!("{}...", &text[..end]),
        _ => text.to_owned(),
    }
}

/// Input budget of the summary model: `context_window - max_output_tokens`, capped by
/// `max_input_tokens` when > 0; `None` (no fitting) when `context_window` is 0.
#[must_use]
pub fn input_budget(
    context_window: u32,
    max_output_tokens: u32,
    max_input_tokens: u32,
) -> Option<i64> {
    if context_window == 0 {
        return None;
    }
    let window = i64::from(context_window) - i64::from(max_output_tokens);
    Some(match max_input_tokens {
        0 => window,
        max => window.min(i64::from(max)),
    })
}

/// Number of oldest messages to drop in one fitting or retry step: `ceil(n / 5)`, keeping at
/// least two messages (0 when `n <= 2`).
#[must_use]
pub fn drop_step(n: usize) -> usize {
    n.div_ceil(5).min(n.saturating_sub(2))
}

/// Index of the first entry kept so that the request (`system` + the user prompt) fits `budget`
/// at `bytes_per_token_conservative` bytes per token, dropping the oldest [`drop_step`] entries
/// per step.
#[must_use]
pub fn fit(
    system: &str,
    existing: Option<&str>,
    entries: &[SummaryEntry],
    content_limit: usize,
    budget: Option<i64>,
    budgets: &EstimationBudgets,
) -> usize {
    let Some(budget) = budget else {
        return 0;
    };
    let bytes_per_token = u64::from(budgets.bytes_per_token_conservative.max(1));
    let mut start = 0;
    loop {
        let prompt = user_prompt(existing, &entries[start..], content_limit);
        let bytes = u64::try_from(system.len() + prompt.len()).unwrap_or(u64::MAX);
        let tokens = i64::try_from(bytes.div_ceil(bytes_per_token)).unwrap_or(i64::MAX);
        let step = drop_step(entries.len() - start);
        if tokens <= budget || step == 0 {
            return start;
        }
        start += step;
    }
}

/// The text stored from a summary response: without `<analysis>…</analysis>` blocks, the
/// content of `<summary>…</summary>`, else the whole remaining text unless it still contains
/// `<analysis` / `<summary` markup (then empty); runs of blank lines collapsed, trimmed.
#[must_use]
pub fn parse_summary(text: &str) -> String {
    let mut rest = text.to_owned();
    while let Some(start) = rest.find("<analysis>") {
        let Some(len) = rest[start..].find("</analysis>") else {
            break;
        };
        rest.replace_range(start..start + len + "</analysis>".len(), "");
    }
    let body = match (rest.find("<summary>"), rest.find("</summary>")) {
        (Some(open), Some(close)) if close > open => {
            rest[open + "<summary>".len()..close].to_owned()
        }
        _ if rest.contains("<analysis") || rest.contains("<summary") => return String::new(),
        _ => rest,
    };
    collapse_blank_lines(&body)
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.trim().lines() {
        let blank = line.trim().is_empty();
        if blank && out.last().is_some_and(|l: &&str| l.is_empty()) {
            continue;
        }
        out.push(if blank { "" } else { line });
    }
    out.join("\n")
}

/// Stored `token_estimate`: `output_tokens - reasoning_tokens` of the call, or `ceil(bytes / 4)`
/// of `summary` when that difference is not positive.
#[must_use]
pub fn token_estimate(usage: Option<ProviderUsage>, summary: &str) -> i32 {
    let reported = usage.map_or(0, |u| u.output_tokens.saturating_sub(u.reasoning_tokens));
    let tokens = if reported > 0 {
        reported
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(assembled: i64, truncated: bool, exists: bool) -> SummaryTriggerInfo {
        SummaryTriggerInfo {
            assembled_tokens: assembled,
            effective_budget: 1000,
            messages_truncated: truncated,
            summary_exists: exists,
        }
    }

    #[test]
    fn trigger_rules() {
        // Truncation always triggers, with or without a summary.
        assert!(evaluate_trigger(&info(10, true, false), 80));
        assert!(evaluate_trigger(&info(10, true, true), 80));
        // No summary: the proactive threshold, inclusive.
        assert!(evaluate_trigger(&info(800, false, false), 80));
        assert!(!evaluate_trigger(&info(799, false, false), 80));
        assert!(evaluate_trigger(&info(500, false, false), 50));
        // An existing summary without truncation never triggers.
        assert!(!evaluate_trigger(&info(999, false, true), 80));
    }

    #[test]
    fn summary_parse_rules() {
        assert_eq!(
            parse_summary("<analysis>x</analysis>\n<summary>A\n\n\nB</summary>"),
            "A\n\nB"
        );
        assert_eq!(
            parse_summary("  plain text\n\n\n\nmore "),
            "plain text\n\nmore"
        );
        assert_eq!(parse_summary("<analysis>still thinking"), "");
        assert_eq!(parse_summary("<analysis>x</analysis>"), "");
        assert_eq!(parse_summary("<summary>unterminated"), "");
        assert_eq!(
            parse_summary("<analysis>a</analysis>\nKept text"),
            "Kept text"
        );
        // Whitespace-only lines count as blank.
        assert_eq!(parse_summary("<summary>A\n  \n\t\nB</summary>"), "A\n\nB");
    }

    fn entries(n: usize) -> Vec<SummaryEntry> {
        (0..n)
            .map(|i| SummaryEntry {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: format!("m{i}"),
            })
            .collect()
    }

    #[test]
    fn prompt_entries_and_content_limit() {
        let e = vec![
            SummaryEntry {
                role: Role::User,
                content: "h\u{e9}llo w\u{f6}rld".to_owned(),
            },
            SummaryEntry {
                role: Role::Assistant,
                content: "ok".to_owned(),
            },
        ];
        let prompt = user_prompt(None, &e, 5);
        assert!(
            prompt.starts_with("Summarize the following conversation:\n\nUser: h\u{e9}llo...\n\nAssistant: ok\n\nBefore providing your final summary"),
            "{prompt}"
        );
        assert!(user_prompt(None, &e, 0).contains("User: h\u{e9}llo w\u{f6}rld\n\n"));
    }

    #[test]
    fn fitting_drops_the_oldest_fifth_keeping_two() {
        assert_eq!(
            [
                drop_step(10),
                drop_step(8),
                drop_step(3),
                drop_step(2),
                drop_step(0)
            ],
            [2, 2, 1, 0, 0]
        );
        let e = entries(10);
        let b = EstimationBudgets::default();
        assert_eq!(
            fit("", None, &e, 0, None, &b),
            0,
            "no fitting without a context window"
        );
        assert_eq!(fit("", None, &e, 0, Some(1_000_000), &b), 0);
        // A budget nothing fits into: 10 -> 8 -> 6 -> 4 -> 3 -> 2 messages, never fewer.
        assert_eq!(fit("", None, &e, 0, Some(1), &b), 8);
        assert_eq!(input_budget(4096, 1024, 0), Some(3072));
        assert_eq!(input_budget(4096, 1024, 2000), Some(2000));
        assert_eq!(input_budget(0, 1024, 2000), None);
    }

    #[test]
    fn token_estimate_prefers_visible_output_tokens() {
        let usage = |output_tokens, reasoning_tokens| {
            Some(ProviderUsage {
                output_tokens,
                reasoning_tokens,
                ..ProviderUsage::default()
            })
        };
        assert_eq!(token_estimate(usage(40, 10), "x"), 30);
        assert_eq!(token_estimate(usage(10, 10), "12345"), 2);
        assert_eq!(token_estimate(None, "12345678"), 2);
    }

    /// `schedule` for the turn of `user_message`, in its own write transaction.
    async fn schedule_for(
        app: &crate::test_support::app::TestApp,
        tenant: Uuid,
        chat: Uuid,
        user_message: Uuid,
    ) -> bool {
        let outbox = std::sync::Arc::clone(&app.services.outbox);
        crate::infra::db::tx::write_tx_with_wakes(&app.services.db, move |tx, wakes| {
            let outbox = std::sync::Arc::clone(&outbox);
            Box::pin(async move {
                let (scheduled, wake) = schedule(tx, &outbox, tenant, chat, user_message).await?;
                if let Some(wake) = wake {
                    wakes.add(wake);
                }
                Ok(scheduled)
            })
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn schedule_needs_a_target_after_the_frontier() {
        use crate::infra::outbox::QueueKind;
        use crate::test_support::app::{TestApp, ctx};
        use crate::test_support::stream::{
            answer, create_chat, messages_of, script_provider, seed_thread_summary, stream_uri,
        };

        let app = TestApp::builder()
            .outbox_handler(
                QueueKind::ThreadSummary,
                std::sync::Arc::new(crate::infra::outbox::LoggingAckHandler),
            )
            .build()
            .await;
        let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
        let who = ctx(tenant, user);
        let chat = create_chat(&app, &who, None).await;
        script_provider(&app, answer(&["a"], 1, 1));
        for q in ["q1", "q2"] {
            app.stream(
                "POST",
                &stream_uri(chat),
                &who,
                serde_json::json!({ "content": q }),
            )
            .await
            .expect("send");
        }
        let rows = messages_of(&app, chat).await; // q1, a1, q2, a2

        // Nothing before the first user message.
        assert!(!schedule_for(&app, tenant, chat, rows[0].id).await);
        // No summary: everything up to a1.
        assert!(schedule_for(&app, tenant, chat, rows[2].id).await);
        // The stored frontier already is the target.
        seed_thread_summary(&app, &rows[1]).await;
        assert!(!schedule_for(&app, tenant, chat, rows[2].id).await);
        // From a later position (a2) the target (q2) is after the frontier.
        assert!(schedule_for(&app, tenant, chat, rows[3].id).await);
    }
}
