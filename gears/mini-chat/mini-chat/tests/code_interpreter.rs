#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Code interpreter: when the tool is sent, the per-message call limit, the per-user daily quota, the
//! kill switch, the reserve surcharge and the counters (DESIGN "Code Interpreter Configuration",
//! "Code Interpreter Quota Enforcement (P1 Determinism Rules)", B.6.1, 5.4.1).
//!
//! There is no request flag: the tool is sent when the effective model supports it, the
//! `disable_code_interpreter` kill switch is off and the chat has at least one ready attachment with
//! `for_code_interpreter = 1` (an XLSX). The upload API belongs to the attachments slice; the tests
//! insert the `attachments` rows with SQL.

mod common;

use common::{
    AUDIT_QUEUE, AttachmentSpec, PREMIUM_MODEL, STANDARD_MODEL, Seed, USAGE_QUEUE,
    XLSX_CONTENT_TYPE, assert_nothing_happened, assert_quota_exhausted, catalog_entry, chat_on,
    chat_request_body, code_interpreter_frames, done_of, enqueue_chat, env_user_chat,
    env_with_kill_switches, env_with_policy, estimated_credits, exec_sql, expect_quota_row,
    expect_stream, get, insert_attachment_row, insert_ready_xlsx, merge, named, new_chat,
    only_turn, policy, quota_rows, scalar_i64, seed_quota, send_text, terminal, turn_path,
    uuid_hex,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};

fn tools_of(body: &Value) -> Vec<Value> {
    body.get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn code_interpreter_tool(body: &Value) -> Option<Value> {
    tools_of(body)
        .into_iter()
        .find(|tool| tool["type"] == json!("code_interpreter"))
}

fn file_ids(tool: &Value) -> Vec<String> {
    let mut ids: Vec<String> = tool["container"]["file_ids"]
        .as_array()
        .unwrap_or_else(|| panic!("container.file_ids is an array: {tool}"))
        .iter()
        .map(|id| id.as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    ids
}

async fn code_calls(env: &TestEnv, period: &str, bucket: &str) -> i64 {
    scalar_i64(
        env,
        &format!(
            "SELECT COALESCE(SUM(code_interpreter_calls), 0) AS n FROM quota_usage \
             WHERE period_type = '{period}' AND bucket = '{bucket}'"
        ),
    )
    .await
}

/// A ready XLSX that is not routed to the code interpreter: the chat holds the same document, so any
/// document related reserve term is the same as for a routed one and only the tool differs.
async fn insert_unrouted_xlsx(env: &TestEnv, user: &TestUser, chat: &str, provider_file_id: &str) {
    insert_attachment_row(
        env,
        user,
        chat,
        AttachmentSpec {
            kind: "document",
            status: "ready",
            provider_file_id,
            filename: "data.xlsx",
            content_type: XLSX_CONTENT_TYPE,
            for_file_search: false,
            for_code_interpreter: false,
        },
    )
    .await;
}

/// The standard model of the harness default catalog has no code interpreter support.
async fn env_with_xlsx(provider_file_id: &str) -> (TestEnv, TestUser, String) {
    let (env, user, chat) = env_user_chat().await;
    insert_ready_xlsx(&env, &user, &chat, provider_file_id).await;
    (env, user, chat)
}

// ===========================================================================
// When the tool is sent
// ===========================================================================

#[tokio::test]
async fn a_chat_with_a_ready_xlsx_sends_the_code_interpreter_tool_with_the_file() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000001").await;

    let events = expect_stream(&send_text(&env, &user, &chat, "sum column A").await);

    assert_eq!(done_of(&events).event, "done");
    let body = chat_request_body(&env, 0);
    let tool = code_interpreter_tool(&body)
        .unwrap_or_else(|| panic!("a code_interpreter tool is expected: {body}"));
    assert_eq!(file_ids(&tool), ["file-xlsx0000000001"]);
    assert_eq!(
        body["include"],
        json!(["code_interpreter_call.outputs"]),
        "the call outputs are requested"
    );
}

#[tokio::test]
async fn every_ready_code_interpreter_file_of_the_chat_is_offered() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000002").await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000003").await;

    expect_stream(&send_text(&env, &user, &chat, "sum").await);

    let tool = code_interpreter_tool(&chat_request_body(&env, 0)).unwrap();
    assert_eq!(
        file_ids(&tool),
        ["file-xlsx0000000002", "file-xlsx0000000003"]
    );
}

