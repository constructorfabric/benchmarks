#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the thread-summary handler that replaces the foundation placeholder
//! (`cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`,
//! `-build-summary-prompt`, `-call-summary-model`, `-commit-summary`,
//! `cpt-cf-mini-chat-dod-background-workers-summary-handler`,
//! `-summary-commit`, `cpt-cf-mini-chat-flow-background-workers-long-conversation-compression`):
//! the frozen range, the prompt, the non-streaming call as a system task with
//! its usage event, failure keeping the previous summary, retries and the dead
//! letter, the frontier CAS with the frozen-target lock and no resurrection
//! after a mutation.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    BillingOutcome, RequesterType, SettlementMethod, SystemTaskType, TerminalState,
};
use serde_json::{Value, json};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use toolkit_security::SecurityContext;
use tracing::Level;
use uuid::Uuid;

use crate::chat_test_env::{ts, ts as stamp};
use crate::config::ThreadSummaryWorkerConfig;
use crate::domain::service::quota::test_support::PolicyStep;
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::HandlerOutcome;
use crate::domain::service::workers::clock::WorkerClock;
use crate::domain::service::workers::metrics::SummaryResult;
use crate::domain::service::workers::ports::{
    SummaryAnswer, SummaryCallError, SummaryModelClient, SummaryRequest,
};
use crate::domain::service::workers::summary_trigger::SummaryTask;
use crate::domain::service::workers::test_support::{ManualClock, Metric};
use crate::mutation_test_env::{SeedTurn, Seeded};
use crate::stream_test_env::{body, oa, rid};
use crate::test_doubles::{Gate, LogCapture, ProxyReply, S2S_SUBJECT, USER_TENANT, outbox_message};
use crate::worker_test_env::{WorkerEnv, WorkerOptions, boundary};

const RESPONSES: &str = "/api.openai.test/v1/responses";

fn is_ok(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Ok)
}

fn is_retry(result: &MessageResult) -> bool {
    matches!(result, MessageResult::Retry)
}

fn rejected(result: &MessageResult) -> Option<&str> {
    match result {
        MessageResult::Reject(reason) => Some(reason),
        _ => None,
    }
}

fn answer(text: &str, output: u64, reasoning: u64) -> Value {
    json!({
        "output": [ { "type": "message", "content": [ { "type": "output_text", "text": text } ] } ],
        "usage": {
            "input_tokens": 200, "output_tokens": output,
            "input_tokens_details": { "cached_tokens": 20 },
            "output_tokens_details": { "reasoning_tokens": reasoning }
        }
    })
}

fn context_error() -> ProxyReply {
    ProxyReply::json(
        400,
        &json!({ "error": { "code": "context_length_exceeded", "message": "too long" } }),
    )
}

/// Queues one good summary answer.
fn script_summary(env: &WorkerEnv, text: &str) {
    env.oagw.push_proxy(ProxyReply::json(
        200,
        &answer(
            &format!("<analysis>a</analysis><summary>{text}</summary>"),
            40,
            10,
        ),
    ));
}

/// The non-streaming summary calls the provider received.
fn summary_calls(env: &WorkerEnv) -> Vec<crate::test_doubles::RecordedProxy> {
    env.oagw
        .proxied()
        .into_iter()
        .filter(|call| {
            call.method == "POST"
                && call.uri.starts_with(RESPONSES)
                && call.json()["stream"] == false
        })
        .collect()
}

fn prompt_of(call: &crate::test_doubles::RecordedProxy) -> String {
    call.json()["input"][0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn entries(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter(|line| line.starts_with("User: ") || line.starts_with("Assistant: "))
        .map(str::to_owned)
        .collect()
}

/// The three-turn conversation of the default chat and the task that
/// summarizes all of it (no base, the frozen target is the last answer).
async fn three_turns(env: &WorkerEnv) -> (Vec<Seeded>, SummaryTask) {
    let turns = env.conversation(3).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[2].assistant_message, 31));
    (turns, task)
}

async fn deliver(env: &WorkerEnv, task: &SummaryTask, attempts: i16) -> MessageResult {
    env.deliver_summary(task, attempts).await
}

// ================================================================================================
// generation
// ================================================================================================

#[tokio::test]
async fn a_work_item_is_summarized_and_committed_with_its_frontier_flags_and_system_usage_event() {
    let env = WorkerEnv::new().await;
    let (turns, task) = three_turns(&env).await;
    script_summary(&env, "The user asked three things.");
    let turns_before = env.quota.dump("chat_turns").await;
    let quota_before = env.quota.quota_dump().await;

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    // one non-streaming call with the S2S context
    let calls = summary_calls(&env);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].subject, S2S_SUBJECT);
    // the stored summary
    let summary = env.summary(env.chat).await.expect("a summary");
    assert_eq!(
        summary.text, "The user asked three things.",
        "the <summary> content only"
    );
    assert_eq!(summary.frontier_id, turns[2].assistant_message);
    assert_eq!(summary.frontier_at, ts(31));
    assert_eq!(summary.token_estimate, 30, "output 40 minus reasoning 10");
    assert_eq!(
        summary.created_at,
        crate::attachment_test_env::AttEnv::stamp(env.now())
    );
    assert_eq!(summary.updated_at, summary.created_at);
    // exactly the messages of the range are flagged
    let all: Vec<Uuid> = turns
        .iter()
        .flat_map(|t| [t.user_message, t.assistant_message])
        .collect();
    assert_eq!(env.compressed_ids(env.chat).await, all);
    // one system-task usage event
    let events = env.usage().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.requester_type, RequesterType::System);
    assert_eq!(
        event.system_task_type,
        Some(SystemTaskType::ThreadSummaryUpdate)
    );
    assert_eq!(event.billing_outcome, BillingOutcome::SystemTask);
    assert_eq!(event.settlement_method, SettlementMethod::None);
    assert_eq!(event.terminal_state, TerminalState::Completed);
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.user_id, None);
    assert_eq!(event.turn_id, None);
    assert_eq!(event.effective_model, "sum");
    assert_eq!(event.selected_model, "sum");
    assert_eq!(event.request_id, task.system_request_id);
    assert_eq!(
        event.dedupe_key,
        format!(
            "{}/thread_summary_update/{}",
            USER_TENANT.simple(),
            task.system_request_id.simple()
        )
    );
    let usage = event.usage.expect("the provider usage");
    assert_eq!(
        (
            usage.input_tokens,
            usage.output_tokens,
            usage.reasoning_tokens
        ),
        (200, 40, 10)
    );
    // a system task: no chat_turns row, no quota_usage change, no audit event
    assert_eq!(env.quota.dump("chat_turns").await, turns_before);
    assert_eq!(env.quota.quota_dump().await, quota_before);
    assert!(env.audits().await.is_empty());
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::Success)]
    );
}

