#![allow(clippy::unwrap_used, clippy::expect_used)]

use mini_chat_sdk::{EstimationBudgets, ModelCatalogEntry};
use uuid::Uuid;

use super::test_support::*;
use super::*;
use crate::clock;
use crate::domain::error::DomainError;
use crate::testing::{TENANT_A, TINY, TestApp, USER_A1, test_catalog};

/// Model with simple estimation: `tokens = bytes + overhead` (bpt 1, no margin).
fn model(window: u32, max_input: u32, overhead: u32) -> ModelCatalogEntry {
    let mut m = test_catalog().into_iter().find(|m| m.id == TINY).unwrap();
    m.context_window = window;
    m.max_input_tokens = max_input;
    m.system_prompt = String::new();
    m.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: overhead,
        safety_margin_pct: 0,
        image_token_budget: 50,
        ..EstimationBudgets::default()
    };
    m
}

fn msg(role: &str, content: &str) -> HistoryMessage {
    HistoryMessage { id: Uuid::new_v4(), role: role.to_owned(), content: content.to_owned(), created_at: clock::now() }
}

fn req(model: ModelCatalogEntry, max_out: i64, user: &str) -> ContextRequest {
    ContextRequest {
        model,
        max_output_tokens_applied: max_out,
        guards: Vec::new(),
        summary: None,
        recent: Vec::new(),
        user_message: user.to_owned(),
        image_file_ids: Vec::new(),
        surcharge_tokens: 0,
    }
}

fn assert_budget_exceeded(r: Result<ContextPlan, DomainError>) {
    match r {
        Err(DomainError::OutOfRange { field, reason, .. }) => {
            assert_eq!(field, "context");
            assert_eq!(reason, "CONTEXT_BUDGET_EXCEEDED");
        }
        Err(e) => panic!("unexpected error {e:?}"),
        Ok(p) => panic!("expected CONTEXT_BUDGET_EXCEEDED, got plan {:?}", p.input),
    }
}

fn texts(plan: &ContextPlan) -> Vec<String> {
    plan.input.iter().map(|i| i.text.clone()).collect()
}

#[test]
fn estimate_matches_preflight_formula() {
    let b = EstimationBudgets::default(); // bpt 4, overhead 100, margin 10
    assert_eq!(estimate_text_tokens(0, &b), 110);
    // ceil(10/4)=3 + 100 = 103 * 1.1 = 113.3 -> 114
    assert_eq!(estimate_text_tokens(10, &b), 114);
    let zero_bpt = EstimationBudgets { bytes_per_token_conservative: 0, fixed_overhead_tokens: 0, safety_margin_pct: 0, ..b };
    assert_eq!(estimate_text_tokens(7, &zero_bpt), 7);
}

#[test]
fn effective_budget_uses_min_of_input_limit_and_window() {
    let p = assemble(&req(model(1000, 0, 0), 200, "hi")).unwrap();
    assert_eq!(p.effective_budget, 800, "max_input_tokens 0 = no separate limit");
    let p = assemble(&req(model(1000, 500, 0), 200, "hi")).unwrap();
    assert_eq!(p.effective_budget, 500);
    let p = assemble(&req(model(1000, 900, 0), 200, "hi")).unwrap();
    assert_eq!(p.effective_budget, 800);
}

#[test]
fn surcharges_and_overhead_reduce_budget() {
    // budget = 800 - 100 (surcharge) - 10 (overhead) = 690; mandatory = (0+10) + (L+10).
    let mut r = req(model(1000, 0, 10), 200, &"x".repeat(670));
    r.surcharge_tokens = 100;
    let p = assemble(&r).unwrap();
    assert_eq!(p.assembled_tokens, 690);
    assert_eq!(p.effective_budget, 800, "effective budget excludes surcharges and overhead");
    r.user_message = "x".repeat(671);
    assert_budget_exceeded(assemble(&r));
}

