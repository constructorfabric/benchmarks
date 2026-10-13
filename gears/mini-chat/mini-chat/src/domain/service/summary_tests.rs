#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::UsageTokens;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::SecureUpdateExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::test_helpers::*;
use crate::domain::service::test_support::{TENANT_A, TestEnv, USER_A1, ctx_a1, model};
use crate::infra::db::entity::message;

fn msg(role: &str, content: &str) -> message::Model {
    message::Model {
        id: Uuid::new_v4(),
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
        created_at: OffsetDateTime::UNIX_EPOCH,
        deleted_at: None,
    }
}

#[test]
fn parse_summary_rules() {
    assert_eq!(
        parse_summary("<analysis>a</analysis><summary>S</summary>"),
        "S"
    );
    assert_eq!(
        parse_summary("<analysis>\nthink\n</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"),
        "A\n\nB"
    );
    assert_eq!(parse_summary("plain text"), "plain text");
    assert_eq!(parse_summary("<analysis>only</analysis> rest"), "rest");
    assert_eq!(parse_summary("<analysis>unterminated"), "");
    assert_eq!(parse_summary("<summary>unterminated"), "");
    assert_eq!(parse_summary("<analysis>x</analysis>"), "");
}

#[test]
fn token_estimate_rules() {
    let u = UsageTokens {
        output_tokens: 40,
        reasoning_tokens: 10,
        ..UsageTokens::default()
    };
    assert_eq!(token_estimate(Some(&u), "abc"), 30);
    let u = UsageTokens {
        output_tokens: 10,
        reasoning_tokens: 10,
        ..UsageTokens::default()
    };
    assert_eq!(token_estimate(Some(&u), "abcde"), 2);
    assert_eq!(token_estimate(None, "abcdefgh"), 2);
}

#[test]
fn prompt_format() {
    let msgs = vec![
        msg("user", "hello"),
        msg("assistant", "hi there"),
        msg("system", "x"),
    ];
    let p = build_user_prompt(None, &msgs, 0);
    assert!(p.starts_with(
        "Summarize the following conversation:\n\nUser: hello\n\nAssistant: hi there\n\n"
    ));
    assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
    assert!(!p.contains("System"));

    let p = build_user_prompt(Some("old summary"), &msgs, 3);
    assert!(p.starts_with(OPENING_MERGE));
    assert!(p.contains("<existing_summary>\nold summary\n</existing_summary>\n\nNew messages to incorporate:\n\nUser: hel...\n\nAssistant: hi ..."));

    assert_eq!(cut_content("abcdef", 0), "abcdef");
    assert_eq!(cut_content("abcdef", 6), "abcdef");
    assert_eq!(cut_content("ábcdef", 2), "áb...");
}

#[test]
fn fit_drops_oldest_keeping_two() {
    let msgs: Vec<message::Model> = (0..10).map(|i| msg("user", &"x".repeat(400 + i))).collect();
    assert_eq!(fit_start("", None, &msgs, 0, 4, None), 0);
    assert_eq!(fit_start("", None, &msgs, 0, 4, Some(1_000_000)), 0);
    // Tiny budget: drop until two remain.
    assert_eq!(fit_start("", None, &msgs, 0, 4, Some(1)), 8);
    // Medium budget: the oldest messages are dropped until the prompt fits.
    let start = fit_start("", None, &msgs, 0, 4, Some(650));
    assert!(start > 0 && start < 8, "{start}");
    let p = build_user_prompt(None, &msgs[start..], 0);
    assert!(p.len().div_ceil(4) <= 650);
}

#[test]
fn proactive_threshold() {
    let t = SummaryTrigger {
        assembled_tokens: 80,
        effective_budget: 100,
        threshold_pct: 80,
        messages_truncated: false,
    };
    assert!(t.over_threshold());
    let t = SummaryTrigger {
        assembled_tokens: 79,
        ..t
    };
    assert!(!t.over_threshold());
}

#[test]
fn system_prompt_choice() {
    let mut m = model("s", "standard");
    assert_eq!(system_prompt(&m, "cfg prompt"), "cfg prompt");
    assert_eq!(
        system_prompt(&m, " "),
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT
    );
    m.thread_summary_prompt = "model prompt".into();
    assert_eq!(system_prompt(&m, "cfg prompt"), "model prompt");
    m.context_window = 1000;
    m.max_output_tokens = 200;
    m.max_input_tokens = 500;
    assert_eq!(summary_input_budget(&m), Some(500));
    m.max_input_tokens = 0;
    assert_eq!(summary_input_budget(&m), Some(800));
    m.context_window = 0;
    assert_eq!(summary_input_budget(&m), None);
}

/// Chat model with a 2000-token input budget so that 1% (20 tokens) is always reached.
const CHAT_MODEL: &str = "small-window";