#[tokio::test]
async fn the_request_carries_the_b55_prompt_with_user_and_assistant_entries_and_no_tools() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");

    deliver(&env, &task, 0).await;

    let call = &summary_calls(&env)[0];
    let json = call.json();
    // the catalog thread_summary_prompt wins
    assert_eq!(json["instructions"], "SECRET-SUMMARY-PROMPT");
    assert_eq!(json["model"], "provider-sum");
    assert_eq!(
        json["max_output_tokens"], 500,
        "the summary model's max_output_tokens"
    );
    assert!(json.get("tools").is_none());
    assert_eq!(json["metadata"]["request_type"], "summary");
    assert_eq!(json["metadata"]["feature"], "none");
    assert_eq!(
        json["metadata"]["user_id"],
        "11111111-6a88-4768-9dfc-6bcd5187d9ed"
    );
    assert_eq!(json["metadata"]["tenant_id"], USER_TENANT.to_string());
    assert_eq!(json["metadata"]["chat_id"], env.chat.to_string());
    let prompt = prompt_of(call);
    assert!(
        prompt.starts_with("Summarize the following conversation:"),
        "{prompt}"
    );
    assert_eq!(
        entries(&prompt),
        vec![
            "User: question 1",
            "Assistant: answer 1",
            "User: question 2",
            "Assistant: answer 2",
            "User: question 3",
            "Assistant: answer 3",
        ]
    );
    assert!(
        prompt.contains("<analysis>") && prompt.contains("<summary>"),
        "the analysis instruction"
    );
    for (name, _) in &call.headers {
        let lower = name.to_ascii_lowercase();
        assert!(
            lower != "authorization" && lower != "x-api-key",
            "OAGW injects the credential: {name}"
        );
    }
}

#[tokio::test]
async fn the_system_prompt_falls_back_to_the_configured_one_then_the_built_in_default() {
    // `sum-small` has an empty catalog summary prompt
    let mut summary = ThreadSummaryWorkerConfig::default();
    "sum-small".clone_into(&mut summary.summary_model_id);
    "CONFIGURED PROMPT".clone_into(&mut summary.summary_system_prompt);
    let env = WorkerEnv::with(WorkerOptions {
        summary: summary.clone(),
        ..WorkerOptions::default()
    })
    .await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");
    deliver(&env, &task, 0).await;
    assert_eq!(
        summary_calls(&env)[0].json()["instructions"],
        "CONFIGURED PROMPT"
    );

    summary.summary_system_prompt = String::new();
    let env = WorkerEnv::with(WorkerOptions {
        summary,
        ..WorkerOptions::default()
    })
    .await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");
    deliver(&env, &task, 0).await;
    assert_eq!(
        summary_calls(&env)[0].json()["instructions"],
        crate::domain::service::workers::summary_prompt::DEFAULT_SYSTEM_PROMPT
    );
}

#[tokio::test]
async fn message_content_is_cut_to_the_configured_limit_with_an_ellipsis() {
    let mut summary = ThreadSummaryWorkerConfig::default();
    "sum".clone_into(&mut summary.summary_model_id);
    summary.message_content_limit = 20;
    let env = WorkerEnv::with(WorkerOptions {
        summary,
        ..WorkerOptions::default()
    })
    .await;
    env.m
        .seed_turn(&SeedTurn::completed(1, 10).question(&"q".repeat(100)))
        .await;
    let task = WorkerEnv::task(
        env.chat,
        None,
        boundary(SeedTurn::completed(1, 10).assistant_message_id(), 11),
    );
    script_summary(&env, "S");

    deliver(&env, &task, 0).await;

    let prompt = prompt_of(&summary_calls(&env)[0]);
    assert!(
        prompt.contains(&format!("User: {}...", "q".repeat(20))),
        "{prompt}"
    );
    assert!(!prompt.contains(&"q".repeat(21)));
}

