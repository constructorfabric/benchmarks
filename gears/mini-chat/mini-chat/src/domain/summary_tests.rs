#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use mini_chat_sdk::UsageTokens;
use uuid::Uuid;

use super::prompt::*;
use super::*;
use crate::clock;
use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, ThreadSummaryWorkerConfig};
use crate::domain::context::HistoryMessage;
use crate::domain::context::test_support::*;
use crate::infra::outbox::PAYLOAD_THREAD_SUMMARY;
use crate::testing::{STANDARD, TENANT_A, TestApp, USER_A1, test_catalog};

fn hm(role: &str, content: &str) -> HistoryMessage {
    HistoryMessage { id: Uuid::new_v4(), role: role.to_owned(), content: content.to_owned(), created_at: clock::now() }
}

// ───────────────────────────── trigger ─────────────────────────────

#[test]
fn trigger_conditions() {
    let t = |messages_truncated, assembled_tokens, summary_exists| SummaryTrigger {
        messages_truncated,
        assembled_tokens,
        effective_budget: 1000,
        summary_exists,
    };
    assert!(t(false, 800, false).fires(80), "proactive at exactly the threshold");
    assert!(!t(false, 799, false).fires(80));
    assert!(!t(false, 999, true).fires(80), "existing summary without truncation never fires");
    assert!(t(true, 10, true).fires(80), "truncation always fires");
    assert!(t(true, 10, false).fires(80));
}

// ───────────────────────────── prompt ─────────────────────────────

#[test]
fn system_prompt_fallback_chain() {
    let mut model = test_catalog().into_iter().find(|m| m.id == STANDARD).unwrap();
    let mut cfg = ThreadSummaryWorkerConfig::default();
    model.thread_summary_prompt = "Catalog prompt".into();
    cfg.summary_system_prompt = "Config prompt".into();
    assert_eq!(system_prompt(&model, &cfg), "Catalog prompt");
    model.thread_summary_prompt = "  ".into();
    assert_eq!(system_prompt(&model, &cfg), "Config prompt");
    cfg.summary_system_prompt = String::new();
    assert_eq!(system_prompt(&model, &cfg), DEFAULT_SUMMARY_SYSTEM_PROMPT);
}