#[tokio::test]
async fn the_tool_is_sent_on_every_turn_of_the_chat_while_the_file_is_ready() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000004").await;

    expect_stream(&send_text(&env, &user, &chat, "one").await);
    expect_stream(&send_text(&env, &user, &chat, "two").await);

    for index in 0..2 {
        assert!(code_interpreter_tool(&chat_request_body(&env, index)).is_some());
    }
}

#[tokio::test]
async fn without_a_ready_xlsx_no_tool_and_no_include_are_sent() {
    let (env, user, chat) = env_user_chat().await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let body = chat_request_body(&env, 0);
    assert!(code_interpreter_tool(&body).is_none(), "{body}");
    assert!(body.get("include").is_none(), "{body}");
}

#[tokio::test]
async fn attachments_that_are_not_ready_routed_or_live_do_not_enable_the_tool() {
    let (env, user, chat) = env_user_chat().await;
    for (status, for_code_interpreter, file) in [
        ("pending", true, "file-notready00000001"),
        ("uploaded", true, "file-notready00000002"),
        ("failed", true, "file-notready00000003"),
        ("ready", false, "file-notrouted0000001"),
    ] {
        insert_attachment_row(
            &env,
            &user,
            &chat,
            AttachmentSpec {
                kind: "document",
                status,
                provider_file_id: file,
                filename: "data.xlsx",
                content_type: XLSX_CONTENT_TYPE,
                for_file_search: false,
                for_code_interpreter,
            },
        )
        .await;
    }
    let deleted = insert_ready_xlsx(&env, &user, &chat, "file-deleted00000001").await;
    exec_sql(
        &env,
        &format!(
            "UPDATE attachments SET deleted_at = '2000-01-01T00:00:00Z' WHERE hex(id) = '{}'",
            uuid_hex(deleted)
        ),
    )
    .await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(code_interpreter_tool(&chat_request_body(&env, 0)).is_none());
}

#[tokio::test]
async fn a_file_of_another_chat_is_not_offered() {
    let (env, user, chat) = env_user_chat().await;
    let other_chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &other_chat, "file-otherchat0000001").await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(code_interpreter_tool(&chat_request_body(&env, 0)).is_none());
}

#[tokio::test]
async fn a_model_without_code_interpreter_support_gets_no_tool_and_no_surcharge() {
    let (env, user, _chat) = env_user_chat().await;
    let chat = chat_on(&env, &user, STANDARD_MODEL).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000005").await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(done_of(&events).event, "done");
    let body = chat_request_body(&env, 0);
    assert!(code_interpreter_tool(&body).is_none(), "{body}");
    assert!(body.get("include").is_none(), "{body}");
    assert_eq!(
        only_turn(&env, &chat).await.reserve_tokens,
        Some(common::estimated_text_tokens(5) + 4096)
    );
}