#[tokio::test]
async fn the_prompt_is_fitted_to_the_model_budget_but_the_commit_still_covers_the_whole_range() {
    let mut summary = ThreadSummaryWorkerConfig::default();
    "sum-small".clone_into(&mut summary.summary_model_id);
    summary.message_content_limit = 0;
    let env = WorkerEnv::with(WorkerOptions {
        summary,
        ..WorkerOptions::default()
    })
    .await;
    // 10 turns of 300-byte questions: the range is far above the 900-token budget
    let mut seeded = Vec::new();
    for n in 1..=10_u128 {
        let secs = i64::try_from(n).unwrap() * 10;
        seeded.push(
            env.m
                .seed_turn(&SeedTurn::completed(n, secs).question(&"w".repeat(300)))
                .await,
        );
    }
    let task = WorkerEnv::task(env.chat, None, boundary(seeded[9].assistant_message, 101));
    script_summary(&env, "S");

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    let sent = entries(&prompt_of(&summary_calls(&env)[0]));
    assert!(
        sent.len() < 20,
        "older messages were dropped from the prompt: {}",
        sent.len()
    );
    assert!(
        sent.last().unwrap().starts_with("Assistant: answer 10"),
        "the newest are kept"
    );
    assert_eq!(
        env.compressed_ids(env.chat).await.len(),
        20,
        "the commit covers exactly the frozen range, dropped messages included"
    );
}

#[tokio::test]
async fn the_frozen_range_excludes_messages_created_after_the_target() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(4).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[1].assistant_message, 21));
    script_summary(&env, "S");

    deliver(&env, &task, 0).await;

    let sent = entries(&prompt_of(&summary_calls(&env)[0]));
    assert_eq!(sent.len(), 4, "only turns 1 and 2: {sent:?}");
    assert!(
        !sent
            .iter()
            .any(|e| e.contains("question 3") || e.contains("question 4"))
    );
    assert_eq!(
        env.compressed_ids(env.chat).await,
        vec![
            turns[0].user_message,
            turns[0].assistant_message,
            turns[1].user_message,
            turns[1].assistant_message
        ],
        "later messages are never read and never flagged"
    );
    assert_eq!(
        env.summary(env.chat).await.unwrap().frontier_id,
        turns[1].assistant_message
    );
}

#[tokio::test]
async fn only_live_non_compressed_messages_after_the_base_are_summarized_and_flagged() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(4).await;
    env.seed_summary(
        env.chat,
        "earlier summary",
        turns[0].assistant_message,
        11,
        9,
    )
    .await;
    // turn 1 is compressed, a message of turn 3 is soft-deleted (a stray row)
    env.exec(&format!(
        "UPDATE messages SET is_compressed = 1 WHERE id IN ({}, {})",
        crate::chat_test_env::blob(turns[0].user_message),
        crate::chat_test_env::blob(turns[0].assistant_message)
    ))
    .await;
    env.m
        .quota
        .db
        .soft_delete("messages", turns[2].user_message)
        .await;
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[0].assistant_message, 11)),
        boundary(turns[3].assistant_message, 41),
    );
    script_summary(&env, "merged");

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    let prompt = prompt_of(&summary_calls(&env)[0]);
    assert!(
        prompt.contains("<existing_summary>\nearlier summary\n</existing_summary>"),
        "{prompt}"
    );
    assert_eq!(
        entries(&prompt),
        vec![
            "User: question 2",
            "Assistant: answer 2",
            "Assistant: answer 3",
            "User: question 4",
            "Assistant: answer 4"
        ]
    );
    let flagged = env.compressed_ids(env.chat).await;
    assert!(
        flagged.contains(&turns[1].user_message) && flagged.contains(&turns[3].assistant_message)
    );
    assert!(
        !flagged.contains(&turns[2].user_message),
        "a soft-deleted message is never flagged"
    );
}

#[tokio::test]
async fn a_second_summary_replaces_the_first_through_the_base_cas() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(4).await;
    env.seed_summary(env.chat, "first", turns[1].assistant_message, 21, 9)
        .await;
    let created = env.summary(env.chat).await.unwrap().created_at;
    env.advance(60);
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[1].assistant_message, 21)),
        boundary(turns[3].assistant_message, 41),
    );
    script_summary(&env, "second");

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    let summary = env.summary(env.chat).await.unwrap();
    assert_eq!(summary.text, "second");
    assert_eq!(summary.frontier_id, turns[3].assistant_message);
    assert_eq!(
        summary.created_at, created,
        "the row is updated, not replaced"
    );
    assert_eq!(
        summary.updated_at,
        crate::attachment_test_env::AttEnv::stamp(env.now())
    );
    assert_eq!(summary.token_estimate, 30);
    assert_eq!(env.count("thread_summaries").await, 1);
    assert_eq!(
        env.compressed_ids(env.chat).await,
        vec![
            turns[2].user_message,
            turns[2].assistant_message,
            turns[3].user_message,
            turns[3].assistant_message
        ],
        "exactly the range (base, target]"
    );
}

#[tokio::test]
async fn the_token_estimate_falls_back_to_a_quarter_of_the_bytes_when_there_is_no_visible_output_count()
 {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    // output 10, reasoning 10: no positive difference
    env.oagw.push_proxy(ProxyReply::json(
        200,
        &answer("<summary>abcdefghi</summary>", 10, 10),
    ));

    deliver(&env, &task, 0).await;

    assert_eq!(
        env.summary(env.chat).await.unwrap().token_estimate,
        3,
        "9 bytes / 4 rounded up"
    );
}

// ================================================================================================
// the next turn uses the summary
// ================================================================================================