#[test]
fn content_truncation() {
    assert_eq!(truncate_content("abcdef", 3), "abc...");
    assert_eq!(truncate_content("abc", 3), "abc");
    assert_eq!(truncate_content("abcdef", 0), "abcdef");
    assert_eq!(truncate_content("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 2), "\u{e9}\u{e9}...", "limit counts characters");
}

#[test]
fn entries_skip_system_messages() {
    let msgs = vec![hm("user", "hello"), hm("system", "ignored"), hm("assistant", "hi there")];
    assert_eq!(entries(&msgs, 0), vec!["User: hello".to_owned(), "Assistant: hi there".to_owned()]);
    assert_eq!(entries(&msgs, 2), vec!["User: he...".to_owned(), "Assistant: hi...".to_owned()]);
}

#[test]
fn user_prompt_without_summary() {
    let p = user_prompt(None, &["User: a".into(), "Assistant: b".into()]);
    assert!(p.starts_with("Summarize the following conversation:\n\nUser: a\n\nAssistant: b\n\n"));
    assert!(p.ends_with(ANALYSIS_INSTRUCTION));
    assert!(!p.contains("<existing_summary>"));
}

#[test]
fn user_prompt_with_existing_summary() {
    let p = user_prompt(Some("Old summary"), &["User: a".into()]);
    assert!(p.starts_with("The existing summary below covers the earlier conversation."));
    assert!(p.contains("IMPORTANT: Keep the summary concise."));
    assert!(p.contains("<existing_summary>\nOld summary\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: a\n\n"));
    assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
}

#[test]
fn fitting_drops_oldest_fifth_keeping_two() {
    let mut model = test_catalog().into_iter().find(|m| m.id == STANDARD).unwrap();
    model.estimation_budgets.bytes_per_token_conservative = 1;
    let base_len = i64::try_from(user_prompt(None, &[]).len()).unwrap();
    let mut entries: Vec<String> = (0..10).map(|i| format!("User: {i:0>96}")).collect(); // 102 bytes each
    // Room for system (0) + base + about 7 entries (each entry adds 102 + 2 separator bytes).
    model.max_input_tokens = 0;
    model.max_output_tokens = 0;
    model.context_window = u32::try_from(base_len + 7 * 104).unwrap();
    let dropped = fit_entries(&mut entries, None, "", &model);
    assert_eq!(dropped, 4, "two steps of ceil(n/5): 2 then 2");
    assert_eq!(entries.len(), 6);
    assert!(entries[0].ends_with('4'));

    // Never below two messages.
    let mut entries: Vec<String> = (0..5).map(|i| format!("User: {i:0>96}")).collect();
    model.context_window = 10;
    assert_eq!(fit_entries(&mut entries, None, "", &model), 3);
    assert_eq!(entries.len(), 2);

    // No fitting without a context window.
    let mut entries: Vec<String> = (0..5).map(|i| format!("User: {i}")).collect();
    model.context_window = 0;
    assert_eq!(fit_entries(&mut entries, None, "", &model), 0);
    assert_eq!(entries.len(), 5);
}

#[test]
fn input_budget_caps_by_max_input() {
    let mut model = test_catalog().into_iter().find(|m| m.id == STANDARD).unwrap();
    model.context_window = 1000;
    model.max_output_tokens = 200;
    model.max_input_tokens = 0;
    assert_eq!(input_budget(&model), Some(800));
    model.max_input_tokens = 500;
    assert_eq!(input_budget(&model), Some(500));
    model.context_window = 0;
    assert_eq!(input_budget(&model), None);
}

#[test]
fn parse_summary_variants() {
    assert_eq!(parse_summary("<analysis>think</analysis>\n<summary>The summary</summary>"), "The summary");
    assert_eq!(parse_summary("<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB", "blank-line runs collapsed");
    assert_eq!(parse_summary("<analysis>x</analysis>Plain text answer"), "Plain text answer");
    assert_eq!(parse_summary("Just text"), "Just text");
    assert_eq!(parse_summary("<analysis>unterminated"), "");
    assert_eq!(parse_summary("<summary>unterminated"), "");
    assert_eq!(parse_summary("<analysis>a</analysis><summary>  </summary>"), "");
    assert_eq!(parse_summary(""), "");
}

#[test]
fn token_estimate_rules() {
    let u = |out, reasoning| UsageTokens { input_tokens: 1, output_tokens: out, reasoning_tokens: reasoning, ..Default::default() };
    assert_eq!(token_estimate(Some(&u(30, 10)), "abc"), 20);
    assert_eq!(token_estimate(Some(&u(10, 10)), "abcdefghi"), 3, "non-positive difference -> ceil(bytes/4)");
    assert_eq!(token_estimate(None, "abcd"), 1);
}

// ───────────────────────────── maybe_enqueue ─────────────────────────────

const TRUNCATED: SummaryTrigger =
    SummaryTrigger { messages_truncated: true, assembled_tokens: 10, effective_budget: 1000, summary_exists: false };

/// Runs `maybe_enqueue` in a transaction; returns whether something was enqueued and the pending
/// thread-summary messages seen inside the transaction.
async fn run_enqueue(
    app: &Arc<AppServices>,
    chat_id: Uuid,
    causing: Uuid,
    trigger: SummaryTrigger,
) -> (bool, Vec<PendingMessage>) {
    let app2 = Arc::clone(app);
    let (enqueued, pending) = app
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let wake = maybe_enqueue(&app2, tx, TENANT_A, chat_id, causing, &trigger).await?;
                let pending: Vec<PendingMessage> = pending_messages(tx)
                    .await
                    .into_iter()
                    .filter(|m| m.payload_type == PAYLOAD_THREAD_SUMMARY)
                    .collect();
                Ok::<_, DomainError>((wake.is_some(), pending))
            })
        })
        .await
        .unwrap();
    (enqueued, pending)
}