#[test]
fn non_positive_budget_is_rejected() {
    // max_output_tokens_applied >= context_window
    assert_budget_exceeded(assemble(&req(model(1000, 0, 0), 1000, "hi")));
    assert_budget_exceeded(assemble(&req(model(1000, 0, 0), 1200, "hi")));
    // deductions reach the input limit
    let mut r = req(model(1000, 0, 0), 200, "hi");
    r.surcharge_tokens = 800;
    assert_budget_exceeded(assemble(&r));
    let r = req(model(1000, 0, 800), 200, "hi");
    assert_budget_exceeded(assemble(&r));
}

#[test]
fn oversized_mandatory_items_are_rejected() {
    let mut m = model(1000, 0, 0);
    m.system_prompt = "s".repeat(500);
    let r = req(m.clone(), 200, &"u".repeat(301));
    assert_budget_exceeded(assemble(&r));
    let r = req(m.clone(), 200, &"u".repeat(300));
    assert_eq!(assemble(&r).unwrap().assembled_tokens, 800);
    // images count toward the mandatory items
    let mut r = req(m, 200, &"u".repeat(251));
    r.image_file_ids = vec!["file-1".into()];
    assert_budget_exceeded(assemble(&r));
}

#[test]
fn instructions_are_system_prompt_plus_guards() {
    let mut m = model(1000, 0, 0);
    m.system_prompt = "You are helpful.".into();
    let mut r = req(m, 200, "hi");
    r.guards = vec!["Guard A".into(), "Guard B".into()];
    let p = assemble(&r).unwrap();
    assert_eq!(p.instructions, "You are helpful.\n\nGuard A\n\nGuard B");
    assert!(p.instructions.ends_with("Guard B"));
}

#[test]
fn history_order_roles_and_tokens() {
    let mut r = req(model(1000, 0, 0), 200, "now");
    r.recent = vec![msg("user", "q1"), msg("assistant", "a1"), msg("user", "q2"), msg("assistant", "a2")];
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec!["q1", "a1", "q2", "a2", "now"]);
    let roles: Vec<InputRole> = p.input.iter().map(|i| i.role).collect();
    assert_eq!(roles, vec![InputRole::User, InputRole::Assistant, InputRole::User, InputRole::Assistant, InputRole::User]);
    assert!(!p.messages_truncated);
    assert_eq!(p.summary_applied, None);
    assert_eq!(p.assembled_tokens, 2 * 4 + 3);
}

#[test]
fn summary_kept_when_it_fits() {
    let mut r = req(model(1000, 0, 0), 200, "now");
    r.summary = Some(SummaryContext {
        text: "S".into(),
        token_estimate: 42,
        frontier_created_at: clock::now(),
        frontier_message_id: Uuid::new_v4(),
    });
    r.recent = vec![msg("user", "q"), msg("assistant", "a")];
    let p = assemble(&r).unwrap();
    let expected = format!("{SUMMARY_PREAMBLE}\n\nS");
    assert_eq!(texts(&p), vec![expected.clone(), "q".into(), "a".into(), "now".into()]);
    assert_eq!(p.input[0].role, InputRole::User);
    assert_eq!(p.summary_applied, Some(42));
    assert_eq!(p.assembled_tokens, i64::try_from(expected.len()).unwrap() + 2 + 3);
}

#[test]
fn summary_dropped_when_it_does_not_fit() {
    // budget 300: mandatory "now" = 3; summary text > 297 tokens.
    let mut r = req(model(500, 0, 0), 200, "now");
    r.summary = Some(SummaryContext {
        text: "S".repeat(300),
        token_estimate: 7,
        frontier_created_at: clock::now(),
        frontier_message_id: Uuid::new_v4(),
    });
    r.recent = vec![msg("user", "q"), msg("assistant", "a")];
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec!["q", "a", "now"]);
    assert_eq!(p.summary_applied, None);
    assert!(!p.messages_truncated, "dropping the summary is not message truncation");
}