#[tokio::test]
async fn the_next_provider_request_starts_with_the_summary_and_contains_no_compressed_message() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "Summary of the first three turns.");
    deliver(&env, &task, 0).await;
    let estimate = env.summary(env.chat).await.unwrap().token_estimate;

    env.script(&[oa::delta("fine"), oa::completed(10, 5)]);
    let streamed = env
        .m
        .stream
        .stream(env.chat, &body("a new question", rid(9)))
        .await;

    assert_eq!(streamed.names().last(), Some(&"done"));
    assert_eq!(
        streamed.of("stream_started")[0].data["thread_summary_applied"]["token_estimate"],
        estimate,
        "thread_summary_applied is the stored token_estimate"
    );
    let request = env.oagw.proxied().into_iter().last().unwrap().json();
    let input = request["input"].as_array().unwrap();
    let first = input[0]["content"][0]["text"].as_str().unwrap();
    assert!(
        first.starts_with("This conversation has earlier messages that have been summarized."),
        "{first}"
    );
    assert!(first.contains("Summary of the first three turns."));
    let all = request.to_string();
    for compressed in ["question 1", "answer 1", "question 2", "answer 3"] {
        assert!(
            !all.contains(compressed),
            "a compressed message reached the provider: {compressed}"
        );
    }
    assert!(all.contains("a new question"));
    // the compressed messages stay visible in the message list
    let page = env
        .get(&format!("/mini-chat/v1/chats/{}/messages", env.chat))
        .await;
    assert!(page.text.contains("question 1"), "{}", page.text);
}

// ================================================================================================
// failure, retries and the dead letter
// ================================================================================================

#[tokio::test]
async fn a_failed_call_keeps_the_previous_summary_and_is_retried() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(4).await;
    env.seed_summary(env.chat, "previous", turns[1].assistant_message, 21, 9)
        .await;
    let before = (
        env.summary(env.chat).await,
        env.compressed_ids(env.chat).await,
    );
    env.oagw.push_proxy(ProxyReply::json(
        500,
        &json!({ "error": { "message": "boom" } }),
    ));
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[1].assistant_message, 21)),
        boundary(turns[3].assistant_message, 41),
    );

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(
        (
            env.summary(env.chat).await,
            env.compressed_ids(env.chat).await
        ),
        before
    );
    assert!(
        env.usage().await.is_empty(),
        "no usage event without a commit"
    );
    assert_eq!(
        env.metrics.events(),
        vec![
            Metric::SummaryFallback,
            Metric::SummaryExecution(SummaryResult::ProviderError)
        ]
    );
}

#[tokio::test]
async fn the_delivery_that_reaches_max_attempts_is_dead_lettered_with_the_same_metric() {
    let env = WorkerEnv::new().await; // thread_summary_worker.max_attempts 3
    let (_, task) = three_turns(&env).await;
    for _ in 0..3 {
        env.oagw.push_proxy(ProxyReply::json(503, &json!({})));
    }

    let first = deliver(&env, &task, 0).await;
    let second = deliver(&env, &task, 1).await;
    let third = deliver(&env, &task, 2).await;

    assert!(
        is_retry(&first) && is_retry(&second),
        "{first:?} {second:?}"
    );
    assert_eq!(
        rejected(&third),
        Some("thread summary: max attempts (3) reached"),
        "{third:?}"
    );
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::ProviderError)),
        3
    );
    assert_eq!(env.metrics.count(Metric::SummaryFallback), 3);
    assert!(env.summary(env.chat).await.is_none());
}

#[tokio::test]
async fn a_response_without_a_usable_summary_records_empty_summary_and_retries() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    env.oagw.push_proxy(ProxyReply::json(
        200,
        &answer("<analysis>only analysis, never closed", 5, 0),
    ));

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert!(env.summary(env.chat).await.is_none());
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::EmptySummary)]
    );
    assert_eq!(
        env.metrics.count(Metric::SummaryFallback),
        0,
        "provider failures only"
    );
}

#[tokio::test]
async fn an_empty_summary_on_the_last_delivery_is_dead_lettered() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    env.oagw
        .push_proxy(ProxyReply::json(200, &answer("", 0, 0)));

    let result = deliver(&env, &task, 2).await;

    assert!(rejected(&result).is_some(), "{result:?}");
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::EmptySummary)),
        1
    );
}

#[tokio::test]
async fn a_context_length_error_is_retried_twice_dropping_the_oldest_fifth_and_the_commit_covers_the_whole_range()
 {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(5).await; // 10 messages
    let task = WorkerEnv::task(env.chat, None, boundary(turns[4].assistant_message, 51));
    env.oagw.push_proxy(context_error());
    env.oagw.push_proxy(context_error());
    script_summary(&env, "fits at last");

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    let calls = summary_calls(&env);
    assert_eq!(calls.len(), 3);
    let counts: Vec<usize> = calls
        .iter()
        .map(|call| entries(&prompt_of(call)).len())
        .collect();
    assert_eq!(
        counts,
        vec![10, 8, 6],
        "ceil(10/5) = 2 dropped, then ceil(8/5) = 2"
    );
    let last = entries(&prompt_of(&calls[2]));
    assert_eq!(
        last[0], "User: question 3",
        "the oldest messages were dropped"
    );
    assert_eq!(
        env.compressed_ids(env.chat).await.len(),
        10,
        "the whole frozen range is committed"
    );
    assert_eq!(env.summary(env.chat).await.unwrap().text, "fits at last");
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::Success)]
    );
}

#[tokio::test]
async fn a_context_length_error_that_persists_after_two_retries_is_a_provider_failure() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(5).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[4].assistant_message, 51));
    for _ in 0..3 {
        env.oagw.push_proxy(context_error());
    }

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(summary_calls(&env).len(), 3, "the call and two retries");
    assert_eq!(
        env.metrics.events(),
        vec![
            Metric::SummaryFallback,
            Metric::SummaryExecution(SummaryResult::ProviderError)
        ]
    );
    assert!(env.summary(env.chat).await.is_none());
}

