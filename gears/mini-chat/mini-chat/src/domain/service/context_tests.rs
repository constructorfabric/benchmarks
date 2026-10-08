#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::EstimationBudgets;
use sea_orm::ActiveValue::Set;
use time::{Duration, OffsetDateTime};
use toolkit_db::secure::secure_insert;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::service::finalization::user_message;
use crate::domain::service::stream::test_helpers::create_chat;
use crate::domain::service::test_support::{TENANT_A, TestEnv, USER_A1};

fn budgets() -> EstimationBudgets {
    EstimationBudgets {
        bytes_per_token_conservative: 4,
        fixed_overhead_tokens: 0,
        safety_margin_pct: 0,
        image_token_budget: 100,
        tool_surcharge_tokens: 10,
        web_search_surcharge_tokens: 20,
        code_interpreter_surcharge_tokens: 30,
        minimal_generation_floor: 50,
    }
}

fn msg(role: &str, content: &str, i: i64) -> message::Model {
    message::Model {
        id: Uuid::from_u128(u128::try_from(i).unwrap()),
        tenant_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        request_id: Some(Uuid::nil()),
        role: role.to_owned(),
        content: content.to_owned(),
        content_type: "text".to_owned(),
        token_estimate: 0,
        provider_response_id: None,
        request_kind: "chat".to_owned(),
        features_used: serde_json::json!([]),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: None,
        is_compressed: false,
        created_at: OffsetDateTime::UNIX_EPOCH + Duration::seconds(i),
        deleted_at: None,
    }
}

fn summary(text: &str) -> thread_summary::Model {
    thread_summary::Model {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        summary_text: text.to_owned(),
        summarized_up_to_created_at: OffsetDateTime::UNIX_EPOCH,
        summarized_up_to_message_id: Uuid::nil(),
        token_estimate: 77,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
    }
}

fn input<'a>(
    b: &'a EstimationBudgets,
    window: u32,
    recent: &'a [message::Model],
    s: Option<&'a thread_summary::Model>,
) -> ContextInput<'a> {
    ContextInput {
        budgets: b,
        context_window: window,
        max_input_tokens: 0,
        max_output_tokens_applied: 100,
        tools: ToolGates::default(),
        instructions: "",
        current_text: "1234",
        image_count: 0,
        summary: s,
        recent,
    }
}

#[test]
fn text_estimate_formula() {
    let b = EstimationBudgets::default(); // bpt 4, overhead 100, margin 10
    assert_eq!(estimate_text_tokens(&b, 0), 110);
    assert_eq!(estimate_text_tokens(&b, 400), 220);
    assert_eq!(estimate_text_tokens(&b, 1), 112); // ceil(101 * 1.1)
    let zero = EstimationBudgets {
        bytes_per_token_conservative: 0,
        ..budgets()
    };
    assert_eq!(estimate_text_tokens(&zero, 7), 7);
    assert_eq!(estimate_message_tokens(&budgets(), "12345678", 2), 2 + 200);
}

#[test]
fn input_limit_rules() {
    assert_eq!(input_limit(1000, 0, 100), 900);
    assert_eq!(input_limit(1000, 500, 100), 500);
    assert_eq!(input_limit(1000, 950, 100), 900);
}

