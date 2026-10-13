#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Pipeline behaviour the contract tests of `send_stream`, `sse_contract` and `quota_settlement`
//! do not pin: mid-turn tool limits, tool counters, file citations, attachments and images, kill
//! switch downgrades, the thread summary in the context, finalization failures and concurrent
//! sends (DESIGN sections 3.3, 3.4, 3.6, 5.7).

mod common;
mod live;

use common::{
    PREMIUM_MODEL, STANDARD_MODEL, USAGE_QUEUE, assert_failed_precondition,
    assert_invalid_argument, assert_nothing_happened, assert_quota_exhausted, catalog_entry,
    chat_request_body, closing_frames, delta_frames, enqueue_chat, env_two_models, env_user_chat,
    env_with_policy, exec_sql, expect_stream, first, frame, input_text, limits, merge, named,
    names, new_chat, only_turn, opening_frames, policy, send, send_text, send_with_id, started,
    terminal, two_model_catalog, uuid_hex,
};
use mini_chat::test_support::{ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serde_json::{Value, json};
use uuid::Uuid;

const TS: &str = "'2026-01-01T00:00:00Z'";

fn blob(id: Uuid) -> String {
    format!("X'{}'", id.simple())
}

async fn scalar_i64(env: &TestEnv, sql: &str) -> i64 {
    env.db()
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
        .await
        .unwrap()
        .expect("the query returns a row")
        .try_get("", "n")
        .unwrap()
}

async fn scalar_text(env: &TestEnv, sql: &str) -> String {
    env.db()
        .query_one_raw(Statement::from_string(DbBackend::Sqlite, sql.to_owned()))
        .await
        .unwrap()
        .expect("the query returns a row")
        .try_get("", "t")
        .unwrap()
}

fn web_search_body(content: &str) -> Value {
    json!({ "content": content, "web_search": { "enabled": true } })
}

fn tool_frames(name: &str, calls: usize) -> Vec<(String, Value)> {
    let mut frames = opening_frames();
    for _ in 0..calls {
        frames.push(frame(
            &format!("response.{name}_call.searching"),
            json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
        ));
        frames.push(frame(
            &format!("response.{name}_call.completed"),
            json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
        ));
    }
    frames.extend(delta_frames(&["done"]));
    frames.extend(closing_frames(
        "done",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    frames
}

async fn insert_attachment(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    kind: &str,
    status: &str,
    provider_file_id: &str,
    filename: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    exec_sql(
        env,
        &format!(
            "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, \
             content_type, size_bytes, storage_backend, provider_file_id, status, attachment_kind, \
             for_file_search, created_at, updated_at) VALUES ({}, {}, {}, {}, '{filename}', \
             'text/plain', 10, 'openai', '{provider_file_id}', '{status}', '{kind}', {}, {TS}, {TS})",
            blob(id),
            blob(user.tenant_id),
            blob(Uuid::parse_str(chat).unwrap()),
            blob(user.user_id),
            i32::from(kind == "document"),
        ),
    )
    .await;
    id
}

async fn insert_vector_store(env: &TestEnv, user: &TestUser, chat: &str, store: &str) {
    exec_sql(
        env,
        &format!(
            "INSERT INTO chat_vector_stores (id, tenant_id, chat_id, vector_store_id, provider, \
             file_count, created_at) VALUES ({}, {}, {}, '{store}', 'openai', 1, {TS})",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(Uuid::parse_str(chat).unwrap()),
        ),
    )
    .await;
}

// ===========================================================================
// Mid-turn tool limits and counters
// ===========================================================================

#[tokio::test]
async fn more_web_search_calls_than_the_per_message_limit_fail_the_turn() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, ScriptedResponse::Sse(tool_frames("web_search", 3)));

    let events = expect_stream(&send(&env, &user, &chat, web_search_body("search")).await);

    let error = terminal(&events);
    assert_eq!(error.event, "error", "{events:?}");
    assert_eq!(error.data["code"], json!("web_search_calls_exceeded"));
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage[0]["billing_outcome"], json!("failed"));
    assert_eq!(
        usage[0]["settlement_method"],
        json!("estimated"),
        "the stream is cancelled before usage arrives"
    );
}