#[tokio::test]
async fn a_range_of_two_messages_is_never_retried_on_a_context_length_error() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(1).await;
    let task = WorkerEnv::task(env.chat, None, boundary(turns[0].assistant_message, 11));
    env.oagw.push_proxy(context_error());

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result));
    assert_eq!(summary_calls(&env).len(), 1);
}

#[tokio::test]
async fn a_missing_or_disabled_summary_model_is_rejected_without_a_provider_call() {
    for model in ["missing-model", "sum-off"] {
        let mut summary = ThreadSummaryWorkerConfig::default();
        model.clone_into(&mut summary.summary_model_id);
        let env = WorkerEnv::with(WorkerOptions {
            summary,
            ..WorkerOptions::default()
        })
        .await;
        let (_, task) = three_turns(&env).await;
        let logs = LogCapture::start();

        let result = deliver(&env, &task, 0).await;

        assert!(rejected(&result).is_some(), "{model}: {result:?}");
        assert_eq!(env.oagw.proxy_call_count(), 0, "{model}");
        assert_eq!(
            env.metrics.events(),
            vec![Metric::SummaryExecution(SummaryResult::ModelUnavailable)]
        );
        assert!(
            logs.at_containing(Level::ERROR, model).len() == 1,
            "an error names the model id: {:?}",
            logs.records()
        );
    }
}

#[tokio::test]
async fn an_empty_model_id_means_gpt_4_1_mini_which_the_catalog_may_lack() {
    let summary = ThreadSummaryWorkerConfig {
        summary_model_id: String::new(),
        ..ThreadSummaryWorkerConfig::default()
    };
    let env = WorkerEnv::with(WorkerOptions {
        summary,
        ..WorkerOptions::default()
    })
    .await;
    let (_, task) = three_turns(&env).await;
    let logs = LogCapture::start();

    let result = deliver(&env, &task, 0).await;

    assert!(rejected(&result).is_some());
    assert!(
        !logs.at_containing(Level::ERROR, "gpt-4.1-mini").is_empty(),
        "{:?}",
        logs.records()
    );
}

#[tokio::test]
async fn a_policy_failure_resolving_the_model_is_retry_and_dead_letters_at_the_limit() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    env.quota.policy.fail(PolicyStep::Snapshot);

    let retry = deliver(&env, &task, 0).await;
    let last = deliver(&env, &task, 2).await;

    assert!(is_retry(&retry), "{retry:?}");
    assert!(rejected(&last).is_some(), "{last:?}");
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::Retry)),
        2
    );
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_summary_model_whose_provider_entry_does_not_resolve_is_retry() {
    let mut summary = ThreadSummaryWorkerConfig::default();
    "sum-noprov".clone_into(&mut summary.summary_model_id);
    let env = WorkerEnv::with(WorkerOptions {
        summary,
        ..WorkerOptions::default()
    })
    .await;
    let (_, task) = three_turns(&env).await;

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::Retry)]
    );
}

// ================================================================================================
// the guards before the call
// ================================================================================================

#[tokio::test]
async fn a_work_item_of_a_soft_deleted_or_missing_chat_ends_ok_without_a_call_or_a_metric() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    env.soft_delete_chat(env.chat).await;

    let deleted = deliver(&env, &task, 0).await;
    let mut gone = task;
    gone.chat_id = Uuid::from_u128(0xDEAD);
    let missing = deliver(&env, &gone, 0).await;

    assert!(
        is_ok(&deleted) && is_ok(&missing),
        "{deleted:?} {missing:?}"
    );
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert!(env.metrics.events().is_empty());
}

#[tokio::test]
async fn a_work_item_of_another_tenant_finds_no_chat() {
    let env = WorkerEnv::new().await;
    let (_, mut task) = three_turns(&env).await;
    task.tenant_id = crate::test_doubles::OTHER_TENANT;

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result));
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_base_without_a_stored_summary_is_base_missing_and_ends_ok() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[0].assistant_message, 11)),
        boundary(turns[2].assistant_message, 31),
    );

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::BaseMissing)]
    );
}

#[tokio::test]
async fn a_moved_frontier_is_a_cas_conflict_and_ends_ok_without_a_call() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    env.seed_summary(env.chat, "already there", turns[1].assistant_message, 21, 9)
        .await;
    // a row exists while the base is null
    let first = WorkerEnv::task(env.chat, None, boundary(turns[2].assistant_message, 31));
    // the stored frontier differs from the base
    let moved = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[0].assistant_message, 11)),
        boundary(turns[2].assistant_message, 31),
    );

    let a = deliver(&env, &first, 0).await;
    let b = deliver(&env, &moved, 0).await;

    assert!(is_ok(&a) && is_ok(&b));
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(env.metrics.count(Metric::SummaryCasConflict), 2);
    assert_eq!(
        env.summary(env.chat).await.unwrap().text,
        "already there",
        "nothing changed"
    );
}

#[tokio::test]
async fn a_frozen_target_that_was_deleted_before_the_handler_ran_is_frontier_deleted() {
    let env = WorkerEnv::new().await;
    let (turns, task) = three_turns(&env).await;
    let deleted = env.m.delete(SeedTurn::completed(3, 30).request_id()).await;
    assert_eq!(deleted.status.as_u16(), 204, "{}", deleted.text);

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::FrontierDeleted)]
    );
    assert!(env.summary(env.chat).await.is_none());
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert!(env.usage().await.is_empty());
    let _ = turns;
}