#[tokio::test]
async fn enqueue_disabled_by_config() {
    let t = TestApp::with_config(|c| c.thread_summary_worker.enabled = false).await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let (enqueued, pending) = run_enqueue(&t.app, chat, rid, TRUNCATED).await;
    assert!(!enqueued);
    assert!(pending.is_empty());
}

#[tokio::test]
async fn enqueue_not_fired_below_threshold_or_with_summary() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let below = SummaryTrigger { messages_truncated: false, assembled_tokens: 799, effective_budget: 1000, summary_exists: false };
    assert!(!run_enqueue(&t.app, chat, rid, below).await.0);
    let with_summary = SummaryTrigger { messages_truncated: false, assembled_tokens: 999, effective_budget: 1000, summary_exists: true };
    assert!(!run_enqueue(&t.app, chat, rid, with_summary).await.0);
}

#[tokio::test]
async fn enqueue_payload_on_threshold() {
    let t = TestApp::with_config(|c| c.thread_summary_worker.summary_model_id = "missing-model".into()).await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (_, _, a2) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q3", "a3").await;
    let at_threshold = SummaryTrigger { messages_truncated: false, assembled_tokens: 800, effective_budget: 1000, summary_exists: false };
    let (enqueued, pending) = run_enqueue(&t.app, chat, rid, at_threshold).await;
    assert!(enqueued);
    assert_eq!(pending.len(), 1);
    let m = &pending[0];
    assert_eq!(m.queue, "mini-chat.thread_summary");
    assert_eq!(m.partition, i64::from(t.app.outbox.partition_for(chat)));
    let p: ThreadSummaryPayload = serde_json::from_value(m.json.clone()).unwrap();
    assert_eq!(p.tenant_id, TENANT_A);
    assert_eq!(p.chat_id, chat);
    assert_eq!(p.system_task_type, "thread_summary_update");
    assert_eq!(p.system_request_id.get_version_num(), 4);
    assert_ne!(p.system_request_id, rid);
    assert_eq!(p.base_frontier_created_at, None);
    assert_eq!(p.base_frontier_message_id, None);
    assert_eq!(p.frozen_target_message_id, a2.id, "target = latest message before the causing turn");
    assert_eq!(p.frozen_target_created_at, a2.created_at);
    assert!(m.json.get("base_frontier_created_at").is_some_and(serde_json::Value::is_null));
}

#[tokio::test]
async fn enqueue_with_existing_summary_on_truncation() {
    let t = TestApp::with_config(|c| c.thread_summary_worker.summary_model_id = "missing-model".into()).await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (_, _, a1) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (_, _, a2) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q3", "a3").await;
    insert_summary(&t.app, TENANT_A, chat, "s", (a1.created_at, a1.id), 1).await;
    let trig = SummaryTrigger { summary_exists: true, ..TRUNCATED };
    let (enqueued, pending) = run_enqueue(&t.app, chat, rid, trig).await;
    assert!(enqueued);
    let p: ThreadSummaryPayload = serde_json::from_value(pending[0].json.clone()).unwrap();
    assert_eq!(p.base_frontier_created_at, Some(a1.created_at));
    assert_eq!(p.base_frontier_message_id, Some(a1.id));
    assert_eq!(p.frozen_target_message_id, a2.id);
}

#[tokio::test]
async fn enqueue_skipped_without_earlier_message() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (enqueued, pending) = run_enqueue(&t.app, chat, rid, TRUNCATED).await;
    assert!(!enqueued);
    assert!(pending.is_empty());
}

#[tokio::test]
async fn enqueue_skipped_when_frontier_at_target() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (_, _, a2) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let (rid, _, _) = insert_turn(&t.app, TENANT_A, chat, "q3", "a3").await;
    insert_summary(&t.app, TENANT_A, chat, "s", (a2.created_at, a2.id), 1).await;
    let (enqueued, pending) = run_enqueue(&t.app, chat, rid, TRUNCATED).await;
    assert!(!enqueued);
    assert!(pending.is_empty());
}