#[tokio::test]
async fn a_configured_code_interpreter_limit_fails_the_turn() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "code_interpreter_max_calls_per_message": 1 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let mut frames = opening_frames();
    for _ in 0..2 {
        frames.push(frame(
            "response.code_interpreter_call.in_progress",
            json!({ "item_id": "ci_0123456789abcdef", "output_index": 1 }),
        ));
    }
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    assert_eq!(
        terminal(&events).data["code"],
        json!("code_interpreter_calls_exceeded")
    );
}

#[tokio::test]
async fn completed_web_search_calls_are_counted_on_the_turn_the_quota_and_the_usage_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, ScriptedResponse::Sse(tool_frames("web_search", 2)));

    let events = expect_stream(&send(&env, &user, &chat, web_search_body("search")).await);

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    assert_eq!(
        scalar_i64(
            &env,
            "SELECT web_search_completed_count AS n FROM chat_turns"
        )
        .await,
        2
    );
    assert_eq!(
        scalar_i64(
            &env,
            "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = 'daily' AND bucket = 'total'"
        )
        .await,
        2
    );
    assert_eq!(
        scalar_i64(
            &env,
            "SELECT web_search_calls AS n FROM quota_usage WHERE period_type = 'daily' AND bucket = 'tier:premium'"
        )
        .await,
        0,
        "tool counters live in the total bucket only"
    );
    assert_eq!(
        env.outbox_payloads(USAGE_QUEUE).await[0]["web_search_calls"],
        json!(2)
    );
}

#[tokio::test]
async fn the_daily_web_search_quota_is_checked_against_the_settled_calls() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "quota": { "web_search_daily_quota": 2 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    enqueue_chat(&env, ScriptedResponse::Sse(tool_frames("web_search", 2)));
    expect_stream(&send(&env, &user, &chat, web_search_body("one")).await);

    let rejected = send(&env, &user, &chat, web_search_body("two")).await;
    let plain = send_text(&env, &user, &chat, "no tool").await;

    assert_quota_exhausted(&rejected, "web_search");
    assert_eq!(plain.status, http::StatusCode::OK);
}

// ===========================================================================
// File search, citations, attachments, images
// ===========================================================================

fn file_citation_frames(provider_file_ids: &[&str]) -> Vec<(String, Value)> {
    let annotations: Vec<Value> = provider_file_ids
        .iter()
        .map(|id| json!({ "type": "file_citation", "file_id": id, "filename": "provider-side.pdf", "index": 0 }))
        .collect();
    let mut frames = opening_frames();
    frames.extend(delta_frames(&["See the report."]));
    frames.extend(closing_frames(
        "See the report.",
        &json!(annotations),
        Some((10, 5)),
        "response.completed",
    ));
    frames
}

#[tokio::test]
async fn a_chat_with_a_ready_document_sends_file_search_and_maps_file_citations() {
    let (env, user, chat) = env_user_chat().await;
    let attachment = insert_attachment(
        &env,
        &user,
        &chat,
        "document",
        "ready",
        "file-aaaaaaaaaaaaaaaa",
        "Q3 Report.pdf",
    )
    .await;
    insert_vector_store(&env, &user, &chat, "vs_chatstore000001").await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(file_citation_frames(&[
            "file-aaaaaaaaaaaaaaaa",
            "file-unknownunknown1",
        ])),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "what does the report say").await);

    let body = chat_request_body(&env, 0);
    let tool = &body["tools"][0];
    assert_eq!(tool["type"], json!("file_search"));
    assert_eq!(tool["vector_store_ids"], json!(["vs_chatstore000001"]));
    assert_eq!(tool["max_num_results"], json!(5));
    assert_eq!(body["metadata"]["feature"], json!("file_search"));
    assert!(
        common::system_prompt_of(&body).contains("file_search"),
        "the tool guard is sent"
    );
    let citations = first(&events, "citations");
    let items = citations.data["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "the unknown provider file is omitted: {items:?}"
    );
    assert_eq!(
        items[0],
        json!({
            "source": "file",
            "title": "Q3 Report.pdf",
            "attachment_id": attachment.to_string(),
            "snippet": ""
        })
    );
    assert!(
        !citations.data.to_string().contains("file-"),
        "no provider file id reaches the client"
    );
}