#[tokio::test]
async fn a_frozen_target_that_never_existed_is_frontier_deleted() {
    let env = WorkerEnv::new().await;
    env.conversation(2).await;
    let task = WorkerEnv::task(env.chat, None, boundary(Uuid::from_u128(0xBAD), 99));

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result));
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::FrontierDeleted)]
    );
}

#[tokio::test]
async fn an_empty_range_ends_ok_without_a_call_and_without_a_metric() {
    let env = WorkerEnv::new().await;
    let (turns, task) = three_turns(&env).await;
    let all: Vec<String> = turns
        .iter()
        .flat_map(|t| [t.user_message, t.assistant_message])
        .map(crate::chat_test_env::blob)
        .collect();
    env.exec(&format!(
        "UPDATE messages SET is_compressed = 1 WHERE id IN ({})",
        all.join(", ")
    ))
    .await;
    let logs = LogCapture::start();

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert!(env.metrics.events().is_empty());
    assert!(!logs.at(Level::WARN).is_empty(), "a warning names the chat");
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_without_a_call() {
    let env = WorkerEnv::new().await;
    three_turns(&env).await;

    let result = env
        .summary_handler()
        .handle(&outbox_message(br#"{"tenant_id":"x"}"#.to_vec(), 0))
        .await;

    assert!(rejected(&result).is_some(), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

// ================================================================================================
// database failures
// ================================================================================================

#[tokio::test]
async fn a_failing_read_is_retry_with_the_retry_metric_and_the_dead_letter_rule() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    env.exec("ALTER TABLE thread_summaries RENAME TO thread_summaries_hidden")
        .await;

    let first = deliver(&env, &task, 0).await;
    let last = deliver(&env, &task, 2).await;

    assert!(is_retry(&first), "{first:?}");
    assert!(rejected(&last).is_some(), "{last:?}");
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::Retry)),
        2
    );
    env.exec("ALTER TABLE thread_summaries_hidden RENAME TO thread_summaries")
        .await;
}

#[tokio::test]
async fn a_failing_commit_is_retry_and_commits_nothing() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");
    env.exec("CREATE TRIGGER fail_summary BEFORE INSERT ON thread_summaries BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .await;

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert!(env.summary(env.chat).await.is_none());
    assert!(
        env.compressed_ids(env.chat).await.is_empty(),
        "the transaction rolled back as a whole"
    );
    assert!(env.usage().await.is_empty());
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::Retry)]
    );
}

#[tokio::test]
async fn a_failing_usage_enqueue_rolls_the_whole_commit_back() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");
    env.exec("CREATE TRIGGER fail_outbox BEFORE INSERT ON mini_chat__outbox_body BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .await;

    let result = deliver(&env, &task, 0).await;

    assert!(is_retry(&result), "{result:?}");
    assert!(
        env.summary(env.chat).await.is_none(),
        "summary, frontier, flags and usage commit together"
    );
    assert!(env.compressed_ids(env.chat).await.is_empty());
}

// ================================================================================================
// the guarded commit: races with mutations and with other deliveries
// ================================================================================================

#[tokio::test]
async fn a_delete_of_the_turn_with_the_frozen_target_between_the_call_and_the_commit_writes_nothing()
 {
    let env = Arc::new(WorkerEnv::new().await);
    let (_, task) = three_turns(&env).await;
    let gate = Gate::new();
    env.oagw.push_proxy(
        ProxyReply::json(200, &answer("<summary>stale</summary>", 40, 0)).held_by(&gate),
    );
    let handler = tokio::spawn({
        let env = Arc::clone(&env);
        async move { env.deliver_summary(&task, 0).await }
    });
    gate.arrived().await;

    // the delete commits first
    let deleted = env.m.delete(SeedTurn::completed(3, 30).request_id()).await;
    assert_eq!(deleted.status.as_u16(), 204, "{}", deleted.text);
    gate.release();
    let result = tokio::time::timeout(Duration::from_secs(10), handler)
        .await
        .unwrap()
        .unwrap();

    assert!(is_ok(&result), "{result:?}");
    assert!(
        env.summary(env.chat).await.is_none(),
        "no summary of removed content"
    );
    assert!(
        env.compressed_ids(env.chat).await.is_empty(),
        "no is_compressed change"
    );
    assert!(env.usage().await.is_empty(), "no usage event");
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::FrontierDeleted)]
    );
}

#[tokio::test]
async fn a_summary_deleted_between_the_call_and_the_commit_is_base_missing_and_does_not_reappear() {
    let env = Arc::new(WorkerEnv::new().await);
    let turns = env.conversation(4).await;
    env.seed_summary(env.chat, "old", turns[1].assistant_message, 21, 9)
        .await;
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[1].assistant_message, 21)),
        boundary(turns[3].assistant_message, 41),
    );
    let gate = Gate::new();
    env.oagw.push_proxy(
        ProxyReply::json(200, &answer("<summary>stale</summary>", 40, 0)).held_by(&gate),
    );
    let handler = tokio::spawn({
        let env = Arc::clone(&env);
        async move { env.deliver_summary(&task, 0).await }
    });
    gate.arrived().await;

    // a covering mutation deleted the summary and cleared the flags
    env.exec("DELETE FROM thread_summaries").await;
    gate.release();
    let result = tokio::time::timeout(Duration::from_secs(10), handler)
        .await
        .unwrap()
        .unwrap();

    assert!(is_ok(&result), "{result:?}");
    assert!(
        env.summary(env.chat).await.is_none(),
        "no stale summary reappears"
    );
    assert!(env.compressed_ids(env.chat).await.is_empty());
    assert!(env.usage().await.is_empty());
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::BaseMissing)]
    );
}