#[tokio::test]
async fn enqueued_work_produces_summary_end_to_end() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (_, u1, a1) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (rid, u2, a2) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    let app2 = Arc::clone(&t.app);
    let wake = t
        .app
        .db
        .transaction(move |tx| Box::pin(async move { maybe_enqueue(&app2, tx, TENANT_A, chat, rid, &TRUNCATED).await }))
        .await
        .unwrap()
        .expect("enqueued");
    wake.fire();
    let app = Arc::clone(&t.app);
    t.eventually("summary row created", || {
        let app = Arc::clone(&app);
        async move { get_summary(&app, chat).await.is_some() }
    })
    .await;
    let s = get_summary(&t.app, chat).await.unwrap();
    assert_eq!(s.summary_text, "Summary text");
    assert_eq!((s.summarized_up_to_created_at, s.summarized_up_to_message_id), (a1.created_at, a1.id));
    let msgs = get_messages(&t.app, chat).await;
    let compressed: Vec<(Uuid, bool)> = msgs.iter().map(|m| (m.id, m.is_compressed)).collect();
    assert_eq!(compressed, vec![(u1.id, true), (a1.id, true), (u2.id, false), (a2.id, false)]);
}

// ───────────────────────────── invalidate_for_mutation ─────────────────────────────

async fn run_invalidate(app: &Arc<AppServices>, chat: Uuid, at: (OffsetDateTime, Uuid)) {
    app.db
        .transaction(move |tx| Box::pin(async move { invalidate_for_mutation(tx, TENANT_A, chat, at.0, at.1).await }))
        .await
        .unwrap();
}

#[tokio::test]
async fn invalidate_keeps_summary_before_mutated_turn() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (_, u1, a1) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (_, u2, _) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    insert_summary(&t.app, TENANT_A, chat, "s", (a1.created_at, a1.id), 1).await;
    set_message_flags(&t.app, u1.id, false, true).await;
    set_message_flags(&t.app, a1.id, false, true).await;

    run_invalidate(&t.app, chat, (u2.created_at, u2.id)).await;
    assert!(get_summary(&t.app, chat).await.is_some());
    assert_eq!(get_messages(&t.app, chat).await.iter().filter(|m| m.is_compressed).count(), 2);
}

#[tokio::test]
async fn invalidate_deletes_summary_covering_mutated_turn() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (_, u1, a1) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    let (_, u2, a2) = insert_turn(&t.app, TENANT_A, chat, "q2", "a2").await;
    // frontier after the user message of turn 2
    insert_summary(&t.app, TENANT_A, chat, "s", (a2.created_at, a2.id), 1).await;
    for id in [u1.id, a1.id, u2.id, a2.id] {
        set_message_flags(&t.app, id, false, true).await;
    }
    run_invalidate(&t.app, chat, (u2.created_at, u2.id)).await;
    assert!(get_summary(&t.app, chat).await.is_none());
    assert!(get_messages(&t.app, chat).await.iter().all(|m| !m.is_compressed));
}

#[tokio::test]
async fn invalidate_deletes_summary_with_frontier_at_user_message() {
    let t = TestApp::new().await;
    let chat = insert_chat(&t.app, TENANT_A, USER_A1, STANDARD).await;
    let (_, u1, a1) = insert_turn(&t.app, TENANT_A, chat, "q1", "a1").await;
    insert_summary(&t.app, TENANT_A, chat, "s", (u1.created_at, u1.id), 1).await;
    set_message_flags(&t.app, u1.id, false, true).await;
    let _ = a1;
    run_invalidate(&t.app, chat, (u1.created_at, u1.id)).await;
    assert!(get_summary(&t.app, chat).await.is_none());
    assert!(get_messages(&t.app, chat).await.iter().all(|m| !m.is_compressed));
    // no summary at all: no-op
    run_invalidate(&t.app, chat, (u1.created_at, u1.id)).await;
}
