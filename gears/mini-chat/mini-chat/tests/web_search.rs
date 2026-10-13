#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Web search: tool events and citations, the per-user daily quota, the per-message call limit, the
//! `disable_web_search` kill switch, the system prompt guard and the reserve surcharge (DESIGN "Web
//! Search Configuration", "Web Search Quota Enforcement (P1 Determinism Rules)", B.6, 5.4.1).
//!
//! The harness default catalog has web search on the premium model and not on the standard one; the
//! tests that need other combinations build a catalog with `common::catalog_entry` (web search on
//! every model) and switch it off per model.

mod common;

use common::{
    AUDIT_QUEUE, PREMIUM_MODEL, STANDARD_MODEL, SYSTEM_PROMPT, Seed, USAGE_QUEUE, WEB_SEARCH_GUARD,
    assert_failed_precondition, assert_nothing_happened, assert_quota_exhausted, catalog_entry,
    chat_on, chat_request_body, closing_frames, delta_frames, done_of, enqueue_chat, env_user_chat,
    env_with_policy, estimated_credits, expect_stream, first, frame, get, merge, named, names,
    new_chat, only_turn, opening_frames, policy, quota_rows, scalar_i64, seed_quota, send,
    send_text, system_prompt_of, terminal, turn_path, web_search_frames,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

fn web_body(content: &str) -> Value {
    json!({ "content": content, "web_search": { "enabled": true } })
}

/// Premium with web search, standard without (the harness default combination, with the
/// `catalog_entry` provider model ids).
fn premium_only_web_catalog() -> Vec<Value> {
    let mut standard = catalog_entry(STANDARD_MODEL, "Standard", true, false);
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    vec![
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        standard,
    ]
}

fn tools_of(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn has_web_search_tool(body: &Value) -> bool {
    tools_of(body).iter().any(|tool| {
        tool["type"]
            .as_str()
            .is_some_and(|kind| kind.starts_with("web_search"))
    })
}

async fn web_calls(env: &TestEnv, period: &str, bucket: &str) -> i64 {
    scalar_i64(
        env,
        &format!(
            "SELECT COALESCE(SUM(web_search_calls), 0) AS n FROM quota_usage \
             WHERE period_type = '{period}' AND bucket = '{bucket}'"
        ),
    )
    .await
}

// ===========================================================================
// A web search turn: tool events, citations and counters
// ===========================================================================

#[tokio::test]
async fn a_web_search_turn_reports_the_tool_then_the_answer_then_web_citations() {
    let (env, user, chat) = env_user_chat().await;
    let annotation = json!({
        "type": "url_citation", "start_index": 0, "end_index": 4,
        "url": "https://example.com/rust", "title": "Rust Language"
    });
    let mut frames = opening_frames();
    frames.push(frame(
        "response.web_search_call.searching",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
    ));
    frames.push(frame(
        "response.web_search_call.completed",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
    ));
    frames.extend(delta_frames(&["Rust is fast."]));
    frames.extend(closing_frames(
        "Rust is fast.",
        &json!([annotation]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send(&env, &user, &chat, web_body("rust")).await);

    assert_eq!(
        names(&events),
        [
            "stream_started",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ],
        "{events:?}"
    );
    let tools = named(&events, "tool");
    assert_eq!(
        tools[0].data,
        json!({ "phase": "start", "name": "web_search", "details": {} })
    );
    assert_eq!(
        tools[1].data,
        json!({ "phase": "done", "name": "web_search", "details": {} })
    );
    let items = first(&events, "citations").data["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["source"], json!("web"));
    assert_eq!(items[0]["url"], json!("https://example.com/rust"));
    assert_eq!(items[0]["title"], json!("Rust Language"));
    assert!(has_web_search_tool(&chat_request_body(&env, 0)));
}

#[tokio::test]
async fn completed_searches_are_counted_on_the_turn_the_quota_the_usage_and_the_audit_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 2, Some((10, 5)))),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(done_of(&events).event, "done");
    assert_eq!(
        scalar_i64(
            &env,
            "SELECT web_search_completed_count AS n FROM chat_turns"
        )
        .await,
        2
    );
    for period in ["daily", "monthly"] {
        assert_eq!(web_calls(&env, period, "total").await, 2, "{period}");
        assert_eq!(
            web_calls(&env, period, "tier:premium").await,
            0,
            "{period}: tool counters live in the total bucket only"
        );
    }
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["web_search_calls"],
        json!(2)
    );
    assert_eq!(
        env.outbox_payloads(AUDIT_QUEUE).await[0]["tool_calls"]["web_search_calls"],
        json!(2)
    );
}

#[tokio::test]
async fn the_counters_accumulate_across_turns_and_stay_per_user() {
    let (env, user, chat) = env_user_chat().await;
    let other = TestUser::in_tenant(user.tenant_id);
    let other_chat = new_chat(&env, &other).await;
    for _ in 0..2 {
        enqueue_chat(
            &env,
            ScriptedResponse::Sse(web_search_frames(1, 1, Some((10, 5)))),
        );
        expect_stream(&send(&env, &user, &chat, web_body("search")).await);
    }
    expect_stream(&send_text(&env, &other, &other_chat, "no search").await);

    let mine = quota_rows(&env, &user).await;
    let theirs = quota_rows(&env, &other).await;

    assert_eq!(
        scalar_i64(
            &env,
            "SELECT SUM(web_search_calls) AS n FROM quota_usage WHERE bucket = 'total' \
             AND period_type = 'daily'"
        )
        .await,
        2
    );
    assert_eq!(common::expect_quota_row(&mine, "daily", "total").calls, 2);
    assert_eq!(common::expect_quota_row(&theirs, "daily", "total").calls, 1);
}

#[tokio::test]
async fn a_turn_without_web_search_counts_no_searches() {
    let (env, user, chat) = env_user_chat().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(web_calls(&env, "daily", "total").await, 0);
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["web_search_calls"],
        json!(0)
    );
}

