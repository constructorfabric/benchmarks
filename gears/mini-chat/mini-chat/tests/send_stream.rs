#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `POST /v1/chats/{id}/messages:stream`: request correlation, preflight validation, relaying
//! without buffering and persistence of the answer (DESIGN sections 3.3 "Streaming Contract",
//! 3.6 "Send Message with Streaming Response", 5.7 content durability).

mod common;
mod live;

use common::{
    CHAT_RESOURCE_TYPE, PREMIUM_MODEL, STANDARD_MODEL, assert_failed_precondition,
    assert_invalid_argument, assert_not_found, assert_nothing_happened, assert_problem,
    complete_turn, content_type, enqueue_chat, enqueue_text, env_two_models, env_user_chat,
    env_with_policy, exec_sql, expect_status, expect_stream, field_violation_reason, first, frame,
    get_chat, merge, message_rows, names, new_chat, only_turn, opening_frames, parse_ts, policy,
    raw_request, send, send_text, send_with_id, started, stream_message_id, stream_path,
    stream_request_id, streamed_text, terminal, text_script, two_model_catalog, uuid_hex,
};
use http::{Method, StatusCode};
use mini_chat::test_support::{AuthzMode, ScriptedResponse, TestEnv, TestEnvBuilder, TestUser};
use serde_json::json;
use uuid::Uuid;

// ===========================================================================
// Streaming response correlated by request_id
// ===========================================================================

#[tokio::test]
async fn send_message_answers_with_an_event_stream() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    expect_status(&resp, StatusCode::OK);
    assert!(
        content_type(&resp).starts_with("text/event-stream"),
        "Content-Type must be text/event-stream, got `{}`",
        content_type(&resp)
    );
    let events = expect_stream(&resp);
    assert_eq!(names(&events).first(), Some(&"stream_started"));
    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn client_supplied_request_id_is_echoed_in_stream_started() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();

    let events = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(
        started(&events).data["request_id"],
        json!(request_id.to_string()),
        "{events:?}"
    );
}

#[tokio::test]
async fn request_id_of_any_uuid_version_is_accepted() {
    let (env, user, chat) = env_user_chat().await;
    // A version 1 UUID: the server does not check the version.
    let request_id = Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap();

    let events = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(stream_request_id(&events), request_id);
    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn omitted_request_id_is_generated_as_a_uuid_v4() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let request_id = stream_request_id(&events);
    assert_eq!(request_id.get_version_num(), 4, "{request_id}");
}

#[tokio::test]
async fn generated_request_ids_differ_between_turns() {
    let (env, user, chat) = env_user_chat().await;

    let first_turn = complete_turn(&env, &user, &chat, "one", "A").await;
    let second_turn = complete_turn(&env, &user, &chat, "two", "B").await;

    assert_ne!(first_turn.request_id, second_turn.request_id);
}

#[tokio::test]
async fn the_server_generated_request_id_is_the_turn_key() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.request_id, stream_request_id(&events));
}

#[tokio::test]
async fn user_and_assistant_messages_share_the_request_id() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    enqueue_text(&env, &["Hi there"], 10, 5);

    let events = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);
    assert_eq!(terminal(&events).event, "done");

    let rows = message_rows(&env, &chat).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[1].role, "assistant");
    assert_eq!(rows[0].request_id, Some(request_id));
    assert_eq!(rows[1].request_id, Some(request_id));
}

#[tokio::test]
async fn messages_api_exposes_the_shared_request_id() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    send_with_id(&env, &user, &chat, "hello", request_id).await;

    let page = common::list_messages_ok(&env, &user, &chat, &[]).await;

    let items = common::items(&page);
    assert_eq!(items.len(), 2, "{page}");
    for item in items {
        assert_eq!(item["request_id"], json!(request_id.to_string()), "{item}");
    }
}

#[tokio::test]
async fn the_user_message_is_stored_with_the_sent_content() {
    let (env, user, chat) = env_user_chat().await;

    send_text(&env, &user, &chat, "  keep my words  ").await;

    let rows = message_rows(&env, &chat).await;
    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[0].content.trim(), "keep my words");
}

// ===========================================================================
// No buffering: events are relayed while the provider stream is still open
// ===========================================================================

#[tokio::test]
async fn stream_started_and_the_first_delta_arrive_before_the_provider_stream_ends() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();

    // The provider sends `Hel` and then keeps the stream open until the client disconnects.
    let stream = live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["Hel"]).await;

    assert_eq!(
        live::turn_state(&env, &chat, request_id).await,
        "running",
        "the provider stream has not ended, so the turn is still running"
    );
    stream.disconnect();
}