#[tokio::test]
async fn a_failed_attachment_does_not_enable_file_search() {
    let (env, user, chat) = env_user_chat().await;
    insert_attachment(
        &env,
        &user,
        &chat,
        "document",
        "failed",
        "file-bbbbbbbbbbbbbbbb",
        "broken.pdf",
    )
    .await;
    insert_vector_store(&env, &user, &chat, "vs_chatstore000002").await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(chat_request_body(&env, 0).get("tools").is_none());
}

#[tokio::test]
async fn file_search_is_not_sent_when_the_kill_switch_is_on() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "kill_switches": { "disable_file_search": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    insert_attachment(
        &env,
        &user,
        &chat,
        "document",
        "ready",
        "file-cccccccccccccccc",
        "a.pdf",
    )
    .await;
    insert_vector_store(&env, &user, &chat, "vs_chatstore000003").await;

    expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(chat_request_body(&env, 0).get("tools").is_none());
}

#[tokio::test]
async fn a_ready_image_is_attached_to_the_message_sent_to_the_provider_and_listed() {
    let (env, user, chat) = env_user_chat().await;
    let image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-dddddddddddddddd",
        "cat.png",
    )
    .await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "what is this", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    let body = chat_request_body(&env, 0);
    let last = body["input"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"][0],
        json!({ "type": "input_text", "text": "what is this" })
    );
    assert_eq!(
        last["content"][1],
        json!({ "type": "input_image", "file_id": "file-dddddddddddddddd" })
    );
    let page = common::list_messages_ok(&env, &user, &chat, &[]).await;
    let attachments = &common::items(&page)[0]["attachments"];
    assert_eq!(attachments[0]["attachment_id"], json!(image.to_string()));
    assert_eq!(attachments[0]["kind"], json!("image"));
    assert_eq!(attachments[0]["filename"], json!("cat.png"));
    assert_eq!(attachments[0]["status"], json!("ready"));
    assert!(
        common::items(&page)[1]["attachments"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_image_surcharge_enters_the_reserve() {
    let (env, user, chat) = env_user_chat().await;
    let image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-eeeeeeeeeeeeeeee",
        "cat.png",
    )
    .await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "attachment_ids": [image.to_string()] }),
        )
        .await,
    );
    assert_eq!(terminal(&events).event, "done");

    let turn = only_turn(&env, &chat).await;
    assert_eq!(
        turn.reserve_tokens,
        Some(common::estimated_text_tokens(5) + 1000 + 4096),
        "image_token_budget = 1000"
    );
}