#[tokio::test]
async fn a_covering_delete_of_the_latest_turn_before_the_handler_runs_is_base_missing() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    // the summary ends at the question of turn 3: the delete of turn 3 covers it
    env.seed_summary(
        env.chat,
        "ends at the question",
        turns[2].user_message,
        30,
        9,
    )
    .await;
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[2].user_message, 30)),
        boundary(turns[2].assistant_message, 31),
    );
    let deleted = env.m.delete(SeedTurn::completed(3, 30).request_id()).await;
    assert_eq!(deleted.status.as_u16(), 204, "{}", deleted.text);
    assert!(
        env.summary(env.chat).await.is_none(),
        "the mutation invalidated the summary"
    );

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(env.oagw.proxy_call_count(), 0);
    assert_eq!(
        env.metrics.events(),
        vec![Metric::SummaryExecution(SummaryResult::BaseMissing)]
    );
}

#[tokio::test]
async fn a_summary_before_the_mutated_turn_survives_the_mutation_and_the_commit_still_works() {
    let env = WorkerEnv::new().await;
    let turns = env.conversation(3).await;
    env.seed_summary(env.chat, "covers turn 1", turns[0].assistant_message, 11, 9)
        .await;
    let deleted = env.m.delete(SeedTurn::completed(3, 30).request_id()).await;
    assert_eq!(deleted.status.as_u16(), 204);
    assert!(
        env.summary(env.chat).await.is_some(),
        "the frontier is before the mutated turn"
    );
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[0].assistant_message, 11)),
        boundary(turns[1].assistant_message, 21),
    );
    script_summary(&env, "now covers turn 2");

    let result = deliver(&env, &task, 0).await;

    assert!(is_ok(&result), "{result:?}");
    assert_eq!(
        env.summary(env.chat).await.unwrap().text,
        "now covers turn 2"
    );
}

#[tokio::test]
async fn two_deliveries_that_both_reach_the_provider_commit_one_summary_and_the_other_records_a_conflict()
 {
    let env = Arc::new(WorkerEnv::new().await);
    let (_, task) = three_turns(&env).await;
    let gate = Gate::new();
    for text in ["first", "second"] {
        env.oagw.push_proxy(
            ProxyReply::json(200, &answer(&format!("<summary>{text}</summary>"), 40, 0))
                .held_by(&gate),
        );
    }
    let mut handlers = Vec::new();
    for _ in 0..2 {
        let (env, task) = (Arc::clone(&env), task);
        handlers.push(tokio::spawn(
            async move { env.deliver_summary(&task, 0).await },
        ));
    }
    for _ in 0..500 {
        if gate.arrivals() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(gate.arrivals(), 2, "both deliveries reached the provider");
    gate.release();
    let mut results = Vec::new();
    for handler in handlers {
        results.push(
            tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap()
                .unwrap(),
        );
    }

    assert!(results.iter().all(is_ok), "{results:?}");
    assert_eq!(
        env.count("thread_summaries").await,
        1,
        "exactly one committed summary"
    );
    assert_eq!(env.usage().await.len(), 1, "exactly one usage event");
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::Success)),
        1
    );
    assert_eq!(env.metrics.count(Metric::SummaryCasConflict), 1);
    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::BaseMissing)),
        0
    );
}

#[tokio::test]
async fn two_concurrent_commits_from_the_same_base_advance_the_frontier_once() {
    let env = Arc::new(WorkerEnv::new().await);
    let turns = env.conversation(4).await;
    env.seed_summary(env.chat, "old", turns[0].assistant_message, 11, 9)
        .await;
    let task = WorkerEnv::task(
        env.chat,
        Some(boundary(turns[0].assistant_message, 11)),
        boundary(turns[3].assistant_message, 41),
    );
    let gate = Gate::new();
    for text in ["one", "two"] {
        env.oagw.push_proxy(
            ProxyReply::json(200, &answer(&format!("<summary>{text}</summary>"), 40, 0))
                .held_by(&gate),
        );
    }
    let mut handlers = Vec::new();
    for _ in 0..2 {
        let (env, task) = (Arc::clone(&env), task);
        handlers.push(tokio::spawn(
            async move { env.deliver_summary(&task, 0).await },
        ));
    }
    for _ in 0..500 {
        if gate.arrivals() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gate.release();
    for handler in handlers {
        assert!(is_ok(
            &tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .unwrap()
                .unwrap()
        ));
    }

    assert_eq!(
        env.metrics
            .count(Metric::SummaryExecution(SummaryResult::Success)),
        1
    );
    assert_eq!(env.metrics.count(Metric::SummaryCasConflict), 1);
    assert_eq!(env.usage().await.len(), 1);
    assert_eq!(
        env.summary(env.chat).await.unwrap().frontier_id,
        turns[3].assistant_message
    );
}

#[tokio::test]
async fn a_redelivery_after_the_commit_changes_nothing_and_calls_nothing() {
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "S");
    deliver(&env, &task, 0).await;
    let calls = env.oagw.proxy_call_count();
    let before = (
        env.summary(env.chat).await,
        env.compressed_ids(env.chat).await,
        env.usage().await.len(),
    );

    let again = deliver(&env, &task, 1).await;

    assert!(is_ok(&again));
    assert_eq!(env.oagw.proxy_call_count(), calls);
    assert_eq!(
        (
            env.summary(env.chat).await,
            env.compressed_ids(env.chat).await,
            env.usage().await.len()
        ),
        before
    );
    assert_eq!(env.metrics.count(Metric::SummaryCasConflict), 1);
}

