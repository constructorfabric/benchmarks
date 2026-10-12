#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(
    clippy::integer_division,
    reason = "the estimate of the prompt size is bytes / bytes_per_token_conservative (DESIGN 3.6)"
)]

//! The thread summary (DESIGN section 3.6 "Thread Summary Update": trigger, durable scheduling,
//! handler stages, prompt format, response parsing, `token_estimate`, CAS commit, system-task usage
//! event; 3.2 "System Task Attribution Rules"; B.5.5 prompts; B.9.4 `thread_summary_worker`).
//!
//! The context window of the premium model is made small (`Setup::small_window`: token budget
//! 2900, effective budget 3000) so that a few kilobytes of history reach the compression threshold
//! (80 %, 2400 tokens) or force the truncation of older messages. Handler stages that do not
//! depend on the trigger are driven with hand-made work items (`enqueue_task`).
//!
//! Token arithmetic (DESIGN 5.5.4 with the `catalog_entry` budgets): the 28 byte system prompt is
//! 118 tokens, a message of 5 bytes 113, of 4000 bytes 1210, of 7600 bytes 2200.

mod billing;
mod common;
mod live;
mod mutation;
mod summary;

use billing::dead_letters;
use common::*;
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestUser};
use mutation::{
    Conversation, all_messages, conversation_in, delete_ok, input_items, live_turns, retry,
};
use serde_json::{Value, json};
use summary::*;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// The conversation of the trigger tests: two small turns, then a 7600 byte third message in the
/// small window (2770 assembled tokens: 92.3 % of the effective budget, no truncation).
struct Triggered {
    c: Conversation,
    task: Value,
    events: Vec<mini_chat::test_support::SseEvent>,
}

async fn triggered_in(setup: Setup, summary_text: &str) -> Triggered {
    let env = setup.small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, summary_text);
    let events = send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;
    let mut enqueued = tasks(&c.env).await;
    assert_eq!(enqueued.len(), 1, "exactly one summary task: {enqueued:?}");
    Triggered {
        c,
        task: enqueued.remove(0),
        events,
    }
}

async fn triggered(summary_text: &str) -> Triggered {
    triggered_in(Setup::new(), summary_text).await
}

fn instant(value: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(value.as_str().expect("an RFC 3339 string"), &Rfc3339).unwrap()
}

// ===========================================================================
// Trigger: proactive, urgent, and when it must not fire
// ===========================================================================

#[tokio::test]
async fn reaching_the_compression_threshold_enqueues_one_summary_task() {
    let t = triggered("SUMMARY-ONE").await;
    let (env, chat) = (&t.c.env, &t.c.chat);

    let task = &t.task;
    assert_eq!(task["tenant_id"], json!(t.c.user.tenant_id));
    assert_eq!(task["chat_id"], json!(chat));
    let system_request_id = Uuid::parse_str(task["system_request_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        system_request_id.get_version_num(),
        4,
        "a server-generated UUID v4"
    );
    assert_eq!(task["system_task_type"], json!("thread_summary_update"));
    assert!(
        task["base_frontier_created_at"].is_null(),
        "no base frontier without a summary"
    );
    assert!(task["base_frontier_message_id"].is_null());
    // The frozen target is the last message of the turn before the causing one; the causing
    // turn itself is never summarized.
    let target = message_at(env, chat, 3).await;
    assert_eq!(task["frozen_target_message_id"], json!(target));
    assert_eq!(
        instant(&task["frozen_target_created_at"]),
        frontier_of(env, target).await.instant()
    );
    assert_eq!(
        done_of(&t.events).event,
        "done",
        "the turn itself completed normally"
    );
}

#[tokio::test]
async fn the_task_is_durable_as_soon_as_the_causing_turn_is_finalized() {
    let t = triggered("SUMMARY-ONE").await;

    // `done` is only sent after the finalization commit; the task is part of that transaction.
    let causing = stream_request_id(&t.events);
    assert_eq!(
        turn_row(&t.c.env, &t.c.chat, causing).await.unwrap().state,
        "completed"
    );
    assert_eq!(tasks(&t.c.env).await.len(), 1);
    let turn_usage =
        t.c.env
            .outbox_payloads(USAGE_QUEUE)
            .await
            .into_iter()
            .filter(|event| event["billing_outcome"] != json!("system_task"))
            .count();
    assert_eq!(
        turn_usage, 3,
        "the three turns settled in their own transactions"
    );
}

#[tokio::test]
async fn the_causing_turn_is_never_part_of_the_frozen_range() {
    let t = triggered("SUMMARY-ONE").await;
    let causing = stream_request_id(&t.events);

    let causing_messages: Vec<Uuid> = all_messages(&t.c.env, &t.c.chat)
        .await
        .into_iter()
        .filter(|message| message.request_id == Some(causing))
        .map(|message| message.id)
        .collect();

    assert_eq!(causing_messages.len(), 2);
    let target: Uuid = serde_json::from_value(t.task["frozen_target_message_id"].clone()).unwrap();
    assert!(!causing_messages.contains(&target));
}

#[tokio::test]
async fn the_turn_that_triggers_the_summary_does_not_get_it_in_its_own_context() {
    let t = triggered("SUMMARY-ONE").await;

    let request = turn_requests(&t.c.env).pop().unwrap();
    let body = request.body_json.unwrap();
    assert!(!input_text(&body).contains(PREAMBLE), "{body}");
    assert!(started(&t.events).data["thread_summary_applied"].is_null());
    assert_eq!(wait_outcomes(&t.c.env, 1).await, outcomes(&["ok"]));
}

#[tokio::test]
async fn the_handler_commits_the_summary_for_the_next_turns() {
    let t = triggered("SUMMARY-ONE").await;

    assert_eq!(wait_outcomes(&t.c.env, 1).await, outcomes(&["ok"]));

    let row = summary_row(&t.c.env, &t.c.chat)
        .await
        .expect("the summary is stored");
    assert_eq!(row.text.trim(), "SUMMARY-ONE");
    let target = message_at(&t.c.env, &t.c.chat, 3).await;
    assert_eq!(row.frontier.message_id, target);
    assert_eq!(
        compressed_ids(&t.c.env, &t.c.chat).await,
        first_messages(&t.c.env, &t.c.chat, 4).await
    );
}

async fn assembled_92_percent_triggers_at(pct: u32) -> usize {
    let env = Setup::new()
        .small_window()
        .worker(json!({ "compression_threshold_pct": pct }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, "S");
    send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;
    tasks(&c.env).await.len()
}

#[tokio::test]
async fn the_proactive_threshold_is_a_percentage_of_the_effective_budget_without_deductions() {
    // assembled = 118 + 4 * 113 + 2200 = 2770 tokens; effective budget = min(max_input, 4000 -
    // 1000) = 3000 (the 100 fixed overhead tokens are not subtracted): 2770 / 3000 = 92.3 %.
    assert_eq!(
        assembled_92_percent_triggers_at(92).await,
        1,
        "92 % of 3000 is 2760 <= 2770"
    );
    assert_eq!(
        assembled_92_percent_triggers_at(93).await,
        0,
        "93 % of 3000 is 2790 > 2770"
    );
}

#[tokio::test]
async fn a_conversation_below_the_threshold_enqueues_nothing() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(
        env,
        &[
            ("Q-ONE", "A-ONE"),
            ("Q-TWO", "A-TWO"),
            ("Q-THREE", "A-THREE"),
        ],
    )
    .await;

    assert!(tasks(&c.env).await.is_empty());
    assert!(summary_requests(&c.env).is_empty());
}

#[tokio::test]
async fn a_turn_that_has_to_drop_older_messages_enqueues_an_urgent_summary() {
    // Window 4000 / output 1000: token budget 2900. The first pair (1210 + 1210) cannot stay
    // next to the 1210 token message and the system prompt, so the second turn drops it. The
    // assembled context (1328 tokens) is far below the 2400 token threshold: only the urgent
    // condition (messages_truncated) can fire.
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[(&medium("Q-ONE"), &medium("A-ONE"))]).await;
    enqueue_text(&c.env, &["A-TWO"], 10, 5);
    enqueue_summary(&c.env, "SUMMARY-ONE");

    send_ok(&c.env, &c.user, &c.chat, &medium("Q-TWO")).await;

    let enqueued = tasks(&c.env).await;
    assert_eq!(enqueued.len(), 1, "{enqueued:?}");
    let target = message_at(&c.env, &c.chat, 1).await;
    assert_eq!(enqueued[0]["frozen_target_message_id"], json!(target));
    assert!(enqueued[0]["base_frontier_message_id"].is_null());
    assert!(
        !input_text(&last_turn_body(&c.env)).contains("Q-ONE"),
        "precondition: the older pair was dropped from the context"
    );
}

