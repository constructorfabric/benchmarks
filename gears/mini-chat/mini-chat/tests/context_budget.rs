#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The context window budget: `INPUT_TOO_LONG` for the input message, `CONTEXT_BUDGET_EXCEEDED`
//! for the full assembled request, and oldest-whole-turns-first history truncation (DESIGN
//! sections 2.2 "Context Window Budget", 3.4 "Context Plan Assembly and Truncation", 5.5).
//!
//! Estimates follow DESIGN 5.5.4 with the `catalog_entry` budgets (4 bytes per token, 100 fixed
//! overhead tokens, 10 % margin): an N-byte message costs `ceil((ceil(N / 4) + 100) * 1.1)` tokens;
//! the token budget is `min(max_input_tokens, context_window - max_output_tokens_applied)` minus
//! the tool surcharges (web search 500) minus the 100 overhead tokens.

mod common;

use common::{
    PREMIUM_MODEL, STANDARD_MODEL, assert_nothing_happened, assert_out_of_range, assert_problem,
    catalog_entry, chat_request_body, complete_turn, content_type, env_two_models_with_config,
    env_with_policy, estimated_text_tokens, expect_stream, input_text, merge, new_chat, policy,
    send, send_text, system_prompt_of, terminal, tiny_limits,
};
use mini_chat::test_support::{TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

fn premium_with(patch: Value) -> Value {
    merge(catalog_entry(PREMIUM_MODEL, "Premium", true, true), patch)
}

fn standard_with(patch: Value) -> Value {
    merge(
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
        patch,
    )
}

async fn env_user_chat_with(entries: Vec<Value>) -> (TestEnv, TestUser, String) {
    let env = env_with_policy(policy(entries)).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

/// `max_input_tokens` 2000 on the premium (default) model.
async fn limited_input_env() -> (TestEnv, TestUser, String) {
    env_user_chat_with(vec![
        premium_with(json!({ "max_input_tokens": 2000 })),
        standard_with(json!({})),
    ])
    .await
}

fn text_of(bytes: usize) -> String {
    "x".repeat(bytes)
}

fn marked(marker: &str, bytes: usize) -> String {
    format!("{marker}{}", "x".repeat(bytes - marker.len()))
}

// ===========================================================================
// INPUT_TOO_LONG: the input message against max_input_tokens
// ===========================================================================

#[tokio::test]
async fn a_message_above_max_input_tokens_is_400_input_too_long() {
    let (env, user, chat) = limited_input_env().await;
    assert!(estimated_text_tokens(20_000) > 2000);

    let resp = send_text(&env, &user, &chat, &text_of(20_000)).await;

    assert_out_of_range(&resp, "INPUT_TOO_LONG");
    assert!(content_type(&resp).starts_with("application/problem+json"));
}

#[tokio::test]
async fn input_too_long_is_rejected_before_any_provider_call_and_leaves_nothing_behind() {
    let (env, user, chat) = limited_input_env().await;

    send_text(&env, &user, &chat, &text_of(20_000)).await;

    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_input_too_long_boundary_is_the_estimate_against_max_input_tokens() {
    // 7000 bytes estimate to 2035 tokens (> 2000): INPUT_TOO_LONG.
    let (env, user, chat) = limited_input_env().await;
    assert_eq!(estimated_text_tokens(7000), 2035);

    let resp = send_text(&env, &user, &chat, &text_of(7000)).await;

    assert_out_of_range(&resp, "INPUT_TOO_LONG");
}

#[tokio::test]
async fn a_message_well_below_max_input_tokens_is_accepted() {
    let (env, user, chat) = limited_input_env().await;

    let events = expect_stream(&send_text(&env, &user, &chat, &text_of(3000)).await);

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn a_zero_max_input_tokens_means_no_separate_input_limit() {
    let env = env_two_models_with_config(json!({})).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, &text_of(20_000)).await);

    assert_eq!(
        terminal(&events).event,
        "done",
        "max_input_tokens = 0 disables INPUT_TOO_LONG"
    );
}

#[tokio::test]
async fn the_input_limit_of_the_effective_model_applies_after_a_downgrade() {
    // Premium has no input limit, standard 2000. With the premium bucket exhausted the turn runs
    // on the standard model, whose entry decides.
    let env = env_with_policy(merge(
        policy(vec![
            premium_with(json!({})),
            standard_with(json!({ "max_input_tokens": 2000 })),
        ]),
        json!({ "default_premium_limits": tiny_limits() }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send_text(&env, &user, &chat, &text_of(20_000)).await;

    assert_out_of_range(&resp, "INPUT_TOO_LONG");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_same_message_is_accepted_when_the_premium_model_stays_effective() {
    let env = env_with_policy(policy(vec![
        premium_with(json!({})),
        standard_with(json!({ "max_input_tokens": 2000 })),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, &text_of(20_000)).await);

    assert_eq!(terminal(&events).event, "done");
}

// ===========================================================================
// CONTEXT_BUDGET_EXCEEDED: mandatory items against the full budget
// ===========================================================================

#[tokio::test]
async fn mandatory_items_that_do_not_fit_the_budget_are_400_context_budget_exceeded() {
    // 6800 bytes estimate to 1980 tokens: within max_input_tokens (2000), but the budget is
    // 2000 - 100 overhead = 1900 and the system prompt comes on top.
    let (env, user, chat) = limited_input_env().await;
    assert_eq!(estimated_text_tokens(6800), 1980);

    let resp = send_text(&env, &user, &chat, &text_of(6800)).await;

    assert_out_of_range(&resp, "CONTEXT_BUDGET_EXCEEDED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn context_budget_exceeded_is_a_json_problem_not_an_event_stream() {
    let (env, user, chat) = limited_input_env().await;

    let resp = send_text(&env, &user, &chat, &text_of(6800)).await;

    assert_problem(&resp, 400, Some("out_of_range"));
    assert!(!content_type(&resp).contains("event-stream"));
}

#[tokio::test]
async fn a_max_output_that_fills_the_context_window_leaves_no_room_for_input() {
    // max_output_tokens_applied (4096) >= context_window (4096).
    let (env, user, chat) = env_user_chat_with(vec![
        premium_with(json!({ "context_window": 4096, "max_output_tokens": 4096 })),
        standard_with(json!({})),
    ])
    .await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_out_of_range(&resp, "CONTEXT_BUDGET_EXCEEDED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_streaming_output_cap_can_make_the_same_catalog_entry_fit() {
    // The applied output cap is min(4096, 1000): the window leaves 3096 tokens for input.
    let env = TestEnvBuilder::new()
        .policy_config(policy(vec![
            premium_with(json!({ "context_window": 4096, "max_output_tokens": 4096 })),
            standard_with(json!({})),
        ]))
        .gear_config(json!({ "streaming": { "max_output_tokens": 1000 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn the_budget_of_the_effective_model_applies_after_a_downgrade() {
    let env = env_with_policy(merge(
        policy(vec![
            premium_with(json!({})),
            standard_with(json!({ "context_window": 4096, "max_output_tokens": 4096 })),
        ]),
        json!({ "default_premium_limits": tiny_limits() }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_out_of_range(&resp, "CONTEXT_BUDGET_EXCEEDED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn tool_surcharges_shrink_the_budget_only_when_the_tool_is_in_the_request() {
    // 4800 bytes estimate to 1430 tokens. Budget without tools: 2000 - 100 = 1900 (fits with the
    // system prompt); with web search: 1900 - 500 = 1400 (does not fit).
    let (env, user, chat) = limited_input_env().await;
    assert_eq!(estimated_text_tokens(4800), 1430);
    let content = text_of(4800);

    let plain = expect_stream(&send_text(&env, &user, &chat, &content).await);
    assert_eq!(terminal(&plain).event, "done");

    let other_chat = new_chat(&env, &user).await;
    let with_web_search = send(
        &env,
        &user,
        &other_chat,
        json!({ "content": content, "web_search": { "enabled": true } }),
    )
    .await;
    assert_out_of_range(&with_web_search, "CONTEXT_BUDGET_EXCEEDED");
}

#[tokio::test]
async fn no_surcharge_applies_when_the_model_does_not_support_the_tool() {
    let mut no_web_search = premium_with(json!({ "max_input_tokens": 2000 }));
    no_web_search["general_config"]["tool_support"]["web_search"] = json!(false);
    let (env, user, chat) = env_user_chat_with(vec![no_web_search, standard_with(json!({}))]).await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": text_of(4800), "web_search": { "enabled": true } }),
    )
    .await;

    let events = expect_stream(&resp);
    assert_eq!(
        terminal(&events).event,
        "done",
        "the tool is left out, so is its surcharge"
    );
}

// ===========================================================================
// History truncation: oldest whole turns first
// ===========================================================================

/// Premium window 4000, output cap 1000: input limit 3000, token budget 2900. A 4000-byte message
/// estimates to 1210 tokens, the 28-byte system prompt to 118.
async fn truncation_env() -> (TestEnv, TestUser, String) {
    env_user_chat_with(vec![
        premium_with(json!({ "context_window": 4000, "max_output_tokens": 1000 })),
        standard_with(json!({})),
    ])
    .await
}

async fn three_big_turns(env: &TestEnv, user: &TestUser, chat: &str) {
    for (question, answer) in [
        ("Q-ONE", "A-ONE"),
        ("Q-TWO", "A-TWO"),
        ("Q-THREE", "A-THREE"),
    ] {
        complete_turn(
            env,
            user,
            chat,
            &marked(question, 4000),
            &marked(answer, 4000),
        )
        .await;
    }
}

#[tokio::test]
async fn the_oldest_whole_turns_are_dropped_first_when_the_history_does_not_fit() {
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let body = chat_request_body(&env, 3);
    let text = input_text(&body);
    assert!(
        text.contains("Q-THREE") && text.contains("A-THREE"),
        "the newest turn is kept whole"
    );
    for dropped in ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO"] {
        assert!(
            !text.contains(dropped),
            "`{dropped}` must have been truncated"
        );
    }
    assert!(text.contains("Q-NOW"));
}

#[tokio::test]
async fn the_current_message_stays_last_and_the_system_prompt_stays_when_history_is_truncated() {
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let body = chat_request_body(&env, 3);
    let last = body["input"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .to_string();
    assert!(last.contains("Q-NOW"), "{last}");
    assert!(system_prompt_of(&body).contains(common::SYSTEM_PROMPT));
}

#[tokio::test]
async fn an_answer_is_never_sent_without_its_question() {
    // Turn three: the budget leaves room for one history message, the newest (A-TWO). It would
    // start the kept range with an assistant message, so it is dropped as well.
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;

    let body = chat_request_body(&env, 2);
    let text = input_text(&body);

    assert!(
        !text.contains("A-TWO"),
        "an orphaned answer must be dropped: {text}"
    );
    assert!(!text.contains("A-ONE"));
    assert!(
        text.contains("Q-THREE"),
        "the current message is always sent"
    );
}

#[tokio::test]
async fn the_current_message_is_never_truncated() {
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;
    let big = marked("Q-BIG", 4000);

    common::send_ok(&env, &user, &chat, &big).await;

    let text = input_text(&chat_request_body(&env, 3));
    assert!(
        text.contains(&big),
        "the current user message is sent whole"
    );
}

#[tokio::test]
async fn a_history_that_fits_the_budget_is_sent_complete() {
    let (env, user, chat) = common::env_user_chat().await;
    complete_turn(&env, &user, &chat, "Q-ONE", "A-ONE").await;
    complete_turn(&env, &user, &chat, "Q-TWO", "A-TWO").await;

    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let text = input_text(&chat_request_body(&env, 2));
    for kept in ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO", "Q-NOW"] {
        assert!(
            text.contains(kept),
            "`{kept}` fits and must be sent: {text}"
        );
    }
}

#[tokio::test]
async fn truncation_does_not_remove_messages_from_the_conversation() {
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;
    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let page = common::list_messages_ok(&env, &user, &chat, &[("limit", "100")]).await;

    assert_eq!(
        common::items(&page).len(),
        8,
        "history stays stored and listed"
    );
}

#[tokio::test]
async fn older_history_stays_dropped_as_the_conversation_grows() {
    let (env, user, chat) = truncation_env().await;
    three_big_turns(&env, &user, &chat).await;
    common::send_ok(&env, &user, &chat, "Q-NOW").await;
    common::send_ok(&env, &user, &chat, "Q-NOW").await;

    let first = input_text(&chat_request_body(&env, 3));
    let second = input_text(&chat_request_body(&env, 4));

    // The second request additionally carries the first "Q-NOW" turn; the older turns that were
    // dropped the first time stay dropped.
    for dropped in ["Q-ONE", "A-ONE", "Q-TWO", "A-TWO"] {
        assert!(
            !first.contains(dropped) && !second.contains(dropped),
            "`{dropped}`"
        );
    }
}