#[test]
fn truncation_drops_oldest_whole_turns() {
    // budget 30, mandatory "now"=3 -> 27 left. Messages of 10 bytes.
    let ten = |c: char| c.to_string().repeat(10);
    let mut r = req(model(230, 0, 0), 200, "now");
    r.recent = vec![
        msg("user", &ten('a')),
        msg("assistant", &ten('b')),
        msg("user", &ten('c')),
        msg("assistant", &ten('d')),
    ];
    // newest first: d (17 left), c (7 left), b does not fit -> a, b dropped.
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec![ten('c'), ten('d'), "now".into()]);
    assert!(p.messages_truncated);
    assert_eq!(p.assembled_tokens, 23);

    // budget 35 -> 32 left: d, c, b fit (2 left), a does not -> kept range would start with b
    // (assistant) -> b dropped too.
    r.model.context_window = 235;
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec![ten('c'), ten('d'), "now".into()]);
    assert!(p.messages_truncated);
    assert_eq!(p.input[0].role, InputRole::User);
    assert_eq!(p.assembled_tokens, 23, "dropped leading assistant is not counted");
}

#[test]
fn leading_assistant_message_never_starts_history() {
    let mut r = req(model(1000, 0, 0), 200, "now");
    r.recent = vec![msg("assistant", "orphan answer"), msg("user", "q"), msg("assistant", "a")];
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec!["q", "a", "now"]);
    assert!(p.messages_truncated);
}

#[test]
fn system_messages_are_not_sent() {
    let mut r = req(model(1000, 0, 0), 200, "now");
    r.recent = vec![msg("system", "sys note"), msg("user", "q"), msg("assistant", "a")];
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec!["q", "a", "now"]);
    assert!(!p.messages_truncated, "skipped system messages do not count as truncation");
}

#[test]
fn nothing_fits_sets_truncated() {
    // budget 10 -> mandatory 3, 7 left; every message is 10 bytes.
    let mut r = req(model(210, 0, 0), 200, "now");
    r.recent = vec![msg("user", &"q".repeat(10)), msg("assistant", &"a".repeat(10))];
    let p = assemble(&r).unwrap();
    assert_eq!(texts(&p), vec!["now"]);
    assert!(p.messages_truncated);
}

#[test]
fn images_are_attached_to_current_message() {
    let mut r = req(model(1000, 0, 0), 200, "look");
    r.image_file_ids = vec!["file-1".into(), "file-2".into()];
    r.recent = vec![msg("user", "q"), msg("assistant", "a")];
    let p = assemble(&r).unwrap();
    let last = p.input.last().unwrap();
    assert_eq!(last.text, "look");
    assert_eq!(last.image_file_ids, vec!["file-1".to_owned(), "file-2".to_owned()]);
    assert!(p.input[..p.input.len() - 1].iter().all(|i| i.image_file_ids.is_empty()));
    assert_eq!(p.assembled_tokens, 4 + 2 * 50 + 2);
}

#[test]
fn assembly_is_deterministic() {
    let mut r = req(model(240, 0, 0), 200, "now");
    r.summary = Some(SummaryContext {
        text: "short".into(),
        token_estimate: 3,
        frontier_created_at: clock::now(),
        frontier_message_id: Uuid::new_v4(),
    });
    r.recent = (0..8).map(|i| msg(if i % 2 == 0 { "user" } else { "assistant" }, &format!("message {i}"))).collect();
    let a = assemble(&r).unwrap();
    let b = assemble(&r).unwrap();
    assert_eq!(a.input, b.input);
    assert_eq!(a.instructions, b.instructions);
    assert_eq!(a.assembled_tokens, b.assembled_tokens);
    assert_eq!(a.messages_truncated, b.messages_truncated);
    assert_eq!(a.summary_applied, b.summary_applied);
}

// ───────────────────────────── load_history ─────────────────────────────

#[tokio::test]
async fn load_history_returns_latest_messages_in_chronological_order() {
    let t = TestApp::new().await;
    let app = &t.app;
    let chat = insert_chat(app, TENANT_A, USER_A1, TINY).await;
    let mut all = Vec::new();
    for i in 0..4 {
        let (_, u, a) = insert_turn(app, TENANT_A, chat, &format!("q{i}"), &format!("a{i}")).await;
        all.push(u.id);
        all.push(a.id);
    }
    let (summary, msgs) = load_history(app, TENANT_A, chat, None, None, 3).await.unwrap();
    assert!(summary.is_none());
    let contents: Vec<&str> = msgs.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["a2", "q3", "a3"]);
    assert_eq!(msgs[0].role, "assistant");

    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 100).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), all);

    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 0).await.unwrap();
    assert!(msgs.is_empty());

    // Tenant isolation: another tenant sees nothing.
    let (_, msgs) = load_history(app, crate::testing::TENANT_B, chat, None, None, 100).await.unwrap();
    assert!(msgs.is_empty());
}