fn last_turn_body(env: &TestEnv) -> Value {
    turn_requests(env).pop().unwrap().body_json.unwrap()
}

#[tokio::test]
async fn an_urgent_summary_after_a_summary_carries_the_stored_frontier_as_its_base() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[(&medium("Q-ONE"), &medium("A-ONE"))]).await;
    // DESIGN 3.6 "Summary trigger based on token budget" (urgent): the second turn's answer is a
    // 4000 byte message too, so that the next turn (summary 159 + Q-TWO 1210 + A-TWO 1210 + Q-THREE
    // 1210 + system prompt 118 = 3907 tokens) has to drop the pair after the frontier. With the
    // 5 byte answer the context is 2810 tokens, nothing is dropped and, a summary existing, the
    // trigger must not fire.
    enqueue_text(&c.env, &[medium("A-TWO").as_str()], 10, 5);
    enqueue_summary(&c.env, "SUMMARY-ONE");
    send_ok(&c.env, &c.user, &c.chat, &medium("Q-TWO")).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    let first = summary_row(&c.env, &c.chat)
        .await
        .expect("the first summary");
    // The next turn again drops the older pair that is not covered by the summary.
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, "SUMMARY-TWO");

    send_ok(&c.env, &c.user, &c.chat, &medium("Q-THREE")).await;

    let enqueued = tasks(&c.env).await;
    assert_eq!(enqueued.len(), 2, "{enqueued:?}");
    let second = &enqueued[1];
    assert_eq!(
        second["base_frontier_message_id"],
        json!(first.frontier.message_id)
    );
    assert_eq!(
        instant(&second["base_frontier_created_at"]),
        first.frontier.instant()
    );
    let target = message_at(&c.env, &c.chat, 3).await;
    assert_eq!(second["frozen_target_message_id"], json!(target));
    assert_ne!(
        second["system_request_id"], enqueued[0]["system_request_id"],
        "every task has its own system_request_id"
    );
    assert_eq!(wait_outcomes(&c.env, 2).await, outcomes(&["ok", "ok"]));
}

#[tokio::test]
async fn with_a_summary_and_no_truncation_the_trigger_does_not_fire() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    let text = "S".repeat(400);
    seed_summary(&c.env, &c.user, &c.chat, &text, 1, 100).await;

    // Assembled: 118 + summary (~270) + 2200 >= 2400, but a summary exists and nothing is dropped.
    let events = send_ok(&c.env, &c.user, &c.chat, &big("Q-TWO")).await;

    assert!(
        !started(&events).data["thread_summary_applied"].is_null(),
        "precondition: the summary was part of the context"
    );
    assert!(
        tasks(&c.env).await.is_empty(),
        "the existing summary is still effective"
    );
}

#[tokio::test]
async fn a_disabled_worker_enqueues_no_task_even_above_the_threshold() {
    let env = Setup::new()
        .small_window()
        .worker(json!({ "enabled": false }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_text(&c.env, &["A-THREE"], 10, 5);

    let events = send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;

    assert_eq!(done_of(&events).event, "done");
    assert!(tasks(&c.env).await.is_empty());
    assert!(summary_requests(&c.env).is_empty());
}

#[tokio::test]
async fn a_disabled_worker_enqueues_no_urgent_task_either() {
    let env = Setup::new()
        .small_window()
        .worker(json!({ "enabled": false }))
        .build()
        .await;
    let c = conversation_in(env, &[(&medium("Q-ONE"), &medium("A-ONE"))]).await;

    send_ok(&c.env, &c.user, &c.chat, &medium("Q-TWO")).await;

    assert!(tasks(&c.env).await.is_empty());
}

#[tokio::test]
async fn without_an_earlier_message_there_is_nothing_to_summarize() {
    let env = Setup::new().small_window().build().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    // 8400 bytes: 2420 + 118 = 2538 assembled tokens, above the threshold, but the chat is empty.
    send_ok(&env, &user, &chat, &marked("Q-ONE", 8400)).await;

    assert!(tasks(&env).await.is_empty());
}

#[tokio::test]
async fn a_failed_turn_enqueues_no_summary_task() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_chat(&c.env, failed_script("boom", None));

    let resp = send_text(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;

    assert_eq!(terminal(&expect_stream(&resp)).event, "error");
    assert!(tasks(&c.env).await.is_empty());
}

#[tokio::test]
async fn soft_deleted_messages_are_never_the_frozen_target() {
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    delete_ok(&c.env, &c.user, &c.chat, c.last().request_id).await;
    enqueue_text(&c.env, &["A-THREE"], 10, 5);
    enqueue_summary(&c.env, "S");

    send_ok(&c.env, &c.user, &c.chat, &big("Q-THREE")).await;

    let enqueued = tasks(&c.env).await;
    assert_eq!(enqueued.len(), 1, "{enqueued:?}");
    let a_one = message_at(&c.env, &c.chat, 1).await;
    assert_eq!(
        enqueued[0]["frozen_target_message_id"],
        json!(a_one),
        "the deleted turn's messages are skipped: the latest live message before the causing turn"
    );
}

#[tokio::test]
async fn retrying_the_big_turn_enqueues_no_second_task_while_a_summary_exists() {
    // Two turns, the second one big: its own finalization enqueues the first task.
    let env = Setup::new().small_window().build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    enqueue_text(&c.env, &["A-TWO"], 10, 5);
    enqueue_summary(&c.env, "S1");
    // 2 history tokens x 113 + 118 + 2200 = 2544 >= 2400.
    let events = send_ok(&c.env, &c.user, &c.chat, &big("Q-TWO")).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    let big_turn = stream_request_id(&events);
    // Retrying the big turn: the summary (frontier A-ONE) is before its user message, so it
    // is kept; nothing is truncated and a summary exists, so no second task is needed.
    enqueue_text(&c.env, &["A-TWO-AGAIN"], 10, 5);

    expect_stream(&retry(&c.env, &c.user, &c.chat, big_turn).await);

    assert_eq!(tasks(&c.env).await.len(), 1);
    assert_eq!(live_turns(&c.env, &c.chat).await.len(), 2);
}

#[tokio::test]
async fn the_trigger_does_not_check_the_summary_model_so_turns_keep_enqueuing_work() {
    let t = triggered_in(
        Setup::new().worker(json!({ "summary_model_id": "no-such-model" })),
        "unused",
    )
    .await;

    assert_eq!(done_of(&t.events).event, "done");
    assert_eq!(wait_outcomes(&t.c.env, 1).await, outcomes(&["reject"]));
}

// ===========================================================================
// Handler: the summary request
// ===========================================================================

async fn run_task_up_to(c: &Conversation, index: usize, summary: &str) -> Uuid {
    enqueue_summary(&c.env, summary);
    let system_request_id = enqueue_task_up_to(&c.env, &c.user, &c.chat, index).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));
    system_request_id
}