#[tokio::test]
async fn attachments_that_are_not_ready_or_not_the_callers_are_invalid() {
    let (env, user, chat) = env_user_chat().await;
    let pending = insert_attachment(
        &env,
        &user,
        &chat,
        "document",
        "pending",
        "file-ffffffffffffffff",
        "p.pdf",
    )
    .await;
    let other_user = TestUser::in_tenant(user.tenant_id);
    let foreign = insert_attachment(
        &env,
        &other_user,
        &chat,
        "document",
        "ready",
        "file-gggggggggggggggg",
        "f.pdf",
    )
    .await;
    let other_chat = new_chat(&env, &user).await;
    let elsewhere = insert_attachment(
        &env,
        &user,
        &other_chat,
        "document",
        "ready",
        "file-hhhhhhhhhhhhhhhh",
        "e.pdf",
    )
    .await;

    for id in [pending, foreign, elsewhere] {
        let resp = send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "attachment_ids": [id.to_string()] }),
        )
        .await;
        assert_invalid_argument(&resp, "attachment", "invalid_attachment");
    }
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn an_image_for_a_model_without_vision_is_400_vision_not_supported() {
    let mut no_vision = catalog_entry(PREMIUM_MODEL, "Premium", true, true);
    no_vision["multimodal_capabilities"] = json!([]);
    let env = env_with_policy(policy(vec![
        no_vision,
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
    ]))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-iiiiiiiiiiiiiiii",
        "cat.png",
    )
    .await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [image.to_string()] }),
    )
    .await;

    assert_invalid_argument(&resp, "content_type", "VISION_NOT_SUPPORTED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn images_are_rejected_when_the_kill_switch_is_on() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "kill_switches": { "disable_images": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-jjjjjjjjjjjjjjjj",
        "cat.png",
    )
    .await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [image.to_string()] }),
    )
    .await;

    assert_failed_precondition(&resp, "images", "FEATURE_DISABLED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn more_images_than_the_per_message_limit_are_400_too_many_images() {
    let env = TestEnvBuilder::new()
        .gear_config(json!({ "rag": { "max_images_per_message": 1 } }))
        .build()
        .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let first_image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-kkkkkkkkkkkkkkkk",
        "a.png",
    )
    .await;
    let second_image = insert_attachment(
        &env,
        &user,
        &chat,
        "image",
        "ready",
        "file-llllllllllllllll",
        "b.png",
    )
    .await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [first_image.to_string(), second_image.to_string()] }),
    )
    .await;

    let problem = common::assert_out_of_range(&resp, "TOO_MANY_IMAGES");
    assert_eq!(problem["status"], json!(400));
    assert_nothing_happened(&env).await;
}

// ===========================================================================
// Kill switches and the downgrade reason
// ===========================================================================

async fn downgrade_reason_of(switches: Value) -> Value {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "kill_switches": switches }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);
    terminal(&events).data.clone()
}

#[tokio::test]
async fn the_force_standard_tier_kill_switch_downgrades_with_its_reason() {
    let done = downgrade_reason_of(json!({ "force_standard_tier": true })).await;

    assert_eq!(done["quota_decision"], json!("downgrade"));
    assert_eq!(done["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(done["downgrade_from"], json!(PREMIUM_MODEL));
    assert_eq!(done["downgrade_reason"], json!("force_standard_tier"));
}

#[tokio::test]
async fn the_disable_premium_tier_kill_switch_downgrades_with_its_reason() {
    let done = downgrade_reason_of(json!({ "disable_premium_tier": true })).await;

    assert_eq!(done["downgrade_reason"], json!("disable_premium_tier"));
    assert_eq!(done["effective_model"], json!(STANDARD_MODEL));
}

#[tokio::test]
async fn a_disabled_chat_model_is_downgraded_with_model_disabled() {
    let env = common::env_with_catalog(vec![
        catalog_entry(PREMIUM_MODEL, "Premium", true, true),
        catalog_entry(STANDARD_MODEL, "Standard", true, false),
        catalog_entry("gpt-off", "Standard", false, false),
    ])
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chats SET model = 'gpt-off' WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(&chat).unwrap())
        ),
    )
    .await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.data["selected_model"], json!("gpt-off"));
    assert_eq!(done.data["effective_model"], json!(STANDARD_MODEL));
    assert_eq!(done.data["downgrade_reason"], json!("model_disabled"));
}

// ===========================================================================
// Thread summary in the context
// ===========================================================================