// ===========================================================================
// Daily quota (quota.web_search_daily_quota, default 75)
// ===========================================================================

async fn quota_env(daily_quota: u32) -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "web_search_daily_quota": daily_quota } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

#[tokio::test]
async fn an_exhausted_daily_quota_is_429_resource_exhausted_with_the_web_search_subject() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(3)).await;

    let resp = send(&env, &user, &chat, web_body("search")).await;

    let problem = assert_quota_exhausted(&resp, "web_search");
    assert_eq!(problem["status"], json!(429));
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_default_daily_quota_is_75_calls() {
    let (env, user, chat) = common::env_user_chat().await;
    seed_quota(&env, &user, "daily", "total", Seed::web(74)).await;
    let below = send(&env, &user, &chat, web_body("one more")).await;
    assert_eq!(below.status, http::StatusCode::OK, "74 < 75");
    seed_quota(&env, &user, "daily", "total", Seed::web(75)).await;

    let at_limit = send(&env, &user, &chat, web_body("too many")).await;

    assert_quota_exhausted(&at_limit, "web_search");
}

#[tokio::test]
async fn one_call_below_the_daily_quota_is_still_accepted() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(2)).await;

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(done_of(&events).event, "done");
}

#[tokio::test]
async fn the_daily_quota_never_rejects_a_request_that_does_not_use_the_tool() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(3)).await;

    let plain = send_text(&env, &user, &chat, "no tool").await;
    let disabled = send(
        &env,
        &user,
        &chat,
        json!({ "content": "explicitly off", "web_search": { "enabled": false } }),
    )
    .await;

    assert_eq!(plain.status, http::StatusCode::OK);
    assert_eq!(disabled.status, http::StatusCode::OK);
}

#[tokio::test]
async fn the_daily_quota_does_not_apply_when_the_model_cannot_search() {
    // The harness standard model has no web search: the tool, its guard and the quota check are
    // skipped and the turn proceeds without web search.
    let (env, user, _chat) = quota_env(3).await;
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(3)).await;

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(done_of(&events).event, "done");
    let body = chat_request_body(&env, 0);
    assert!(!has_web_search_tool(&body), "{body}");
}

#[tokio::test]
async fn the_quota_is_checked_against_the_effective_model_after_a_downgrade() {
    let env = env_with_policy(merge(
        policy(premium_only_web_catalog()),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(75)).await;

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let done = done_of(&events);
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert!(!has_web_search_tool(&chat_request_body(&env, 0)));
}

#[tokio::test]
async fn only_the_daily_total_row_counts_towards_the_quota() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "monthly", "total", Seed::web(100)).await;
    seed_quota(&env, &user, "daily", "tier:premium", Seed::web(100)).await;

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(done_of(&events).event, "done");
}

#[tokio::test]
async fn a_daily_quota_rejection_leaves_the_chat_usable() {
    let (env, user, chat) = quota_env(1).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(1)).await;
    let rejected = send(&env, &user, &chat, web_body("search")).await;
    assert_eq!(rejected.status, http::StatusCode::TOO_MANY_REQUESTS);

    let plain = send_text(&env, &user, &chat, "plain").await;

    assert_eq!(plain.status, http::StatusCode::OK);
}

// ===========================================================================
// Per-message hard limit (quota.web_search_max_calls_per_message, default 2)
// ===========================================================================

async fn limit_env(max_calls: u32) -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "web_search_max_calls_per_message": max_calls } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    (env, user, chat)
}