async fn quiet_three_turns() -> Conversation {
    quiet_conversation(&[
        ("Q-ONE", "A-ONE"),
        ("Q-TWO", "A-TWO"),
        ("Q-THREE", "A-THREE"),
    ])
    .await
}

#[tokio::test]
async fn the_handler_makes_one_non_streaming_call_to_the_summary_model() {
    let env = Setup::new()
        .gear(json!({ "streaming": { "max_output_tokens": 200 } }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;

    run_task_up_to(&c, 3, "S").await;

    let requests = summary_requests(&c.env);
    assert_eq!(requests.len(), 1, "one provider call");
    assert!(
        requests[0].path.ends_with("/v1/responses"),
        "{}",
        requests[0].path
    );
    let body = requests[0].body_json.clone().unwrap();
    assert_eq!(
        body["model"],
        json!(PROVIDER_SUMMARY_MODEL),
        "the catalog provider_model_id"
    );
    assert_eq!(body["stream"], json!(false), "a non-streaming call");
    assert_eq!(
        body["max_output_tokens"],
        json!(SUMMARY_MAX_OUTPUT),
        "the summary model's catalog max_output_tokens, not capped by streaming.max_output_tokens"
    );
}

#[tokio::test]
async fn the_summary_request_carries_the_system_identity_and_summary_metadata() {
    let c = quiet_three_turns().await;

    run_task_up_to(&c, 3, "S").await;

    let body = summary_request(&c.env);
    let system_subject = Uuid::parse_str(SYSTEM_SUBJECT).unwrap();
    assert_eq!(
        body["user"],
        json!(format!(
            "{}{}",
            c.user.tenant_id.simple(),
            system_subject.simple()
        ))
    );
    assert_eq!(body["user"].as_str().unwrap().len(), 64);
    assert_eq!(
        body["metadata"],
        json!({
            "tenant_id": c.user.tenant_id.to_string(),
            "user_id": SYSTEM_SUBJECT,
            "chat_id": c.chat,
            "request_type": "summary",
            "feature": "none",
        })
    );
    assert!(
        body.get("tools").is_none(),
        "a summary request offers no tools: {body}"
    );
}

async fn summary_system_prompt(setup: Setup) -> String {
    let env = setup.build().await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    run_task_up_to(&c, 3, "S").await;
    system_prompt_of(&summary_request(&c.env))
}

#[tokio::test]
async fn the_catalog_thread_summary_prompt_is_the_system_prompt() {
    let prompt = summary_system_prompt(
        Setup::new()
            .summary_entry(json!({ "thread_summary_prompt": "CATALOG-PROMPT" }))
            .worker(json!({ "summary_system_prompt": "WORKER-PROMPT" })),
    )
    .await;

    assert_eq!(prompt, "CATALOG-PROMPT");
}

#[tokio::test]
async fn without_a_catalog_prompt_the_configured_summary_system_prompt_is_used() {
    let prompt = summary_system_prompt(
        Setup::new().worker(json!({ "summary_system_prompt": "WORKER-PROMPT" })),
    )
    .await;

    assert_eq!(prompt, "WORKER-PROMPT");
}

#[tokio::test]
async fn without_any_configured_prompt_the_built_in_default_is_used() {
    let prompt = summary_system_prompt(Setup::new()).await;

    assert_eq!(prompt, DEFAULT_SUMMARY_PROMPT);
}

#[tokio::test]
async fn the_user_prompt_lists_the_messages_of_a_first_summary() {
    let c = quiet_three_turns().await;

    run_task_up_to(&c, 3, "S").await;

    let body = summary_request(&c.env);
    let items = input_items(&body);
    assert_eq!(
        items.len(),
        1,
        "one user message carries the whole prompt: {items:?}"
    );
    assert_eq!(items[0].0, "user");
    let prompt = &items[0].1;
    assert!(prompt.starts_with(OPENING_NEW), "{prompt}");
    let entries = "User: Q-ONE\n\nAssistant: A-ONE\n\nUser: Q-TWO\n\nAssistant: A-TWO";
    let at = prompt
        .find(entries)
        .unwrap_or_else(|| panic!("entries missing: {prompt}"));
    assert!(at >= OPENING_NEW.len());
    assert!(
        !prompt.contains("Q-THREE") && !prompt.contains("A-THREE"),
        "after the frozen target"
    );
    assert!(!prompt.contains("<existing_summary>"), "{prompt}");
    let instruction = prompt
        .find(ANALYSIS_INSTRUCTION_START)
        .expect("the analysis instruction");
    assert!(
        instruction > at + entries.len(),
        "the instruction ends the prompt"
    );
    assert!(
        prompt.trim_end().ends_with(ANALYSIS_INSTRUCTION_END),
        "{prompt}"
    );
    for section in SECTIONS {
        assert!(
            prompt[instruction..].contains(section),
            "section `{section}` missing"
        );
    }
}

#[tokio::test]
async fn the_user_prompt_of_an_update_carries_the_existing_summary_and_only_the_new_messages() {
    let c = quiet_three_turns().await;
    let base = seed_summary(&c.env, &c.user, &c.chat, "OLD-SUMMARY-TEXT", 1, 10).await;
    enqueue_summary(&c.env, "NEW-SUMMARY");
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 5).await).await;

    enqueue_task(&c.env, &c.user, &c.chat, Some(&base), &target).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));

    let prompt = user_prompt(&summary_request(&c.env));
    assert!(!prompt.contains(OPENING_NEW), "{prompt}");
    let position = |needle: &str| {
        prompt
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` is missing: {prompt}"))
    };
    assert!(position(OPENING_UPDATE_1) < position("<existing_summary>"));
    assert!(position("<existing_summary>") < position("OLD-SUMMARY-TEXT"));
    assert!(position("OLD-SUMMARY-TEXT") < position("</existing_summary>"));
    assert!(
        position(OPENING_UPDATE_2) < position("<existing_summary>")
            || position(OPENING_UPDATE_2) < position(NEW_MESSAGES_MARKER)
    );
    assert!(position("</existing_summary>") < position(NEW_MESSAGES_MARKER));
    let entries = "User: Q-TWO\n\nAssistant: A-TWO\n\nUser: Q-THREE\n\nAssistant: A-THREE";
    assert!(position(NEW_MESSAGES_MARKER) < position(entries));
    assert!(
        !prompt.contains("Q-ONE") && !prompt.contains("A-ONE"),
        "messages covered by the summary are not listed again"
    );
    assert!(position(entries) < position(ANALYSIS_INSTRUCTION_START));
}

async fn prompt_with_limit(limit: u32, turns: &[(&str, &str)]) -> String {
    let env = Setup::new()
        .worker(json!({ "message_content_limit": limit }))
        .build()
        .await;
    let c = conversation_in(env, turns).await;
    run_task_up_to(&c, 2 * turns.len() - 1, "S").await;
    user_prompt(&summary_request(&c.env))
}

/// The text of the entry that starts with `label` up to the blank line (or the end of the
/// entries).
fn entry_of<'a>(prompt: &'a str, label: &str) -> &'a str {
    let start = prompt
        .find(label)
        .unwrap_or_else(|| panic!("`{label}` missing: {prompt}"));
    let rest = &prompt[start..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    &rest[..end]
}

#[tokio::test]
async fn message_content_over_the_limit_is_cut_and_ends_with_an_ellipsis() {
    let prompt = prompt_with_limit(
        10,
        &[("0123456789ABCDEFGHIJ", "short"), ("ABCDEFGHIJ", "ok")],
    )
    .await;

    let entry = entry_of(&prompt, "User: 0123456");
    let text = &entry["User: ".len()..];
    assert!(text.ends_with("..."), "{entry}");
    assert!(text.starts_with("0123456"), "{entry}");
    assert!(
        !text.contains("ABCDE"),
        "the tail beyond the limit is gone: {entry}"
    );
    assert!(
        (10..=13).contains(&text.chars().count()),
        "cut to the limit, plus the ellipsis: {entry}"
    );
}

#[tokio::test]
async fn message_content_at_or_below_the_limit_is_kept_whole() {
    let prompt = prompt_with_limit(
        10,
        &[("0123456789ABCDEFGHIJ", "short"), ("ABCDEFGHIJ", "ok")],
    )
    .await;

    assert_eq!(
        entry_of(&prompt, "User: ABCDEFGHIJ"),
        "User: ABCDEFGHIJ",
        "exactly the limit"
    );
    assert_eq!(entry_of(&prompt, "Assistant: short"), "Assistant: short");
    assert_eq!(entry_of(&prompt, "Assistant: ok"), "Assistant: ok");
}

#[tokio::test]
async fn a_zero_message_content_limit_disables_the_truncation() {
    let long = marked("LONG-", 9000);

    let prompt = prompt_with_limit(0, &[(&long, "short"), ("Q", "A")]).await;

    assert!(
        prompt.contains(&long),
        "the whole 9000 byte message is in the prompt"
    );
    assert!(!prompt.contains("..."), "no truncation marker");
}

#[tokio::test]
async fn the_default_limit_cuts_messages_at_4000_characters() {
    let long = marked("LONG-", 5000);
    let env = Setup::new().build().await;
    let c = conversation_in(env, &[(&long, "short"), ("Q", "A")]).await;

    run_task_up_to(&c, 3, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));
    let entry = entry_of(&prompt, "User: LONG-");
    assert!(entry.ends_with("..."), "{}", &entry[entry.len() - 20..]);
    let text = entry["User: ".len()..].chars().count();
    assert!((4000..=4003).contains(&text), "{text}");
}

#[tokio::test]
async fn messages_after_the_frozen_target_are_not_part_of_the_run() {
    let c = quiet_three_turns().await;

    run_task_up_to(&c, 3, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));
    assert!(!prompt.contains("Q-THREE") && !prompt.contains("A-THREE"));
    let compressed = compressed_ids(&c.env, &c.chat).await;
    assert_eq!(compressed, first_messages(&c.env, &c.chat, 4).await);
}

#[tokio::test]
async fn soft_deleted_messages_inside_the_range_are_not_summarized() {
    let c = quiet_three_turns().await;
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(id) IN ('{}', '{}')",
            uuid_hex(message_at(&c.env, &c.chat, 2).await),
            uuid_hex(message_at(&c.env, &c.chat, 3).await)
        ),
    )
    .await;

    run_task_up_to(&c, 5, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));
    assert!(
        !prompt.contains("Q-TWO") && !prompt.contains("A-TWO"),
        "{prompt}"
    );
    assert!(
        prompt.contains("User: Q-ONE\n\nAssistant: A-ONE\n\nUser: Q-THREE\n\nAssistant: A-THREE")
    );
}

#[tokio::test]
async fn already_compressed_messages_are_not_summarized_again() {
    let c = quiet_three_turns().await;
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET is_compressed = 1 WHERE hex(id) IN ('{}', '{}')",
            uuid_hex(message_at(&c.env, &c.chat, 0).await),
            uuid_hex(message_at(&c.env, &c.chat, 1).await)
        ),
    )
    .await;

    run_task_up_to(&c, 3, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));
    assert!(
        !prompt.contains("Q-ONE") && !prompt.contains("A-ONE"),
        "{prompt}"
    );
    assert!(prompt.contains("User: Q-TWO\n\nAssistant: A-TWO"));
}

#[tokio::test]
async fn the_range_order_is_created_at_then_id() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    // Every message gets the same created_at: only the id decides the order and the frontier.
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET created_at = '2026-01-01T00:00:00Z' WHERE hex(chat_id) = '{}'",
            uuid_hex(Uuid::parse_str(&c.chat).unwrap())
        ),
    )
    .await;
    let mut messages: Vec<(Uuid, String)> = all_messages(&c.env, &c.chat)
        .await
        .into_iter()
        .map(|message| {
            let label = if message.role == "user" {
                "User"
            } else {
                "Assistant"
            };
            (message.id, format!("{label}: {}", message.content))
        })
        .collect();
    messages.sort_by_key(|(id, _)| *id);
    let target = frontier_of(&c.env, messages[2].0).await;
    enqueue_summary(&c.env, "S");

    enqueue_task(&c.env, &c.user, &c.chat, None, &target).await;
    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["ok"]));

    let expected = messages[..3]
        .iter()
        .map(|(_, entry)| entry.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = user_prompt(&summary_request(&c.env));
    assert!(
        prompt.contains(&expected),
        "expected `{expected}` in {prompt}"
    );
    assert!(
        !prompt.contains(&messages[3].1),
        "the message after the frontier id is outside the range: {prompt}"
    );
    let compressed = compressed_ids(&c.env, &c.chat).await;
    let mut expected_ids: Vec<Uuid> = messages[..3].iter().map(|(id, _)| *id).collect();
    expected_ids.sort();
    let mut got = compressed;
    got.sort();
    assert_eq!(got, expected_ids);
}

// ---------------------------------------------------------------------------
// Fitting the prompt to the summary model
// ---------------------------------------------------------------------------

/// Ten messages of about 600 bytes (five turns) in a summary model with a 2000 token window and
/// 500 output tokens: an input budget of 1500 tokens (6000 bytes at 4 bytes per token).
fn fitting_turns() -> Vec<(String, String)> {
    (1..=5)
        .map(|n| {
            (
                marked(&format!("QQ{n:02}-"), 600),
                marked(&format!("AA{n:02}-"), 600),
            )
        })
        .collect()
}

fn entry_texts(turns: &[(String, String)]) -> Vec<String> {
    turns
        .iter()
        .flat_map(|(question, answer)| {
            [format!("User: {question}"), format!("Assistant: {answer}")]
        })
        .collect()
}

#[tokio::test]
async fn an_oversized_prompt_drops_the_oldest_messages_in_steps_of_a_fifth() {
    let turns = fitting_turns();
    let pairs: Vec<(&str, &str)> = turns
        .iter()
        .map(|(q, a)| (q.as_str(), a.as_str()))
        .collect();
    let setup =
        Setup::new().summary_entry(json!({ "context_window": 2000, "max_output_tokens": 500 }));
    let c = conversation_in(setup.build().await, &pairs).await;
    run_task_up_to(&c, 9, "S").await;

    let body = summary_request(&c.env);
    let prompt = user_prompt(&body);
    let entries = entry_texts(&turns);
    let kept = entries
        .iter()
        .filter(|entry| prompt.contains(entry.as_str()))
        .count();
    // 10 -> 8 -> 6 -> 4 -> 3 -> 2: whole steps of ceil(n / 5), at least two messages stay.
    assert!(
        [8, 6, 4, 3, 2].contains(&kept),
        "kept {kept} of 10 messages"
    );
    let newest = &entries[entries.len() - kept..];
    for entry in newest {
        assert!(
            prompt.contains(entry.as_str()),
            "the newest {kept} messages are kept"
        );
    }
    for entry in &entries[..entries.len() - kept] {
        assert!(
            !prompt.contains(entry.as_str()),
            "only the oldest messages are dropped"
        );
    }
    let total = system_prompt_of(&body).len() + prompt.len();
    assert!(
        total / 4 <= 1500,
        "{total} bytes do not fit 1500 tokens at 4 bytes per token"
    );
    // The step before (more messages) did not fit: the prompt overhead is the same.
    let joined_len = |count: usize| {
        entries[entries.len() - count..]
            .iter()
            .map(String::len)
            .sum::<usize>()
            + 2 * (count - 1)
    };
    let overhead = total - joined_len(kept);
    let previous = match kept {
        8 => 10,
        6 => 8,
        4 => 6,
        3 => 4,
        _ => 3,
    };
    assert!(
        (overhead + joined_len(previous)) / 4 > 1500,
        "dropping stopped as soon as the prompt fit: {kept} kept, overhead {overhead} bytes"
    );
}

#[tokio::test]
async fn a_summary_model_without_a_context_window_gets_the_whole_prompt() {
    let turns = fitting_turns();
    let pairs: Vec<(&str, &str)> = turns
        .iter()
        .map(|(q, a)| (q.as_str(), a.as_str()))
        .collect();
    let setup =
        Setup::new().summary_entry(json!({ "context_window": 0, "max_output_tokens": 500 }));
    let c = conversation_in(setup.build().await, &pairs).await;
    run_task_up_to(&c, 9, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));

    for entry in entry_texts(&turns) {
        assert!(
            prompt.contains(&entry),
            "no fitting when the catalog context_window is 0"
        );
    }
}

#[tokio::test]
async fn max_input_tokens_caps_the_summary_prompt_budget() {
    let turns = fitting_turns();
    let pairs: Vec<(&str, &str)> = turns
        .iter()
        .map(|(q, a)| (q.as_str(), a.as_str()))
        .collect();
    // The window alone would allow the whole prompt, max_input_tokens (1500) does not.
    let setup = Setup::new().summary_entry(json!({
        "context_window": 128_000, "max_output_tokens": 500, "max_input_tokens": 1500
    }));
    let c = conversation_in(setup.build().await, &pairs).await;
    run_task_up_to(&c, 9, "S").await;

    let body = summary_request(&c.env);

    let total = system_prompt_of(&body).len() + user_prompt(&body).len();
    assert!(total / 4 <= 1500, "{total}");
    assert!(
        !user_prompt(&body).contains("QQ01-"),
        "the oldest messages were dropped"
    );
}

#[tokio::test]
async fn the_prompt_keeps_at_least_two_messages_even_over_budget() {
    let setup =
        Setup::new().summary_entry(json!({ "context_window": 1000, "max_output_tokens": 900 }));
    let c = conversation_in(
        setup.build().await,
        &[
            ("Q-ONE", "A-ONE"),
            ("Q-TWO", "A-TWO"),
            ("Q-THREE", "A-THREE"),
        ],
    )
    .await;
    run_task_up_to(&c, 5, "S").await;

    let prompt = user_prompt(&summary_request(&c.env));

    // The overhead alone exceeds the 100 token budget: 6 -> 4 -> 3 -> 2 messages remain.
    assert!(
        prompt.contains("User: Q-THREE\n\nAssistant: A-THREE"),
        "{prompt}"
    );
    for dropped in ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO"] {
        assert!(
            !prompt.contains(dropped),
            "`{dropped}` was dropped: {prompt}"
        );
    }
}

// ===========================================================================
// Handler: response parsing
// ===========================================================================

/// Runs a one-attempt task whose provider answers `raw`; the stored summary text, if any.
async fn stored_for(raw: &str) -> (Option<String>, Vec<String>) {
    let env = Setup::new()
        .worker(json!({ "max_attempts": 1 }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    enqueue_summary_raw(&c.env, raw, 100, 30, 0);
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;
    let result = wait_outcomes(&c.env, 1).await;
    (
        summary_row(&c.env, &c.chat).await.map(|row| row.text),
        result,
    )
}

#[tokio::test]
async fn the_analysis_block_is_removed_and_the_summary_block_is_stored() {
    let (stored, result) = stored_for(
        "<analysis>private reasoning about the chat</analysis>\n<summary>\nTHE SUMMARY\n</summary>",
    )
    .await;

    assert_eq!(result, outcomes(&["ok"]));
    assert_eq!(stored.unwrap().trim(), "THE SUMMARY");
}

#[tokio::test]
async fn only_the_text_inside_the_summary_block_is_stored() {
    let (stored, _) = stored_for(
        "preface <analysis>a</analysis> middle <summary>INSIDE</summary> trailing words",
    )
    .await;

    assert_eq!(stored.unwrap().trim(), "INSIDE");
}

#[tokio::test]
async fn runs_of_blank_lines_in_the_summary_are_collapsed() {
    let (stored, _) = stored_for("<summary>one\n\n\n\ntwo\n\n\n\n\nthree</summary>").await;

    assert_eq!(stored.unwrap().trim(), "one\n\ntwo\n\nthree");
}

#[tokio::test]
async fn without_a_summary_block_the_remaining_text_is_stored() {
    let (stored, result) = stored_for("<analysis>x</analysis>\nPlain summary text").await;

    assert_eq!(result, outcomes(&["ok"]));
    assert_eq!(stored.unwrap().trim(), "Plain summary text");
}

#[tokio::test]
async fn a_plain_answer_without_any_block_is_stored_with_collapsed_blank_lines() {
    let (stored, _) = stored_for("Just text\n\n\n\nmore text").await;

    assert_eq!(stored.unwrap().trim(), "Just text\n\nmore text");
}

#[tokio::test]
async fn leftover_markup_without_a_complete_summary_block_gives_an_empty_result() {
    for raw in [
        "<summary>never closed, so this is not a block",
        "<analysis>never closed analysis and nothing else",
        "text with a stray <summary tag inside",
    ] {
        let (stored, result) = stored_for(raw).await;

        assert_eq!(stored, None, "`{raw}` stores nothing");
        assert_eq!(
            result,
            outcomes(&["reject"]),
            "an empty result is a retry (reject on the last attempt)"
        );
    }
}

#[tokio::test]
async fn a_blank_answer_is_an_empty_summary() {
    for raw in [
        "",
        "   \n\n  ",
        "<summary>   </summary>",
        "<analysis>only reasoning</analysis>",
    ] {
        let (stored, result) = stored_for(raw).await;

        assert_eq!(stored, None, "`{raw}`");
        assert_eq!(result, outcomes(&["reject"]), "`{raw}`");
    }
}

#[tokio::test]
async fn an_empty_summary_is_retried_and_the_next_answer_is_stored() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    enqueue_summary_raw(&c.env, "   ", 100, 30, 0);
    enqueue_summary(&c.env, "GOOD-SUMMARY");
    let sid = enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(wait_outcomes(&c.env, 2).await, outcomes(&["retry", "ok"]));

    assert_eq!(
        summary_row(&c.env, &c.chat).await.unwrap().text.trim(),
        "GOOD-SUMMARY"
    );
    let usage = system_usage_events(&c.env).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(
        usage[0]["request_id"],
        json!(sid),
        "the same system_request_id on the retry"
    );
}

// ===========================================================================
// Handler: the committed state
// ===========================================================================

#[tokio::test]
async fn the_commit_stores_the_summary_advances_the_frontier_and_marks_the_range() {
    let c = quiet_three_turns().await;
    let target = message_at(&c.env, &c.chat, 3).await;

    run_task_up_to(&c, 3, "THE SUMMARY").await;

    let row = summary_row(&c.env, &c.chat).await.expect("the summary row");
    assert_eq!(row.text.trim(), "THE SUMMARY");
    assert_eq!(row.tenant_id, c.user.tenant_id);
    assert_eq!(
        row.frontier.message_id, target,
        "the frontier is the frozen target"
    );
    assert_eq!(
        row.frontier.instant(),
        frontier_of(&c.env, target).await.instant()
    );
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 4).await,
        "exactly the summarized range is compressed"
    );
    let page = list_messages_ok(&c.env, &c.user, &c.chat, &[("limit", "100")]).await;
    assert_eq!(
        items(&page).len(),
        6,
        "compressed messages stay visible in the history"
    );
}

#[tokio::test]
async fn the_token_estimate_is_output_tokens_minus_reasoning_tokens() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    enqueue_summary_raw(
        &c.env,
        "<summary>ABCDEFGHIJKLMNOPQRSTU</summary>",
        500,
        200,
        50,
    );
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;
    wait_outcomes(&c.env, 1).await;

    assert_eq!(
        summary_row(&c.env, &c.chat).await.unwrap().token_estimate,
        150
    );
}

async fn estimate_for(usage: Option<(u64, u64, u64)>) -> i64 {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    let mut body = summary_body("<summary>ABCDEFGHIJKLMNOPQRSTU</summary>", 0, 0, 0);
    match usage {
        Some((input, output, reasoning)) => {
            body = summary_body(
                "<summary>ABCDEFGHIJKLMNOPQRSTU</summary>",
                input,
                output,
                reasoning,
            );
        }
        None => {
            body.as_object_mut().unwrap().remove("usage");
        }
    }
    enqueue_chat(&c.env, ScriptedResponse::Json { status: 200, body });
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;
    wait_outcomes(&c.env, 1).await;
    summary_row(&c.env, &c.chat).await.unwrap().token_estimate
}

#[tokio::test]
async fn a_non_positive_output_minus_reasoning_falls_back_to_a_quarter_of_the_bytes() {
    // 21 bytes of summary: ceil(21 / 4) = 6.
    assert_eq!(
        estimate_for(Some((100, 0, 0))).await,
        6,
        "the provider reports 0 output tokens"
    );
    assert_eq!(
        estimate_for(Some((100, 40, 40))).await,
        6,
        "only reasoning tokens"
    );
    assert_eq!(estimate_for(None).await, 6, "no usage at all");
}

#[tokio::test]
async fn the_summary_run_emits_one_system_task_usage_event() {
    let c = quiet_three_turns().await;
    let before_usage = c.env.outbox_payloads(USAGE_QUEUE).await.len();
    let before_audit = c.env.outbox_payloads(AUDIT_QUEUE).await.len();
    let quota = quota_rows(&c.env, &c.user).await;
    let turns = turn_row_count(&c.env).await;
    let messages = message_row_count(&c.env).await;

    let sid = run_task_up_to(&c, 3, "S").await;

    let events = system_usage_events(&c.env).await;
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(
        c.env.outbox_payloads(USAGE_QUEUE).await.len(),
        before_usage + 1
    );
    let event = &events[0];
    assert_eq!(event["tenant_id"], json!(c.user.tenant_id));
    assert_eq!(event["chat_id"], json!(c.chat));
    assert_eq!(event["request_id"], json!(sid));
    assert_eq!(event["effective_model"], json!(SUMMARY_MODEL));
    assert_eq!(event["selected_model"], json!(SUMMARY_MODEL));
    assert_eq!(event["terminal_state"], json!("completed"));
    assert_eq!(event["billing_outcome"], json!("system_task"));
    assert_eq!(event["settlement_method"], json!("none"));
    assert_eq!(event["actual_credits_micro"], json!(0));
    assert_eq!(event["requester_type"], json!("system"));
    assert_eq!(event["system_task_type"], json!("thread_summary_update"));
    assert_eq!(event["policy_version_applied"], json!(0));
    assert_eq!(event["web_search_calls"], json!(0));
    assert_eq!(event["code_interpreter_calls"], json!(0));
    assert_eq!(event["file_search_calls"], json!(0));
    assert_eq!(event["usage"]["input_tokens"], json!(120));
    assert_eq!(event["usage"]["output_tokens"], json!(40));
    let _timestamp = parse_ts(&event["timestamp"]);
    assert_eq!(
        event["dedupe_key"],
        json!(format!(
            "{}/thread_summary_update/{}",
            c.user.tenant_id.simple(),
            sid.simple()
        ))
    );
    let object = event.as_object().unwrap();
    assert!(
        !object.contains_key("user_id"),
        "user_id is absent, not null: {event}"
    );
    assert!(
        !object.contains_key("turn_id"),
        "turn_id is absent, not null: {event}"
    );

    // A system task is not a turn: no turn, no message, no quota debit, no audit event.
    assert_eq!(turn_row_count(&c.env).await, turns);
    assert_eq!(message_row_count(&c.env).await, messages);
    assert_eq!(quota_rows(&c.env, &c.user).await, quota);
    assert_eq!(c.env.outbox_payloads(AUDIT_QUEUE).await.len(), before_audit);
}

#[tokio::test]
async fn the_system_task_usage_event_is_published_to_the_policy_plugin() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    let sid = run_task_up_to(&c, 1, "S").await;

    c.env.drain_outbox().await;

    let published = c.env.published_usage();
    let system: Vec<&Value> = published
        .iter()
        .filter(|event| event["billing_outcome"] == json!("system_task"))
        .collect();
    assert_eq!(system.len(), 1, "{published:?}");
    assert_eq!(system[0]["request_id"], json!(sid));
}

#[tokio::test]
async fn a_summary_run_leaves_the_user_quota_alone() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    let before = quota_rows(&c.env, &c.user).await;
    assert!(!before.is_empty());

    run_task_up_to(&c, 1, "S").await;

    assert_eq!(quota_rows(&c.env, &c.user).await, before);
    assert_eq!(total_reserved(&c.env).await, 0);
}

#[tokio::test]
async fn a_second_summary_updates_the_row_and_extends_the_compressed_range() {
    let c = quiet_three_turns().await;
    run_task_up_to(&c, 1, "FIRST").await;
    let first = summary_row(&c.env, &c.chat).await.unwrap();
    enqueue_summary(&c.env, "SECOND");
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 3).await).await;

    enqueue_task(&c.env, &c.user, &c.chat, Some(&first.frontier), &target).await;
    assert_eq!(wait_outcomes(&c.env, 2).await, outcomes(&["ok", "ok"]));

    assert_eq!(summary_row_count(&c.env).await, 1, "one summary per chat");
    let row = summary_row(&c.env, &c.chat).await.unwrap();
    assert_eq!(row.text.trim(), "SECOND");
    assert_eq!(row.frontier.message_id, target.message_id);
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 4).await
    );
    let prompts = summary_requests(&c.env);
    assert_eq!(prompts.len(), 2);
    let second_prompt = user_prompt(prompts[1].body_json.as_ref().unwrap());
    assert!(second_prompt.contains("<existing_summary>") && second_prompt.contains("FIRST"));
    assert_eq!(system_usage_events(&c.env).await.len(), 2);
}

// ===========================================================================
// Handler: failures, retries and dead letters
// ===========================================================================

#[tokio::test]
async fn a_provider_error_keeps_the_previous_summary_and_marks_nothing() {
    let env = Setup::new()
        .worker(json!({ "max_attempts": 1 }))
        .build()
        .await;
    let c = conversation_in(
        env,
        &[
            ("Q-ONE", "A-ONE"),
            ("Q-TWO", "A-TWO"),
            ("Q-THREE", "A-THREE"),
        ],
    )
    .await;
    let base = seed_summary(&c.env, &c.user, &c.chat, "OLD-SUMMARY", 1, 10).await;
    enqueue_summary_failure(&c.env, 500);
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 3).await).await;

    enqueue_task(&c.env, &c.user, &c.chat, Some(&base), &target).await;
    wait_outcomes(&c.env, 1).await;

    let row = summary_row(&c.env, &c.chat).await.unwrap();
    assert_eq!(row.text, "OLD-SUMMARY");
    assert_eq!(
        row.frontier.message_id, base.message_id,
        "the frontier did not move"
    );
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 2).await
    );
    assert!(
        system_usage_events(&c.env).await.is_empty(),
        "a failed call emits no usage event"
    );
}

#[tokio::test]
async fn a_provider_error_is_retried_and_the_retry_keeps_the_system_request_id() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE"), ("Q-TWO", "A-TWO")]).await;
    enqueue_summary_failure(&c.env, 500);
    enqueue_summary(&c.env, "RECOVERED");
    let sid = enqueue_task_up_to(&c.env, &c.user, &c.chat, 3).await;

    assert_eq!(wait_outcomes(&c.env, 2).await, outcomes(&["retry", "ok"]));

    assert_eq!(
        summary_requests(&c.env).len(),
        2,
        "one provider call per attempt"
    );
    assert_eq!(
        summary_row(&c.env, &c.chat).await.unwrap().text.trim(),
        "RECOVERED"
    );
    let usage = system_usage_events(&c.env).await;
    assert_eq!(usage.len(), 1, "only the successful attempt is billed");
    assert_eq!(usage[0]["request_id"], json!(sid));
    assert_eq!(
        usage[0]["dedupe_key"],
        json!(format!(
            "{}/thread_summary_update/{}",
            c.user.tenant_id.simple(),
            sid.simple()
        ))
    );
}

#[tokio::test]
async fn a_persistent_provider_error_is_rejected_after_max_attempts() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    for _ in 0..3 {
        enqueue_summary_failure(&c.env, 500);
    }
    let sid = enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(
        wait_outcomes(&c.env, 3).await,
        outcomes(&["retry", "retry", "reject"])
    );

    assert_eq!(summary_row(&c.env, &c.chat).await.map(|row| row.text), None);
    assert!(compressed_ids(&c.env, &c.chat).await.is_empty());
    let letters = dead_letters(&c.env, SUMMARY_QUEUE).await;
    assert_eq!(letters.len(), 1);
    let payload: Value = serde_json::from_slice(&letters[0].payload).unwrap();
    assert_eq!(
        payload["system_request_id"],
        json!(sid),
        "the dead letter is the same work item"
    );
    assert_eq!(summary_requests(&c.env).len(), 3);
}

#[tokio::test]
async fn max_attempts_is_configurable() {
    let env = Setup::new()
        .worker(json!({ "max_attempts": 2 }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    for _ in 0..2 {
        enqueue_summary_failure(&c.env, 500);
    }
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(
        wait_outcomes(&c.env, 2).await,
        outcomes(&["retry", "reject"])
    );
}

#[tokio::test]
async fn a_rejected_summary_task_does_not_block_the_next_task_of_the_partition() {
    let c = quiet_conversation(&[("Q-ONE", "A-ONE")]).await;
    let other = new_chat(&c.env, &c.user).await;
    complete_turn(&c.env, &c.user, &other, "Q-OTHER", "A-OTHER").await;
    for _ in 0..3 {
        enqueue_summary_failure(&c.env, 500);
    }
    enqueue_summary(&c.env, "OTHER-SUMMARY");
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;
    // Same partition as the failing chat: only the order of the queue separates the two.
    let target = frontier_of(&c.env, message_at(&c.env, &other, 1).await).await;
    let payload = task_payload(&c.user, &other, Uuid::new_v4(), None, &target);
    c.env
        .enqueue_raw_outbox(
            SUMMARY_QUEUE,
            Uuid::parse_str(&c.chat).unwrap(),
            payload.to_string().as_bytes(),
        )
        .await;

    assert_eq!(
        wait_outcomes(&c.env, 4).await,
        outcomes(&["retry", "retry", "reject", "ok"])
    );

    assert_eq!(
        summary_row(&c.env, &other).await.unwrap().text.trim(),
        "OTHER-SUMMARY"
    );
}

#[tokio::test]
async fn a_missing_summary_model_rejects_the_task_without_a_retry_or_a_provider_call() {
    let env = Setup::new()
        .worker(json!({ "summary_model_id": "no-such-model" }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    let sid = enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["reject"]));

    assert!(
        summary_requests(&c.env).is_empty(),
        "the model is resolved before the call"
    );
    assert_eq!(
        c.env.provider().chat_requests().len(),
        1,
        "only the turn reached the provider"
    );
    assert_eq!(summary_row(&c.env, &c.chat).await.map(|row| row.text), None);
    let letters = dead_letters(&c.env, SUMMARY_QUEUE).await;
    assert_eq!(letters.len(), 1);
    let payload: Value = serde_json::from_slice(&letters[0].payload).unwrap();
    assert_eq!(payload["system_request_id"], json!(sid));
}

#[tokio::test]
async fn a_disabled_summary_model_rejects_the_task() {
    let env = Setup::new()
        .summary_entry(json!({ "enabled": false }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["reject"]));

    assert!(summary_requests(&c.env).is_empty());
    assert_eq!(dead_letters(&c.env, SUMMARY_QUEUE).await.len(), 1);
}

#[tokio::test]
async fn the_default_summary_model_must_be_in_the_catalog() {
    // An empty `summary_model_id` means `gpt-4.1-mini`, which this catalog does not contain.
    let env = Setup::new()
        .worker(json!({ "summary_model_id": "" }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(wait_outcomes(&c.env, 1).await, outcomes(&["reject"]));
}

#[tokio::test]
async fn an_empty_summary_task_is_rejected_after_max_attempts() {
    let env = Setup::new()
        .worker(json!({ "max_attempts": 2 }))
        .build()
        .await;
    let c = conversation_in(env, &[("Q-ONE", "A-ONE")]).await;
    enqueue_summary_raw(&c.env, "  ", 10, 0, 0);
    enqueue_summary_raw(&c.env, "", 10, 0, 0);
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 1).await;

    assert_eq!(
        wait_outcomes(&c.env, 2).await,
        outcomes(&["retry", "reject"])
    );

    assert_eq!(summary_row(&c.env, &c.chat).await.map(|row| row.text), None);
}

// ===========================================================================
// Handler: compare-and-set on the frontier
// ===========================================================================

#[tokio::test]
async fn a_task_whose_base_is_stale_does_not_commit_again() {
    let c = quiet_three_turns().await;
    // The frontier moved to A-ONE after the task (base: no summary) was created.
    seed_summary(&c.env, &c.user, &c.chat, "OLD-SUMMARY", 1, 10).await;
    let usage_before = c.env.outbox_payloads(USAGE_QUEUE).await.len();
    enqueue_summary(&c.env, "STALE-RESULT");

    enqueue_task_up_to(&c.env, &c.user, &c.chat, 3).await;

    assert_eq!(
        wait_outcomes(&c.env, 1).await,
        outcomes(&["ok"]),
        "the attempt finishes without a commit"
    );
    let row = summary_row(&c.env, &c.chat).await.unwrap();
    assert_eq!(
        row.text, "OLD-SUMMARY",
        "no second commit for the lost range"
    );
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 2).await
    );
    assert_eq!(
        c.env.outbox_payloads(USAGE_QUEUE).await.len(),
        usage_before,
        "no usage event"
    );
}

#[tokio::test]
async fn two_tasks_for_the_same_frozen_range_commit_once() {
    let c = quiet_three_turns().await;
    enqueue_summary(&c.env, "FIRST");
    enqueue_summary(&c.env, "SECOND");
    let first = enqueue_task_up_to(&c.env, &c.user, &c.chat, 3).await;
    enqueue_task_up_to(&c.env, &c.user, &c.chat, 3).await;

    assert_eq!(wait_outcomes(&c.env, 2).await, outcomes(&["ok", "ok"]));

    assert_eq!(summary_row_count(&c.env).await, 1);
    assert_eq!(
        summary_row(&c.env, &c.chat).await.unwrap().text.trim(),
        "FIRST"
    );
    let usage = system_usage_events(&c.env).await;
    assert_eq!(
        usage.len(),
        1,
        "at most one successful commit per frozen range"
    );
    assert_eq!(usage[0]["request_id"], json!(first));
    assert_eq!(
        compressed_ids(&c.env, &c.chat).await,
        first_messages(&c.env, &c.chat, 4).await
    );
}

#[tokio::test]
async fn a_task_whose_base_summary_is_gone_does_not_recreate_a_summary() {
    let c = quiet_three_turns().await;
    let base = seed_summary(&c.env, &c.user, &c.chat, "OLD-SUMMARY", 1, 10).await;
    // A mutation dropped the summary and cleared the flags after the task was created.
    exec_sql(&c.env, "DELETE FROM thread_summaries").await;
    exec_sql(&c.env, "UPDATE messages SET is_compressed = 0").await;
    enqueue_summary(&c.env, "ORPHAN-RESULT");
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 3).await).await;

    enqueue_task(&c.env, &c.user, &c.chat, Some(&base), &target).await;
    wait_outcomes(&c.env, 1).await;

    assert_eq!(summary_row(&c.env, &c.chat).await.map(|row| row.text), None);
    assert!(compressed_ids(&c.env, &c.chat).await.is_empty());
    assert!(system_usage_events(&c.env).await.is_empty());
}

#[tokio::test]
async fn a_soft_deleted_target_message_skips_the_commit_and_returns_ok() {
    let c = quiet_three_turns().await;
    let target = frontier_of(&c.env, message_at(&c.env, &c.chat, 3).await).await;
    // A mutation removed the frontier message between the enqueue and the commit.
    exec_sql(
        &c.env,
        &format!(
            "UPDATE messages SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(id) = '{}'",
            uuid_hex(target.message_id)
        ),
    )
    .await;
    enqueue_summary(&c.env, "NEVER-STORED");

    enqueue_task(&c.env, &c.user, &c.chat, None, &target).await;

    assert_eq!(
        wait_outcomes(&c.env, 1).await,
        outcomes(&["ok"]),
        "Ok, not a retry"
    );
    assert_eq!(
        summary_row(&c.env, &c.chat).await.map(|row| row.text),
        None,
        "no summary"
    );
    assert!(
        compressed_ids(&c.env, &c.chat).await.is_empty(),
        "no is_compressed change"
    );
    assert!(
        system_usage_events(&c.env).await.is_empty(),
        "no usage event"
    );
    assert!(dead_letters(&c.env, SUMMARY_QUEUE).await.is_empty());
}

#[tokio::test]
async fn retrying_the_turn_that_triggered_the_summary_keeps_the_summary_of_the_earlier_turns() {
    // End to end: the summary covers the turns before the causing turn only.
    let t = triggered("SUMMARY-ONE").await;
    assert_eq!(wait_outcomes(&t.c.env, 1).await, outcomes(&["ok"]));
    let causing = stream_request_id(&t.events);
    enqueue_text(&t.c.env, &["A-THREE-AGAIN"], 10, 5);

    let resp = retry(&t.c.env, &t.c.user, &t.c.chat, causing).await;

    let events = expect_stream(&resp);
    assert_eq!(streamed_text(&events), "A-THREE-AGAIN");
    assert!(
        summary_row(&t.c.env, &t.c.chat).await.is_some(),
        "the summary only covers earlier turns"
    );
}