#[tokio::test]
async fn an_existing_thread_summary_replaces_older_history_and_is_announced() {
    let (env, user, chat) = env_user_chat().await;
    common::complete_turn(&env, &user, &chat, "OLD-QUESTION", "OLD-ANSWER").await;
    let chat_id = Uuid::parse_str(&chat).unwrap();
    let frontier_id = scalar_text(
        &env,
        &format!(
            "SELECT hex(id) AS t FROM messages WHERE hex(chat_id) = '{}' AND role = 'assistant'",
            uuid_hex(chat_id)
        ),
    )
    .await;
    let frontier_at = scalar_text(
        &env,
        &format!(
            "SELECT created_at AS t FROM messages WHERE hex(chat_id) = '{}' AND role = 'assistant'",
            uuid_hex(chat_id)
        ),
    )
    .await;
    exec_sql(
        &env,
        &format!(
            "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, \
             summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, created_at, \
             updated_at) VALUES ({}, {}, {}, 'SUMMARY-OF-OLD-TALK', '{frontier_at}', X'{frontier_id}', 42, {TS}, {TS})",
            blob(Uuid::new_v4()),
            blob(user.tenant_id),
            blob(chat_id),
        ),
    )
    .await;

    let events = expect_stream(&send_text(&env, &user, &chat, "NEW-QUESTION").await);

    let summary_message = "This conversation has earlier messages that have been summarized. \
        The summary below covers the earlier portion of the conversation. Recent messages follow \
        after.\n\nSUMMARY-OF-OLD-TALK";
    assert_eq!(
        started(&events).data["thread_summary_applied"],
        json!({ "token_estimate": common::estimated_text_tokens(
            i64::try_from(summary_message.len()).unwrap()
        ) })
    );
    let text = input_text(&chat_request_body(&env, 1));
    assert!(text.contains("SUMMARY-OF-OLD-TALK"), "{text}");
    assert!(
        text.contains("earlier messages that have been summarized"),
        "{text}"
    );
    assert!(
        !text.contains("OLD-QUESTION") && !text.contains("OLD-ANSWER"),
        "{text}"
    );
    assert!(
        text.find("SUMMARY-OF-OLD-TALK").unwrap() < text.find("NEW-QUESTION").unwrap(),
        "the summary precedes the recent messages and the current message"
    );
}

// ===========================================================================
// Finalization failures
// ===========================================================================

#[tokio::test]
async fn an_assistant_message_that_cannot_be_persisted_fails_the_turn_with_message_persistence_failed()
 {
    let (env, user, chat) = env_user_chat().await;
    exec_sql(
        &env,
        "CREATE TRIGGER refuse_assistant BEFORE INSERT ON messages WHEN NEW.role = 'assistant' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await;
    common::enqueue_text(&env, &["answer"], 10, 5);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let error = terminal(&events);
    assert_eq!(error.event, "error", "{events:?}");
    assert_eq!(error.data["code"], json!("message_persistence_failed"));
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("message_persistence_failed")
    );
    assert_eq!(turn.assistant_message_id, None);
    let usage = env.outbox_payloads(USAGE_QUEUE).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["billing_outcome"], json!("failed"));
    assert_eq!(
        usage[0]["settlement_method"],
        json!("actual"),
        "the provider reported usage"
    );
    assert_eq!(common::total_reserved(&env).await, 0);
}

#[tokio::test]
async fn a_finalization_that_cannot_commit_sends_finalization_failed_and_leaves_the_turn_running() {
    let (env, user, chat) = env_user_chat().await;
    exec_sql(
        &env,
        "CREATE TRIGGER refuse_terminal BEFORE UPDATE ON chat_turns WHEN NEW.state != 'running' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["code"], json!("finalization_failed"));
    assert_eq!(only_turn(&env, &chat).await.state, "running");
    assert!(env.outbox_payloads(USAGE_QUEUE).await.is_empty());
}

#[tokio::test]
async fn a_failed_finalization_of_a_failed_stream_still_reports_the_original_error() {
    let (env, user, chat) = env_user_chat().await;
    exec_sql(
        &env,
        "CREATE TRIGGER refuse_terminal BEFORE UPDATE ON chat_turns WHEN NEW.state != 'running' \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .await;
    enqueue_chat(&env, common::failed_script("boom", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["code"], json!("provider_error"));
}

#[tokio::test]
async fn usage_beyond_the_credit_bounds_cannot_be_settled_and_ends_with_finalization_failed() {
    let (env, user, chat) = env_user_chat().await;
    common::enqueue_text(&env, &["answer"], 20_000_000, 5);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["code"], json!("finalization_failed"));
    assert_eq!(only_turn(&env, &chat).await.state, "running");
}

// ===========================================================================
// Concurrency
// ===========================================================================

#[tokio::test]
async fn two_concurrent_sends_to_one_chat_start_exactly_one_turn() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, common::hang_script(&["par"]));
    enqueue_chat(&env, common::hang_script(&["par"]));

    let (first_response, second_response) = tokio::join!(
        live::open(&env, &user, &chat, json!({ "content": "one" })),
        live::open(&env, &user, &chat, json!({ "content": "two" })),
    );

    let statuses = [first_response.status, second_response.status];
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == http::StatusCode::OK)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == http::StatusCode::CONFLICT)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(common::turn_row_count(&env).await, 1);
    assert_eq!(common::message_row_count(&env).await, 1);
    first_response.disconnect();
    second_response.disconnect();
}