#[tokio::test]
async fn the_kill_switch_silently_omits_the_tool_instead_of_rejecting_the_stream() {
    let env = env_with_kill_switches(json!({ "disable_code_interpreter": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000006").await;
    let control = new_chat(&env, &user).await;
    insert_unrouted_xlsx(&env, &user, &control, "file-xlsx0000000106").await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);
    expect_stream(&send_text(&env, &user, &control, "hello").await);

    assert_eq!(done_of(&events).event, "done", "no 400 for this switch");
    let body = chat_request_body(&env, 0);
    assert!(code_interpreter_tool(&body).is_none(), "{body}");
    assert!(body.get("include").is_none(), "{body}");
    assert_eq!(
        only_turn(&env, &chat).await.reserve_tokens,
        only_turn(&env, &control).await.reserve_tokens,
        "no surcharge for an omitted tool"
    );
}

#[tokio::test]
async fn after_a_downgrade_to_a_model_without_the_tool_it_is_omitted() {
    let mut standard = catalog_entry(STANDARD_MODEL, "Standard", true, false);
    standard["general_config"]["tool_support"]["code_interpreter"] = json!(false);
    let env = env_with_policy(merge(
        policy(vec![
            catalog_entry(PREMIUM_MODEL, "Premium", true, true),
            standard,
        ]),
        json!({ "kill_switches": { "force_standard_tier": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000007").await;
    seed_quota(&env, &user, "daily", "total", Seed::code(50)).await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = done_of(&events);
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert!(code_interpreter_tool(&chat_request_body(&env, 0)).is_none());
}

// ===========================================================================
// Reserve surcharge
// ===========================================================================

#[tokio::test]
async fn the_catalog_surcharge_is_reserved_when_the_tool_is_sent() {
    let (env, user, routed) = env_with_xlsx("file-xlsx0000000008").await;
    let unrouted = new_chat(&env, &user).await;
    insert_unrouted_xlsx(&env, &user, &unrouted, "file-xlsx0000000108").await;
    expect_stream(&send_text(&env, &user, &routed, "hello").await);
    expect_stream(&send_text(&env, &user, &unrouted, "hello").await);

    let with_tool = only_turn(&env, &routed).await;
    let without_tool = only_turn(&env, &unrouted).await;

    assert_eq!(
        with_tool.reserve_tokens.unwrap() - without_tool.reserve_tokens.unwrap(),
        1000,
        "code_interpreter_surcharge_tokens = 1000 in the catalog"
    );
    assert!(with_tool.reserved_credits_micro > without_tool.reserved_credits_micro);
}

// ===========================================================================
// Counters
// ===========================================================================

#[tokio::test]
async fn completed_calls_are_counted_on_the_turn_the_quota_and_the_usage_event() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000009").await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(2, Some((10, 5)))),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    assert_eq!(done_of(&events).event, "done");
    assert_eq!(
        scalar_i64(
            &env,
            "SELECT code_interpreter_completed_count AS n FROM chat_turns"
        )
        .await,
        2
    );
    for period in ["daily", "monthly"] {
        assert_eq!(code_calls(&env, period, "total").await, 2, "{period}");
        assert_eq!(
            code_calls(&env, period, "tier:premium").await,
            0,
            "{period}: tool counters live in the total bucket only"
        );
    }
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["code_interpreter_calls"],
        json!(2)
    );
}

// ===========================================================================
// Per-message limit (quota.code_interpreter_max_calls_per_message, default 10)
// ===========================================================================

async fn limit_env(max_calls: u32) -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "code_interpreter_max_calls_per_message": max_calls } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000010").await;
    (env, user, chat)
}

#[tokio::test]
async fn as_many_calls_as_the_limit_are_allowed() {
    let (env, user, chat) = limit_env(2).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(2, Some((10, 5)))),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    assert_eq!(done_of(&events).event, "done");
}

#[tokio::test]
async fn one_call_more_than_the_limit_fails_the_turn_with_its_own_error_code() {
    let (env, user, chat) = limit_env(2).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(3, Some((10, 5)))),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    let error = terminal(&events);
    assert_eq!(error.event, "error", "{events:?}");
    assert_eq!(
        error.data["code"],
        json!("code_interpreter_calls_exceeded"),
        "not quota_exceeded"
    );
    assert!(named(&events, "done").is_empty());
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
    assert!(turn.has_completed_at);
}

#[tokio::test]
async fn the_default_limit_is_ten_calls_per_message() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000011").await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(10, Some((10, 5)))),
    );
    let ten = expect_stream(&send_text(&env, &user, &chat, "ten").await);
    assert_eq!(done_of(&ten).event, "done");
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(11, Some((10, 5)))),
    );

    let eleven = expect_stream(&send_text(&env, &user, &chat, "eleven").await);

    assert_eq!(
        terminal(&eleven).data["code"],
        json!("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn an_exceeded_limit_settles_estimated_and_emits_a_failed_estimated_usage_event() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(2, Some((10, 5)))),
    );
    expect_stream(&send_text(&env, &user, &chat, "compute").await);

    let turn = only_turn(&env, &chat).await;
    let charged = estimated_credits(&turn, 1_000_000, 3_000_000);
    let rows = quota_rows(&env, &user).await;
    for (period, bucket) in [
        ("daily", "total"),
        ("monthly", "total"),
        ("daily", "tier:premium"),
        ("monthly", "tier:premium"),
    ] {
        let row = expect_quota_row(&rows, period, bucket);
        assert_eq!(row.spent, charged, "{period}/{bucket}");
        assert_eq!(
            row.reserved, 0,
            "{period}/{bucket}: no refund of the surcharge"
        );
    }
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["billing_outcome"], json!("failed"));
    assert_eq!(usage[0]["settlement_method"], json!("estimated"));
    assert_eq!(usage[0]["usage"], Value::Null);
    assert_eq!(usage[0]["actual_credits_micro"], json!(charged));
    let audit = env.outbox_payloads(AUDIT_QUEUE).await;
    assert_eq!(audit[0]["event_type"], json!("turn_failed"));
    assert_eq!(
        audit[0]["error_code"],
        json!("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn the_turn_status_shows_the_error_and_the_chat_stays_usable() {
    let (env, user, chat) = limit_env(1).await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(2, Some((10, 5)))),
    );
    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);
    let request_id = common::stream_request_id(&events);

    let status = get(&env, &user, &turn_path(&chat, &request_id.to_string())).await;
    let again = send_text(&env, &user, &chat, "plain").await;

    assert_eq!(status.json()["state"], json!("error"));
    assert_eq!(
        status.json()["error_code"],
        json!("code_interpreter_calls_exceeded")
    );
    assert_eq!(again.status, http::StatusCode::OK);
}