#[tokio::test]
async fn every_delta_is_relayed_as_it_arrives() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();

    let stream =
        live::start_hung_turn(&env, &user, &chat, request_id, "hello", &["a", "b", "c"]).await;

    assert_eq!(live::turn_state(&env, &chat, request_id).await, "running");
    stream.disconnect();
}

#[tokio::test]
async fn tool_events_are_relayed_before_the_provider_stream_ends() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "response.web_search_call.searching",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0, "sequence_number": 5 }),
    ));
    enqueue_chat(&env, ScriptedResponse::SseThenHang(frames));

    let mut stream = live::open(
        &env,
        &user,
        &chat,
        json!({ "content": "search", "web_search": { "enabled": true } }),
    )
    .await;

    let events = live::read_until(&mut stream, "tool").await;
    assert_eq!(events[0].event, "stream_started");
    let tool = events.last().unwrap();
    assert_eq!(tool.data["phase"], json!("start"));
    assert_eq!(tool.data["name"], json!("web_search"));
    stream.disconnect();
}

#[tokio::test]
async fn a_completed_stream_ends_right_after_the_terminal_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["x"], 10, 5);

    let mut stream = live::open(&env, &user, &chat, json!({ "content": "hello" })).await;
    let events = live::read_to_end(&mut stream).await;

    assert_eq!(terminal(&events).event, "done");
    assert!(
        stream.next_event().await.is_none(),
        "the connection must close after the terminal event"
    );
}

#[tokio::test]
async fn done_is_sent_only_after_the_turn_is_committed_as_completed() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["Hello"], 10, 5);
    let mut stream = live::open(&env, &user, &chat, json!({ "content": "hello" })).await;

    let events = live::read_until(&mut stream, "done").await;

    let request_id = stream_request_id(&events);
    let turn = common::turn_row(&env, &chat, request_id).await.unwrap();
    assert_eq!(turn.state, "completed", "done must follow the commit");
    assert!(turn.assistant_message_id.is_some());
}

// ===========================================================================
// Persistence once the stream completes
// ===========================================================================

#[tokio::test]
async fn assistant_message_holds_the_concatenated_deltas_model_and_provider_usage() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, text_script(&["Hel", "lo ", "wor", "ld"], 42, 17));

    let events = expect_stream(&send_text(&env, &user, &chat, "greet me").await);
    assert_eq!(streamed_text(&events), "Hello world");

    let rows = message_rows(&env, &chat).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    let assistant = &rows[1];
    assert_eq!(assistant.role, "assistant");
    assert_eq!(assistant.content, "Hello world");
    assert_eq!(assistant.model.as_deref(), Some(PREMIUM_MODEL));
    assert_eq!(assistant.input_tokens, 42);
    assert_eq!(assistant.output_tokens, 17);
    assert_eq!(assistant.id.to_string(), stream_message_id(&events));
}

#[tokio::test]
async fn user_message_row_carries_no_model_or_usage() {
    let (env, user, chat) = env_user_chat().await;
    send_text(&env, &user, &chat, "hello").await;

    let rows = message_rows(&env, &chat).await;

    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[0].model, None);
    assert_eq!(rows[0].input_tokens, 0);
    assert_eq!(rows[0].output_tokens, 0);
}

#[tokio::test]
async fn completed_turn_row_links_the_assistant_message_and_records_completion() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.error_code, None);
    assert!(turn.has_completed_at, "a terminal turn has completed_at");
    assert_eq!(
        turn.assistant_message_id.map(|id| id.to_string()),
        Some(stream_message_id(&events))
    );
    let assistant = message_rows(&env, &chat).await.remove(1);
    assert_eq!(turn.assistant_message_id, Some(assistant.id));
}