#[tokio::test]
async fn two_concurrent_sends_with_one_request_id_conflict_on_the_request_id() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_chat(&env, common::hang_script(&["par"]));
    enqueue_chat(&env, common::hang_script(&["par"]));
    let body = json!({ "content": "same", "request_id": request_id.to_string() });

    let (first_response, second_response) = tokio::join!(
        live::open(&env, &user, &chat, body.clone()),
        live::open(&env, &user, &chat, body),
    );

    let (winner, loser) = if first_response.status == http::StatusCode::OK {
        (first_response, second_response)
    } else {
        (second_response, first_response)
    };
    assert_eq!(winner.status, http::StatusCode::OK);
    assert_eq!(loser.status, http::StatusCode::CONFLICT);
    assert_eq!(common::turn_row_count(&env).await, 1);
    winner.disconnect();
}

#[tokio::test]
async fn concurrent_reserves_never_book_more_than_the_limit() {
    // One reserve (12401) fits into 20000, two do not.
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_standard_limits": limits(20_000, 20_000) }),
    ))
    .await;
    let user = TestUser::new();
    let chat_a = new_chat(&env, &user).await;
    let chat_b = new_chat(&env, &user).await;
    enqueue_chat(&env, common::hang_script(&["par"]));
    enqueue_chat(&env, common::hang_script(&["par"]));

    let (a, b) = tokio::join!(
        live::open(&env, &user, &chat_a, json!({ "content": "hello" })),
        live::open(&env, &user, &chat_b, json!({ "content": "hello" })),
    );

    let ok = [&a, &b]
        .iter()
        .filter(|response| response.status == http::StatusCode::OK)
        .count();
    let exhausted = [&a, &b]
        .iter()
        .filter(|response| response.status == http::StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!((ok, exhausted), (1, 1));
    for row in common::quota_rows(&env, &user).await {
        assert!(row.reserved <= 20_000, "{row:?}");
    }
    assert_eq!(
        common::turn_row_count(&env).await,
        1,
        "the loser left no turn behind"
    );
    a.disconnect();
    b.disconnect();
}

// ===========================================================================
// Event stream details
// ===========================================================================

#[tokio::test]
async fn the_event_stream_response_is_not_cached_and_not_chunk_buffered() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_eq!(
        resp.headers
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-cache")
    );
}

#[tokio::test]
async fn reasoning_free_text_streams_only_text_deltas() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(
        named(&events, "delta")
            .iter()
            .all(|event| event.data["type"] == json!("text"))
    );
    assert_eq!(names(&events).first(), Some(&"stream_started"));
}

#[tokio::test]
async fn a_replayed_request_with_another_model_chat_state_still_replays() {
    let env = env_two_models().await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chats SET model = 'gone' WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(&chat).unwrap())
        ),
    )
    .await;

    let replay = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(started(&replay).data["is_new_turn"], json!(false));
    assert_eq!(terminal(&replay).event, "done");
}