// ===========================================================================
// Daily quota (quota.code_interpreter_daily_quota, default 50)
// ===========================================================================

async fn quota_env(daily_quota: u32) -> (TestEnv, TestUser, String) {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "code_interpreter_daily_quota": daily_quota } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000012").await;
    (env, user, chat)
}

#[tokio::test]
async fn an_exhausted_daily_quota_is_429_resource_exhausted_with_the_code_interpreter_subject() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::code(3)).await;

    let resp = send_text(&env, &user, &chat, "compute").await;

    let problem = assert_quota_exhausted(&resp, "code_interpreter");
    assert_eq!(problem["status"], json!(429));
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_default_daily_quota_is_50_calls() {
    let (env, user, chat) = env_with_xlsx("file-xlsx0000000013").await;
    seed_quota(&env, &user, "daily", "total", Seed::code(49)).await;
    let below = send_text(&env, &user, &chat, "one more").await;
    assert_eq!(below.status, http::StatusCode::OK, "49 < 50");
    seed_quota(&env, &user, "daily", "total", Seed::code(50)).await;

    let at_limit = send_text(&env, &user, &chat, "too many").await;

    assert_quota_exhausted(&at_limit, "code_interpreter");
}

#[tokio::test]
async fn the_quota_applies_only_when_the_tool_is_included() {
    let (env, user, _chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::code(3)).await;
    let without_file = new_chat(&env, &user).await;
    let standard = chat_on(&env, &user, STANDARD_MODEL).await;
    insert_ready_xlsx(&env, &user, &standard, "file-xlsx0000000014").await;

    let no_file = send_text(&env, &user, &without_file, "no file").await;
    let unsupported = send_text(&env, &user, &standard, "model cannot").await;

    assert_eq!(no_file.status, http::StatusCode::OK);
    assert_eq!(
        unsupported.status,
        http::StatusCode::OK,
        "the standard model has no code interpreter"
    );
}

#[tokio::test]
async fn the_kill_switch_skips_the_daily_quota_check() {
    let env = env_with_kill_switches(json!({ "disable_code_interpreter": true })).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_ready_xlsx(&env, &user, &chat, "file-xlsx0000000015").await;
    seed_quota(&env, &user, "daily", "total", Seed::code(50)).await;

    let resp = send_text(&env, &user, &chat, "compute").await;

    assert_eq!(resp.status, http::StatusCode::OK);
}

#[tokio::test]
async fn only_the_daily_total_row_counts_towards_the_quota() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "monthly", "total", Seed::code(100)).await;
    seed_quota(&env, &user, "daily", "tier:premium", Seed::code(100)).await;

    let resp = send_text(&env, &user, &chat, "compute").await;

    assert_eq!(resp.status, http::StatusCode::OK);
}

#[tokio::test]
async fn the_web_search_quota_does_not_block_the_code_interpreter() {
    let (env, user, chat) = quota_env(3).await;
    seed_quota(&env, &user, "daily", "total", Seed::web(75)).await;

    let resp = send_text(&env, &user, &chat, "compute").await;

    assert_eq!(resp.status, http::StatusCode::OK);
}

#[tokio::test]
async fn a_rejection_by_the_daily_quota_leaves_nothing_behind() {
    let (env, user, chat) = quota_env(1).await;
    seed_quota(&env, &user, "daily", "total", Seed::code(1)).await;

    let resp = send_text(&env, &user, &chat, "compute").await;

    assert_eq!(resp.status, http::StatusCode::TOO_MANY_REQUESTS);
    assert_nothing_happened(&env).await;
}