#[tokio::test]
async fn as_many_searches_as_the_limit_are_allowed() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(1, 1, Some((10, 5)))),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(done_of(&events).event, "done");
}

#[tokio::test]
async fn the_default_limit_is_two_searches_per_message() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 2, Some((10, 5)))),
    );
    let two = expect_stream(&send(&env, &user, &chat, web_body("two")).await);
    assert_eq!(done_of(&two).event, "done");
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(3, 3, Some((10, 5)))),
    );

    let three = expect_stream(&send(&env, &user, &chat, web_body("three")).await);

    assert_eq!(
        terminal(&three).data["code"],
        json!("web_search_calls_exceeded")
    );
}

#[tokio::test]
async fn one_search_more_than_the_limit_fails_the_turn_with_the_error_code() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 1, Some((10, 5)))),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let error = terminal(&events);
    assert_eq!(error.event, "error", "{events:?}");
    assert_eq!(error.data["code"], json!("web_search_calls_exceeded"));
    assert!(error.data["message"].is_string(), "{error:?}");
    assert!(named(&events, "done").is_empty());
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert!(turn.has_completed_at);
}

#[tokio::test]
async fn the_limit_counts_started_searches_not_completed_ones() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 0, Some((10, 5)))),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    assert_eq!(
        terminal(&events).data["code"],
        json!("web_search_calls_exceeded"),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_exceeded_limit_settles_the_estimated_charge_and_emits_a_failed_estimated_usage_event() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 1, Some((10, 5)))),
    );
    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let turn = only_turn(&env, &chat).await;
    let charged = estimated_credits(&turn, 1_000_000, 3_000_000);
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let row = common::expect_quota_row(&rows, period, bucket);
        assert_eq!(
            row.spent, charged,
            "{period}/{bucket}: the estimated formula"
        );
        assert_eq!(
            row.reserved, 0,
            "{period}/{bucket}: no refund, the reserve is released"
        );
        assert_eq!(row.calls, 1, "{period}/{bucket}");
    }
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["terminal_state"], json!("failed"));
    assert_eq!(usage[0]["billing_outcome"], json!("failed"));
    assert_eq!(
        usage[0]["settlement_method"],
        json!("estimated"),
        "the stream is cancelled before usage arrives"
    );
    assert_eq!(usage[0]["usage"], Value::Null);
    assert_eq!(usage[0]["actual_credits_micro"], json!(charged));
    assert_eq!(
        usage[0]["web_search_calls"],
        json!(1),
        "the completed search"
    );
    let audit = env.outbox_payloads(AUDIT_QUEUE).await;
    assert_eq!(audit[0]["event_type"], json!("turn_failed"));
    assert_eq!(audit[0]["error_code"], json!("web_search_calls_exceeded"));
}

#[tokio::test]
async fn the_failed_turn_is_an_error_in_the_turn_status_and_the_chat_stays_usable() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(web_search_frames(2, 1, Some((10, 5)))),
    );
    let events = expect_stream(&send(&env, &user, &chat, web_body("search")).await);
    let request_id = common::stream_request_id(&events);

    let status = get(&env, &user, &turn_path(&chat, &request_id.to_string())).await;
    let again = send_text(&env, &user, &chat, "plain follow-up").await;

    assert_eq!(status.json()["state"], json!("error"));
    assert_eq!(
        status.json()["error_code"],
        json!("web_search_calls_exceeded")
    );
    assert_eq!(again.status, http::StatusCode::OK);
    assert_eq!(common::message_rows(&env, &chat).await.len(), 3);
}

// ===========================================================================
// Kill switch disable_web_search
// ===========================================================================