#[tokio::test]
async fn completed_turn_records_the_effective_model_on_the_turn() {
    let (env, user, chat) = env_user_chat().await;
    send_text(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;

    assert_eq!(turn.effective_model.as_deref(), Some(PREMIUM_MODEL));
}

#[tokio::test]
async fn completed_turn_keeps_the_provider_response_id_internal() {
    let (env, user, chat) = env_user_chat().await;
    let resp = send_text(&env, &user, &chat, "hello").await;

    let turn = only_turn(&env, &chat).await;

    assert_eq!(
        turn.provider_response_id.as_deref(),
        Some(common::PROVIDER_RESPONSE_ID),
        "the provider response id is persisted internally"
    );
    assert!(!common::body_text(&resp).contains("resp_"));
}

#[tokio::test]
async fn completed_turn_bumps_chat_updated_at_and_counts_both_messages() {
    let (env, user, chat) = env_user_chat().await;
    let before = get_chat(&env, &user, &chat).await.json();
    assert_eq!(before["message_count"], json!(0));
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    send_text(&env, &user, &chat, "hello").await;

    let after = get_chat(&env, &user, &chat).await.json();
    assert!(
        parse_ts(&after["updated_at"]) > parse_ts(&before["updated_at"]),
        "updated_at must move forward: {before} -> {after}"
    );
    assert_eq!(after["message_count"], json!(2), "{after}");
    assert_eq!(after["created_at"], before["created_at"]);
}

#[tokio::test]
async fn an_answer_without_text_completes_with_an_empty_assistant_message() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, text_script(&[], 10, 0));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).event, "done", "{events:?}");
    assert_eq!(streamed_text(&events), "");
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "completed");
    let rows = message_rows(&env, &chat).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].role, "assistant");
    assert_eq!(rows[1].content, "");
}

#[tokio::test]
async fn each_turn_adds_one_user_and_one_assistant_message() {
    let (env, user, chat) = env_user_chat().await;

    complete_turn(&env, &user, &chat, "one", "A").await;
    complete_turn(&env, &user, &chat, "two", "B").await;

    let rows = message_rows(&env, &chat).await;
    let roles: Vec<&str> = rows.iter().map(|row| row.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["message_count"],
        json!(4)
    );
}

#[tokio::test]
async fn the_standard_model_chat_records_its_own_model_on_the_assistant_message() {
    let env = env_two_models().await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await,
    );

    send_text(&env, &user, &chat, "hello").await;

    let rows = message_rows(&env, &chat).await;
    assert_eq!(rows[1].model.as_deref(), Some(STANDARD_MODEL));
}

// ===========================================================================
// Preflight validation: JSON errors, zero provider calls, nothing left behind
// ===========================================================================

#[tokio::test]
async fn empty_content_is_400_empty_content() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "").await;

    assert_invalid_argument(&resp, "content", "EMPTY_CONTENT");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn whitespace_only_content_is_400_empty_content() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, " \t\r\n  ").await;

    assert_invalid_argument(&resp, "content", "EMPTY_CONTENT");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn missing_content_is_422_invalid_json_body() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send(&env, &user, &chat, json!({})).await;

    let problem = assert_problem(&resp, 422, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("invalid_json_body"),
        "{problem}"
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn non_string_content_is_422_invalid_json_body() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send(&env, &user, &chat, json!({ "content": 42 })).await;

    assert_problem(&resp, 422, Some("invalid_argument"));
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn non_uuid_request_id_is_422_invalid_json_body() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "request_id": "not-a-uuid" }),
    )
    .await;

    let problem = assert_problem(&resp, 422, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("invalid_json_body"),
        "{problem}"
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn non_uuid_attachment_id_is_422_invalid_json_body() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": ["not-a-uuid"] }),
    )
    .await;

    let problem = assert_problem(&resp, 422, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("invalid_json_body"),
        "{problem}"
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn unknown_attachment_id_is_400_invalid_attachment() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [Uuid::new_v4().to_string()] }),
    )
    .await;

    assert_invalid_argument(&resp, "attachment", "invalid_attachment");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn duplicate_attachment_ids_are_400_invalid_attachment() {
    let (env, user, chat) = env_user_chat().await;
    let attachment = Uuid::new_v4().to_string();

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "hello", "attachment_ids": [attachment, attachment] }),
    )
    .await;

    assert_invalid_argument(&resp, "attachment", "invalid_attachment");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn an_empty_attachment_list_is_accepted() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "hello", "attachment_ids": [] }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done");
}

fn kill_switch_policy(switches: serde_json::Value) -> serde_json::Value {
    let mut patch = serde_json::Map::new();
    patch.insert("kill_switches".to_owned(), switches);
    merge(
        policy(two_model_catalog()),
        serde_json::Value::Object(patch),
    )
}