#[test]
fn invalid_budget_is_rejected() {
    let b = budgets();
    let mut i = input(&b, 100, &[], None);
    let e = assemble(&i).err().unwrap();
    assert!(
        matches!(e, DomainError::OutOfRange { ref reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED")
    );
    // Surcharges eat the whole input limit.
    i.context_window = 160;
    i.tools = ToolGates {
        web_search: true,
        file_search: true,
        code_interpreter: true,
    };
    assert!(assemble(&i).is_err());
    i.context_window = 162;
    let p = assemble(&i).unwrap();
    assert_eq!(p.token_budget, 2);
    assert_eq!(p.assembled_tokens, 1);
}

#[test]
fn mandatory_items_over_budget() {
    let b = budgets();
    let mut i = input(&b, 110, &[], None); // budget 10
    i.current_text = "12345678901234567890123456789012345678901234"; // 11 tokens
    assert!(assemble(&i).is_err());
    i.current_text = "1";
    i.image_count = 1; // +100
    assert!(assemble(&i).is_err());
}

#[test]
fn summary_kept_or_dropped() {
    let b = budgets();
    let s = summary("short");
    let p = assemble(&input(&b, 10_000, &[], Some(&s))).unwrap();
    assert_eq!(p.summary_applied, Some(77));
    assert_eq!(p.messages.len(), 1);
    assert_eq!(p.messages[0].role, InputRole::User);
    assert_eq!(
        p.messages[0].content,
        vec![crate::infra::llm::ContentPart::Text(format!(
            "{SUMMARY_PREAMBLE}\n\nshort"
        ))]
    );
    // Budget too small for the summary (preamble included): dropped.
    let p = assemble(&input(&b, 130, &[], Some(&s))).unwrap();
    assert_eq!(p.summary_applied, None);
    assert!(p.messages.is_empty());
}

#[test]
fn truncation_keeps_newest_whole_turns() {
    let b = budgets();
    // Each message is 10 tokens (40 bytes).
    let c = "x".repeat(40);
    let recent = vec![
        msg("user", &c, 1),
        msg("assistant", &c, 2),
        msg("user", &c, 3),
        msg("assistant", &c, 4),
        msg("user", &c, 5),
        msg("assistant", &c, 6),
    ];
    // budget = 135 - 100 = 35 tokens; mandatory = 1; 34 left → 3 newest messages fit
    // (msgs 4..6) but the range would start with an assistant message → dropped.
    let p = assemble(&input(&b, 135, &recent, None)).unwrap();
    assert!(p.messages_truncated);
    assert_eq!(p.messages.len(), 2);
    assert_eq!(p.messages[0].role, InputRole::User);
    assert_eq!(p.assembled_tokens, 1 + 20);

    // Everything fits.
    let p = assemble(&input(&b, 1000, &recent, None)).unwrap();
    assert!(!p.messages_truncated);
    assert_eq!(p.messages.len(), 6);
    assert_eq!(p.assembled_tokens, 61);

    // Deterministic.
    let a1 = assemble(&input(&b, 135, &recent, None)).unwrap();
    let a2 = assemble(&input(&b, 135, &recent, None)).unwrap();
    assert_eq!(a1.messages, a2.messages);
    assert_eq!(a1.assembled_tokens, a2.assembled_tokens);
}

#[test]
fn leading_answer_drop_sets_truncated_only_after_budget_truncation() {
    let b = budgets();
    let c = "x".repeat(40);
    // The recent-messages window starts with an answer; everything fits the budget: the
    // leading answer is dropped but nothing was truncated.
    let window_start = vec![
        msg("assistant", &c, 1),
        msg("user", &c, 2),
        msg("assistant", &c, 3),
    ];
    let p = assemble(&input(&b, 1000, &window_start, None)).unwrap();
    assert_eq!(p.messages.len(), 2);
    assert_eq!(p.messages[0].role, InputRole::User);
    assert!(!p.messages_truncated);

    // The budget drops the oldest question; the then-leading answer is dropped as part of
    // the truncation: flagged.
    let recent = vec![
        msg("user", &c, 1),
        msg("assistant", &c, 2),
        msg("user", &c, 3),
        msg("assistant", &c, 4),
    ];
    // 35-token budget, 1 mandatory: msgs 2..4 fit (30), msg 1 is dropped, msg 2 leads.
    let p = assemble(&input(&b, 135, &recent, None)).unwrap();
    assert_eq!(p.messages.len(), 2);
    assert_eq!(p.messages[0].role, InputRole::User);
    assert!(p.messages_truncated);
}

#[test]
fn summary_has_priority_over_history() {
    let b = budgets();
    let c = "x".repeat(40);
    let recent = vec![msg("user", &c, 1), msg("assistant", &c, 2)];
    let s = summary("s");
    let need = estimate_text_tokens(&b, summary_message_text("s").len());
    // Room for the summary only.
    let window = u32::try_from(100 + 1 + need + 5).unwrap();
    let p = assemble(&input(&b, window, &recent, Some(&s))).unwrap();
    assert_eq!(p.summary_applied, Some(77));
    assert_eq!(p.messages.len(), 1);
    assert!(p.messages_truncated);
}

async fn insert_msg(
    env: &TestEnv,
    chat: Uuid,
    rid: Uuid,
    content: &str,
    at: OffsetDateTime,
) -> Uuid {
    let id = Uuid::new_v4();
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<message::Entity>(
        user_message(TENANT_A, chat, id, rid, content.to_owned(), at),
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn load_history_boundary_frontier_and_limit() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, "gpt-premium").await;
    let t0 = OffsetDateTime::now_utc() - Duration::minutes(10);
    let mut ids = Vec::new();
    for i in 0..6 {
        ids.push(
            insert_msg(
                &env,
                chat,
                Uuid::new_v4(),
                &format!("m{i}"),
                t0 + Duration::seconds(i),
            )
            .await,
        );
    }
    let current = Uuid::new_v4();
    insert_msg(&env, chat, current, "current", t0 + Duration::seconds(100)).await;
    let scope = AccessScope::for_tenant(TENANT_A);
    let conn = env.deps.db.conn().unwrap();

    let h = load_history(&conn, &scope, chat, current, 4).await.unwrap();
    let contents: Vec<&str> = h.recent.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["m2", "m3", "m4", "m5"]);
    assert!(h.summary.is_none());

    let h = load_history(&conn, &scope, chat, current, 0).await.unwrap();
    assert!(h.recent.is_empty());

    // Summary frontier at m3: only later messages are loaded.
    let now = OffsetDateTime::now_utc();
    secure_insert::<thread_summary::Entity>(
        thread_summary::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(TENANT_A),
            chat_id: Set(chat),
            summary_text: Set("sum".into()),
            summarized_up_to_created_at: Set(t0 + Duration::seconds(3)),
            summarized_up_to_message_id: Set(ids[3]),
            token_estimate: Set(5),
            created_at: Set(now),
            updated_at: Set(now),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    let h = load_history(&conn, &scope, chat, current, 10)
        .await
        .unwrap();
    let contents: Vec<&str> = h.recent.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, vec!["m4", "m5"]);
    assert_eq!(h.summary.unwrap().summary_text, "sum");
    env.shutdown().await;
}