#[tokio::test]
async fn the_kill_switch_rejects_an_enabled_request_with_feature_disabled_before_any_stream() {
    let env = common::env_with_kill_switches(json!({ "disable_web_search": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send(&env, &user, &chat, web_body("search")).await;

    let problem = assert_failed_precondition(&resp, "web_search", "FEATURE_DISABLED");
    assert_eq!(problem["status"], json!(400));
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_kill_switch_rejects_even_when_the_model_could_not_search() {
    let env = env_with_policy(merge(
        policy(premium_only_web_catalog()),
        json!({ "kill_switches": { "disable_web_search": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    let resp = send(&env, &user, &chat, web_body("search")).await;

    assert_failed_precondition(&resp, "web_search", "FEATURE_DISABLED");
    assert_nothing_happened(&env).await;
}

// ===========================================================================
// The guard text of the system prompt
// ===========================================================================

#[tokio::test]
async fn the_web_search_guard_is_appended_to_the_system_prompt_when_the_tool_is_sent() {
    let (env, user, chat) = env_user_chat().await;

    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let body = chat_request_body(&env, 0);
    assert!(has_web_search_tool(&body), "{body}");
    let prompt = system_prompt_of(&body);
    assert!(prompt.contains(SYSTEM_PROMPT), "{prompt}");
    assert!(prompt.contains(WEB_SEARCH_GUARD), "{prompt}");
    assert!(
        prompt.find(SYSTEM_PROMPT).unwrap() < prompt.find(WEB_SEARCH_GUARD).unwrap(),
        "the guard is appended after the model's system prompt: {prompt}"
    );
}

#[tokio::test]
async fn there_is_no_guard_without_web_search() {
    let (env, user, chat) = env_user_chat().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let prompt = system_prompt_of(&chat_request_body(&env, 0));
    assert!(!prompt.contains("web_search"), "{prompt}");
}

#[tokio::test]
async fn there_is_no_guard_when_the_effective_model_leaves_the_tool_out() {
    let (env, user, _chat) = env_user_chat().await;
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;

    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let body = chat_request_body(&env, 0);
    assert!(!has_web_search_tool(&body));
    assert!(!system_prompt_of(&body).contains("web_search"), "{body}");
}

#[tokio::test]
async fn the_guard_belongs_to_the_turn_that_sends_the_tool() {
    let (env, user, chat) = env_user_chat().await;
    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    expect_stream(&send_text(&env, &user, &chat, "plain follow-up").await);

    assert!(system_prompt_of(&chat_request_body(&env, 0)).contains(WEB_SEARCH_GUARD));
    assert!(!system_prompt_of(&chat_request_body(&env, 1)).contains("web_search"));
}

#[tokio::test]
async fn the_guard_text_is_configurable() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "context": { "web_search_guard": "Search the web sparingly." } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let prompt = system_prompt_of(&chat_request_body(&env, 0));
    assert!(prompt.contains("Search the web sparingly."), "{prompt}");
    assert!(!prompt.contains(WEB_SEARCH_GUARD), "{prompt}");
}

// ===========================================================================
// The reserve surcharge
// ===========================================================================

#[tokio::test]
async fn the_catalog_surcharge_is_reserved_only_when_the_tool_is_sent() {
    let mut premium = catalog_entry(PREMIUM_MODEL, "Premium", true, true);
    premium["estimation_budgets"]["web_search_surcharge_tokens"] = json!(700);
    let env = env_with_policy(policy(vec![
        premium,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let with_tool = new_chat(&env, &user).await;
    let without_tool = new_chat(&env, &user).await;

    expect_stream(&send(&env, &user, &with_tool, web_body("search")).await);
    expect_stream(&send_text(&env, &user, &without_tool, "search").await);

    let estimate = common::estimated_text_tokens(6);
    let with = only_turn(&env, &with_tool).await;
    let without = only_turn(&env, &without_tool).await;
    assert_eq!(with.reserve_tokens, Some(estimate + 700 + 4096));
    assert_eq!(without.reserve_tokens, Some(estimate + 4096));
    assert_eq!(
        with.reserved_credits_micro,
        Some(common::credits_micro(
            estimate + 700,
            4096,
            1_000_000,
            3_000_000
        ))
    );
}

#[tokio::test]
async fn no_surcharge_is_reserved_after_a_downgrade_to_a_model_without_web_search() {
    let env = env_with_policy(merge(
        policy(premium_only_web_catalog()),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    expect_stream(&send(&env, &user, &chat, web_body("search")).await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserve_tokens,
        Some(common::estimated_text_tokens(6) + 4096),
        "the effective model cannot search: no surcharge"
    );
}

// ===========================================================================
// The requested flag
// ===========================================================================

#[tokio::test]
async fn the_requested_flag_is_stored_on_the_turn_even_when_the_tool_is_left_out() {
    let (env, user, _chat) = env_user_chat().await;
    let standard = chat_on(&env, &user, STANDARD_MODEL).await;
    let premium = new_chat(&env, &user).await;

    expect_stream(&send(&env, &user, &standard, web_body("search")).await);
    expect_stream(&send(&env, &user, &premium, web_body("search")).await);
    let plain = chat_on(&env, &user, PREMIUM_MODEL).await;
    expect_stream(&send_text(&env, &user, &plain, "plain").await);

    let flag = |chat: &str| {
        let sql = format!(
            "SELECT web_search_enabled AS n FROM chat_turns WHERE hex(chat_id) = '{}'",
            common::uuid_hex(uuid::Uuid::parse_str(chat).unwrap())
        );
        let env = &env;
        async move { scalar_i64(env, &sql).await }
    };
    assert_eq!(flag(&standard).await, 1);
    assert_eq!(flag(&premium).await, 1);
    assert_eq!(flag(&plain).await, 0);
}