#[tokio::test]
async fn web_search_with_the_kill_switch_is_400_failed_precondition_feature_disabled() {
    let env = env_with_policy(kill_switch_policy(json!({ "disable_web_search": true }))).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "search", "web_search": { "enabled": true } }),
    )
    .await;

    assert_failed_precondition(&resp, "web_search", "FEATURE_DISABLED");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn the_kill_switch_does_not_affect_requests_without_web_search() {
    let env = env_with_policy(kill_switch_policy(json!({ "disable_web_search": true }))).await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let plain = expect_stream(&send_text(&env, &user, &chat, "hello").await);
    let explicit_off = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "again", "web_search": { "enabled": false } }),
        )
        .await,
    );

    assert_eq!(terminal(&plain).event, "done");
    assert_eq!(terminal(&explicit_off).event, "done");
}

#[tokio::test]
async fn web_search_without_the_kill_switch_streams() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn a_chat_model_missing_from_the_catalog_is_400_invalid_model() {
    let (env, user, chat) = env_user_chat().await;
    exec_sql(
        &env,
        &format!(
            "UPDATE chats SET model = 'gpt-removed' WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(&chat).unwrap())
        ),
    )
    .await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_invalid_argument(&resp, "model", "INVALID_MODEL");
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn an_unknown_chat_is_404_naming_the_chat_resource() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    let resp = send_text(&env, &user, &Uuid::new_v4().to_string(), "hello").await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn another_users_chat_is_404_and_never_reaches_the_provider() {
    let (env, owner, chat) = env_user_chat().await;
    let intruder = TestUser::in_tenant(owner.tenant_id);

    let resp = send_text(&env, &intruder, &chat, "hello").await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn another_tenants_chat_is_404_and_never_reaches_the_provider() {
    let (env, _owner, chat) = env_user_chat().await;
    let intruder = TestUser::new();

    let resp = send_text(&env, &intruder, &chat, "hello").await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_deleted_chat_is_404() {
    let (env, user, chat) = env_user_chat().await;
    common::delete_chat(&env, &user, &chat).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_400_invalid_path_params() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    let resp = send_text(&env, &user, "not-a-uuid", "hello").await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        common::has_violation_reason(&problem, "invalid_path_params"),
        "{problem}"
    );
}

#[tokio::test]
async fn malformed_json_is_400_json_syntax_error() {
    let (env, user, chat) = env_user_chat().await;

    let resp = env
        .request_raw(
            &user,
            raw_request(
                Method::POST,
                &stream_path(&chat),
                Some("application/json"),
                "{\"content\": ",
            ),
        )
        .await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("json_syntax_error"),
        "{problem}"
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_body_without_a_json_content_type_is_415() {
    let (env, user, chat) = env_user_chat().await;

    let resp = env
        .request_raw(
            &user,
            raw_request(
                Method::POST,
                &stream_path(&chat),
                Some("text/plain"),
                "{\"content\": \"hello\"}",
            ),
        )
        .await;

    let problem = assert_problem(&resp, 415, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("missing_json_content_type"),
        "{problem}"
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_denied_send_is_403_and_never_reaches_the_provider() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;
    let user = TestUser::new();

    let resp = send_text(&env, &user, &Uuid::new_v4().to_string(), "hello").await;

    assert_problem(&resp, 403, Some("permission_denied"));
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn an_unavailable_pdp_is_503_with_retry_after_and_never_reaches_the_provider() {
    let env = TestEnvBuilder::new()
        .authz(AuthzMode::Unavailable)
        .build()
        .await;
    let user = TestUser::new();

    let resp = send_text(&env, &user, &Uuid::new_v4().to_string(), "hello").await;

    assert_problem(&resp, 503, Some("service_unavailable"));
    assert_eq!(
        resp.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );
    assert_nothing_happened(&env).await;
}

#[tokio::test]
async fn a_rejected_send_leaves_the_chat_untouched() {
    let (env, user, chat) = env_user_chat().await;
    let before = get_chat(&env, &user, &chat).await.json();

    send_text(&env, &user, &chat, "").await;

    let after = get_chat(&env, &user, &chat).await.json();
    assert_eq!(before, after, "a rejected request must not touch the chat");
}

#[tokio::test]
async fn a_rejected_send_can_be_retried_with_the_same_request_id() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();
    let rejected = send_with_id(&env, &user, &chat, "  ", request_id).await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);

    let events = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    assert_eq!(terminal(&events).event, "done");
    assert_eq!(stream_request_id(&events), request_id);
    assert!(
        first(&events, "stream_started").data["is_new_turn"] == json!(true),
        "a rejected request never created a turn"
    );
}