async fn summary_env() -> TestEnv {
    env_with(|o| {
        o.cfg.thread_summary_worker.compression_threshold_pct = 1;
        o.cfg.thread_summary_worker.summary_model_id = "gpt-standard".into();
        let mut m = model(CHAT_MODEL, "premium");
        m.context_window = 4096 + 2000;
        m.max_input_tokens = 0;
        o.catalog.push(m);
    })
    .await
}

async fn send_ok(
    env: &TestEnv,
    chat: Uuid,
    text: &str,
) -> Vec<crate::api::rest::dto::MiniChatSseEvent> {
    let evs = collect(
        env.services
            .stream
            .send(&ctx_a1(), chat, input(text))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    evs
}

async fn tasks(env: &TestEnv, n: usize) -> Vec<ThreadSummaryTask> {
    env.delivered_to(&env.deps.cfg.outbox.thread_summary_queue_name, n)
        .await
        .into_iter()
        .map(|v| serde_json::from_value(v).unwrap())
        .collect()
}

#[tokio::test]
async fn trigger_handler_and_next_turn() {
    let env = summary_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, CHAT_MODEL).await;
    // First turn: no earlier message → nothing enqueued.
    send_ok(&env, chat, "first").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(tasks(&env, 0).await.is_empty());

    send_ok(&env, chat, "second").await;
    let t = tasks(&env, 1).await;
    assert_eq!(t.len(), 1);
    let task = t[0].clone();
    let msgs = messages(&env, chat).await;
    assert_eq!(task.frozen_target_message_id, msgs[1].id);
    assert!(task.base_frontier_message_id.is_none());
    assert_eq!(task.system_task_type, "thread_summary_update");

    let handler = ThreadSummaryHandler::new(env.deps.clone());
    assert_eq!(handler.run(&task).await, SummaryResult::Success);
    let row = summary_row(&env, chat).await.unwrap();
    assert_eq!(row.summary_text, "Summary of the conversation.");
    assert_eq!(row.summarized_up_to_message_id, msgs[1].id);
    assert_eq!(row.token_estimate, 40);
    let msgs = messages(&env, chat).await;
    assert!(msgs[0].is_compressed && msgs[1].is_compressed);
    assert!(!msgs[2].is_compressed && !msgs[3].is_compressed);

    let req = env.llm.requests.lock().last().cloned().unwrap();
    assert_eq!(req.request_type, crate::infra::llm::RequestType::Summary);
    assert!(!req.stream);
    assert_eq!(req.model, "gpt-standard");
    assert_eq!(req.metadata["request_type"], "summary");
    assert_eq!(req.metadata["feature"], "none");
    assert!(req.user.ends_with("111111116a8847689dfc6bcd5187d9ed"));
    assert_eq!(
        req.instructions,
        crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT
    );
    let crate::infra::llm::ContentPart::Text(prompt) = &req.input[0].content[0] else {
        panic!()
    };
    assert!(prompt.contains("User: first\n\nAssistant: Hello world"));
    assert!(!prompt.contains("second"));

    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 3).await;
    let sys = usage
        .iter()
        .find(|u| u["requester_type"] == "system")
        .expect("system usage");
    assert_eq!(sys["billing_outcome"], "system_task");
    assert_eq!(sys["settlement_method"], "none");
    assert_eq!(sys["actual_credits_micro"], 0);
    assert_eq!(sys["system_task_type"], "thread_summary_update");
    assert!(sys.get("user_id").is_none());
    assert_eq!(
        sys["dedupe_key"],
        format!(
            "{}/thread_summary_update/{}",
            TENANT_A.simple(),
            task.system_request_id.simple()
        )
    );

    // Same frozen range again: CAS conflict, no second commit.
    assert_eq!(handler.run(&task).await, SummaryResult::Skipped("conflict"));

    // Next turn uses the summary instead of the compressed messages.
    let evs = send_ok(&env, chat, "third").await;
    assert_eq!(
        data(&evs, "stream_started")["thread_summary_applied"]["token_estimate"],
        40
    );
    let req = env.llm.requests.lock().last().cloned().unwrap();
    let crate::infra::llm::ContentPart::Text(first) = &req.input[0].content[0] else {
        panic!()
    };
    assert!(first.starts_with(crate::domain::service::context::SUMMARY_PREAMBLE));
    assert!(first.ends_with("Summary of the conversation."));
    assert_eq!(req.input.len(), 4); // summary, second, Hello world, third

    // An existing summary without truncation does not trigger again.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(tasks(&env, 0).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn handler_skips_and_failures() {
    let env = summary_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, CHAT_MODEL).await;
    send_ok(&env, chat, "first").await;
    send_ok(&env, chat, "second").await;
    let task = tasks(&env, 1).await[0].clone();
    let handler = ThreadSummaryHandler::new(env.deps.clone());

    // Base frontier that no longer exists.
    let missing = ThreadSummaryTask {
        base_frontier_created_at: Some(OffsetDateTime::now_utc()),
        base_frontier_message_id: Some(Uuid::new_v4()),
        ..task.clone()
    };
    assert_eq!(
        handler.run(&missing).await,
        SummaryResult::Skipped("base_missing")
    );

    // Empty summary → Retry; the last attempt is rejected.
    *env.llm.summary_text.lock() = "<analysis>only analysis</analysis>".into();
    assert!(matches!(handler.run(&task).await, SummaryResult::Retry(_)));
    let msg = |attempts: i16| OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload: serde_json::to_vec(&task).unwrap(),
        payload_type: "x".into(),
        created_at: chrono::Utc::now(),
        attempts,
    };
    assert!(matches!(
        handler.handle(&msg(0)).await,
        MessageResult::Retry
    ));
    assert!(matches!(
        handler.handle(&msg(2)).await,
        MessageResult::Reject(_)
    ));
    let bad = OutboxMessage {
        payload: b"not json".to_vec(),
        ..msg(0)
    };
    assert!(matches!(
        handler.handle(&bad).await,
        MessageResult::Reject(_)
    ));

    // Target message deleted before the commit.
    *env.llm.summary_text.lock() = String::new();
    let conn = env.deps.db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(
            message::Column::DeletedAt,
            Expr::value(Some(OffsetDateTime::now_utc())),
        )
        .filter(message::Column::Id.eq(task.frozen_target_message_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(
        handler.run(&task).await,
        SummaryResult::Skipped("frontier_deleted")
    );
    assert!(summary_row(&env, chat).await.is_none());
    assert!(messages(&env, chat).await.iter().all(|m| !m.is_compressed));
    env.shutdown().await;
}

#[tokio::test]
async fn missing_summary_model_rejects() {
    // Default summary model (gpt-4.1-mini) is not in the test catalog.
    let env = TestEnv::default_env().await;
    let handler = ThreadSummaryHandler::new(env.deps.clone());
    let task = ThreadSummaryTask {
        tenant_id: TENANT_A,
        chat_id: Uuid::new_v4(),
        system_request_id: Uuid::new_v4(),
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        frozen_target_created_at: OffsetDateTime::now_utc(),
        frozen_target_message_id: Uuid::new_v4(),
        system_task_type: SYSTEM_TASK_TYPE.into(),
    };
    assert_eq!(
        handler.run(&task).await,
        SummaryResult::Reject("model_unavailable".into())
    );
    env.shutdown().await;
}

#[tokio::test]
async fn disabled_worker_never_triggers() {
    let env = env_with(|o| {
        o.cfg.thread_summary_worker.compression_threshold_pct = 1;
        o.cfg.thread_summary_worker.enabled = false;
        let mut m = model(CHAT_MODEL, "premium");
        m.context_window = 4096 + 2000;
        m.max_input_tokens = 0;
        o.catalog.push(m);
    })
    .await;
    let chat = create_chat(&env, USER_A1, TENANT_A, CHAT_MODEL).await;
    send_ok(&env, chat, "first").await;
    send_ok(&env, chat, "second").await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(tasks(&env, 0).await.is_empty());
    env.shutdown().await;
}

#[tokio::test]
async fn summary_without_provider_usage_emits_null_usage() {
    let env = summary_env().await;
    *env.llm.summary_no_usage.lock() = true;
    let chat = create_chat(&env, USER_A1, TENANT_A, CHAT_MODEL).await;
    send_ok(&env, chat, "first").await;
    send_ok(&env, chat, "second").await;
    let task = tasks(&env, 1).await[0].clone();
    let handler = ThreadSummaryHandler::new(env.deps.clone());
    assert_eq!(handler.run(&task).await, SummaryResult::Success);
    let usage = env.delivered_to(&env.deps.cfg.outbox.queue_name, 3).await;
    let sys = usage
        .iter()
        .find(|u| u["requester_type"] == "system")
        .expect("system usage");
    assert!(sys["usage"].is_null(), "{sys}");
    env.shutdown().await;
}

#[tokio::test]
async fn startup_summary_model_check() {
    use crate::domain::service::summary::{SummaryModelCheck, check_summary_model};
    // Present and enabled.
    let env = summary_env().await;
    assert_eq!(check_summary_model(&env.deps).await, SummaryModelCheck::Ok);
    env.shutdown().await;
    // Default summary model (gpt-4.1-mini) is missing from the test catalog.
    let env = TestEnv::default_env().await;
    assert_eq!(
        check_summary_model(&env.deps).await,
        SummaryModelCheck::Unavailable
    );
    env.shutdown().await;
    // Disabled model.
    let env = env_with(|o| o.cfg.thread_summary_worker.summary_model_id = "gpt-disabled".into()).await;
    assert_eq!(
        check_summary_model(&env.deps).await,
        SummaryModelCheck::Unavailable
    );
    env.shutdown().await;
    // Thread summaries disabled: nothing checked.
    let env = env_with(|o| o.cfg.thread_summary_worker.enabled = false).await;
    assert_eq!(
        check_summary_model(&env.deps).await,
        SummaryModelCheck::Disabled
    );
    env.shutdown().await;
}