#[tokio::test]
async fn a_commit_and_a_covering_delete_started_concurrently_end_in_a_consistent_state() {
    for round in 0..8_u32 {
        let env = Arc::new(
            WorkerEnv::with(WorkerOptions {
                file_db: true,
                ..WorkerOptions::default()
            })
            .await,
        );
        let turns = env.conversation(3).await;
        // the summary ends at the question of turn 3; the delete of turn 3 covers it
        env.seed_summary(env.chat, "old", turns[2].user_message, 30, 9)
            .await;
        let task = WorkerEnv::task(
            env.chat,
            Some(boundary(turns[2].user_message, 30)),
            boundary(turns[2].assistant_message, 31),
        );
        script_summary(&env, "committed");

        let commit = tokio::spawn({
            let (env, task) = (Arc::clone(&env), task);
            async move { env.deliver_summary(&task, 0).await }
        });
        let delete = env.m.delete(SeedTurn::completed(3, 30).request_id()).await;
        let result = commit.await.unwrap();

        assert_eq!(
            delete.status.as_u16(),
            204,
            "round {round}: {}",
            delete.text
        );
        assert!(
            is_ok(&result) || is_retry(&result),
            "round {round}: {result:?}"
        );
        // either the delete ran first (no summary was written) or the commit ran first and the delete
        // invalidated it: in both cases the removed turn is not summarized
        assert!(
            env.summary(env.chat).await.is_none(),
            "round {round}: no summary of the removed turn"
        );
        assert!(
            env.compressed_ids(env.chat).await.is_empty(),
            "round {round}: flags cleared"
        );
    }
}

// ================================================================================================
// deadlines on the injectable clock
// ================================================================================================

/// A summary client whose call never answers on the manual clock.
struct Hanging {
    clock: Arc<ManualClock>,
    calls: AtomicUsize,
}

#[async_trait]
impl SummaryModelClient for Hanging {
    async fn summarize(
        &self,
        _ctx: &SecurityContext,
        _request: &SummaryRequest,
    ) -> Result<SummaryAnswer, SummaryCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.sleep(Duration::from_secs(36_000)).await;
        Err(SummaryCallError::Failed("never".to_owned()))
    }
}

async fn deadline_run(
    claim_timeout_secs: u64,
) -> (Duration, HandlerOutcome, WorkerEnv, Arc<Hanging>) {
    let mut config = ThreadSummaryWorkerConfig::default();
    "sum".clone_into(&mut config.summary_model_id);
    config.claim_timeout_secs = claim_timeout_secs;
    let env = WorkerEnv::with(WorkerOptions {
        summary: config.clone(),
        ..WorkerOptions::default()
    })
    .await;
    let (_, task) = three_turns(&env).await;
    let clock = ManualClock::new();
    let client = Arc::new(Hanging {
        clock: Arc::clone(&clock),
        calls: AtomicUsize::new(0),
    });
    let service = Arc::new(env.summary_service_with(
        Arc::clone(&client) as Arc<dyn SummaryModelClient>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
        config,
    ));
    let delivery = tokio::spawn(async move { service.handle(&task, 0).await });
    let at = clock.advance_when_pending(2).await;
    let outcome = tokio::time::timeout(Duration::from_secs(10), delivery)
        .await
        .unwrap()
        .unwrap();
    (at, outcome, env, client)
}

#[tokio::test]
async fn the_call_deadline_is_the_claim_timeout_minus_ten_seconds_reserved_for_the_commit() {
    let (at, outcome, env, client) = deadline_run(300).await;

    assert_eq!(at, Duration::from_secs(290));
    assert_eq!(outcome, HandlerOutcome::Retry);
    assert_eq!(client.calls.load(Ordering::SeqCst), 1);
    assert!(
        env.summary(env.chat).await.is_none(),
        "the previous summary is kept"
    );
    assert_eq!(
        env.metrics.events(),
        vec![
            Metric::SummaryFallback,
            Metric::SummaryExecution(SummaryResult::ProviderError)
        ]
    );
}

#[tokio::test]
async fn at_the_30_second_minimum_the_call_deadline_is_20_seconds() {
    let (at, outcome, ..) = deadline_run(30).await;

    assert_eq!(at, Duration::from_secs(20));
    assert_eq!(outcome, HandlerOutcome::Retry);
}

// ================================================================================================
// logs
// ================================================================================================

#[tokio::test]
async fn neither_the_summary_text_nor_a_message_is_ever_logged() {
    let logs = LogCapture::start();
    let env = WorkerEnv::new().await;
    let (_, task) = three_turns(&env).await;
    script_summary(&env, "TOP-SECRET-SUMMARY-TEXT");
    deliver(&env, &task, 0).await;
    env.oagw.push_proxy(ProxyReply::json(
        500,
        &json!({ "error": { "message": "TOP-SECRET-PROVIDER-BODY" } }),
    ));
    let moved = WorkerEnv::task(env.chat, None, task.target);
    deliver(&env, &moved, 1).await;

    let text = logs.all_text();

    for secret in [
        "TOP-SECRET-SUMMARY-TEXT",
        "TOP-SECRET-PROVIDER-BODY",
        "question 1",
        "answer 3",
    ] {
        assert!(!text.contains(secret), "{secret} was logged: {text}");
    }
}

#[test]
fn the_helpers_of_this_file_agree_on_the_boundary_format() {
    let at = boundary(Uuid::from_u128(1), 5);
    assert_eq!(
        at,
        MessageBoundary {
            created_at: at.created_at,
            id: Uuid::from_u128(1)
        }
    );
    assert_eq!(stamp(5), ts(5));
}