#[tokio::test]
async fn load_history_respects_boundary_and_exclusion() {
    let t = TestApp::new().await;
    let app = &t.app;
    let chat = insert_chat(app, TENANT_A, USER_A1, TINY).await;
    let (_, u1, a1) = insert_turn(app, TENANT_A, chat, "q1", "a1").await;
    let (_, u2, _a2) = insert_turn(app, TENANT_A, chat, "q2", "a2").await;

    let (_, msgs) = load_history(app, TENANT_A, chat, Some((a1.created_at, a1.id)), None, 10).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![u1.id, a1.id], "boundary is inclusive");

    let (_, msgs) = load_history(app, TENANT_A, chat, None, Some(u2.id), 10).await.unwrap();
    let contents: Vec<&str> = msgs.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["q1", "a1", "a2"]);
}

#[tokio::test]
async fn load_history_excludes_compressed_deleted_and_unattributed_messages() {
    let t = TestApp::new().await;
    let app = &t.app;
    let chat = insert_chat(app, TENANT_A, USER_A1, TINY).await;
    let (_, u1, a1) = insert_turn(app, TENANT_A, chat, "q1", "a1").await;
    let (_, u2, a2) = insert_turn(app, TENANT_A, chat, "q2", "a2").await;
    insert_message(app, TENANT_A, chat, None, "system", "no request id").await;
    let (_, u3, a3) = insert_turn(app, TENANT_A, chat, "q3", "a3").await;
    set_message_flags(app, u1.id, false, true).await;
    set_message_flags(app, a1.id, false, true).await;
    set_message_flags(app, a2.id, true, false).await;

    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 10).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![u2.id, u3.id, a3.id]);
}

#[tokio::test]
async fn load_history_applies_summary_frontier() {
    let t = TestApp::new().await;
    let app = &t.app;
    let chat = insert_chat(app, TENANT_A, USER_A1, TINY).await;
    let (_, _u1, a1) = insert_turn(app, TENANT_A, chat, "q1", "a1").await;
    let (_, u2, a2) = insert_turn(app, TENANT_A, chat, "q2", "a2").await;
    insert_summary(app, TENANT_A, chat, "the summary", (a1.created_at, a1.id), 12).await;

    let (summary, msgs) = load_history(app, TENANT_A, chat, None, None, 10).await.unwrap();
    let s = summary.unwrap();
    assert_eq!(s.text, "the summary");
    assert_eq!(s.token_estimate, 12);
    assert_eq!((s.frontier_created_at, s.frontier_message_id), (a1.created_at, a1.id));
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![u2.id, a2.id]);
}

#[tokio::test]
async fn load_history_orders_timestamp_ties_by_id() {
    let t = TestApp::new().await;
    let app = &t.app;
    let chat = insert_chat(app, TENANT_A, USER_A1, TINY).await;
    let ts = clock::now();
    let low = Uuid::from_u128(1);
    let mid = Uuid::from_u128(2);
    let high = Uuid::from_u128(3);
    let rid = || Some(Uuid::new_v4());
    insert_message_at(app, TENANT_A, chat, rid(), "assistant", "high", high, ts).await;
    insert_message_at(app, TENANT_A, chat, rid(), "user", "low", low, ts).await;
    insert_message_at(app, TENANT_A, chat, rid(), "user", "mid", mid, ts).await;

    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 10).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![low, mid, high]);
    // limit keeps the latest by (created_at, id)
    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 2).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![mid, high]);
    // boundary and frontier on the tie
    let (_, msgs) = load_history(app, TENANT_A, chat, Some((ts, mid)), None, 10).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![low, mid]);
    insert_summary(app, TENANT_A, chat, "s", (ts, low), 1).await;
    let (_, msgs) = load_history(app, TENANT_A, chat, None, None, 10).await.unwrap();
    assert_eq!(msgs.iter().map(|m| m.id).collect::<Vec<_>>(), vec![mid, high]);
}
