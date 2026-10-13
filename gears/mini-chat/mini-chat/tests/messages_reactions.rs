#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Messages API (`GET /v1/chats/{id}/messages`) and the reactions API
//! (`PUT|DELETE /v1/chats/{id}/messages/{msg_id}/reaction`): DESIGN sections 3.3 "List
//! Messages" and "Message Reaction API", the REST error mapping and ADR-0004.

mod common;

use common::{
    CHAT_RESOURCE_TYPE, MESSAGE_RESOURCE_TYPE, ODATA_RESOURCE_TYPE, assert_failed_precondition,
    assert_invalid_argument, assert_not_found, assert_problem, complete_turn, enqueue_chat,
    env_user_chat, exec_sql, expect_status, get_chat, has_violation_reason, items, list_messages,
    list_messages_ok, message_contents, message_ids, message_roles, message_rows, new_chat,
    next_cursor, parse_ts, reaction_path, reaction_rows, text_script, uuid_hex,
};
use http::{Method, StatusCode};
use mini_chat::test_support::{AuthzMode, TestEnv, TestEnvBuilder, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

/// Runs `turns` completed turns: `question {i}` answered by `answer {i}`.
async fn chat_with_turns(env: &TestEnv, user: &TestUser, turns: usize) -> String {
    let chat = new_chat(env, user).await;
    for turn in 1..=turns {
        complete_turn(
            env,
            user,
            &chat,
            &format!("question {turn}"),
            &format!("answer {turn}"),
        )
        .await;
    }
    chat
}

fn assert_odata_problem(resp: &mini_chat::test_support::TestResponse, field: &str, reason: &str) {
    let problem = assert_invalid_argument(resp, field, reason);
    assert_eq!(
        problem["context"]["resource_type"].as_str(),
        Some(ODATA_RESOURCE_TYPE),
        "{problem}"
    );
}

// ===========================================================================
// List messages: contract
// ===========================================================================

#[tokio::test]
async fn a_new_chat_has_an_empty_message_page() {
    let (env, user, chat) = env_user_chat().await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert!(items(&page).is_empty(), "{page}");
    assert_eq!(page["page_info"]["limit"], json!(20));
    assert_eq!(next_cursor(&page), None);
}

#[tokio::test]
async fn messages_are_listed_chronologically_by_default() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 3).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(
        message_contents(&page),
        [
            "question 1",
            "answer 1",
            "question 2",
            "answer 2",
            "question 3",
            "answer 3"
        ]
    );
    assert_eq!(
        message_roles(&page),
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
}

#[tokio::test]
async fn the_default_order_equals_an_explicit_created_at_ascending_order() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let default = list_messages_ok(&env, &user, &chat, &[]).await;
    let explicit = list_messages_ok(&env, &user, &chat, &[("$orderby", "created_at asc")]).await;

    assert_eq!(message_ids(&default), message_ids(&explicit));
}

#[tokio::test]
async fn a_user_message_has_the_required_contract_fields_and_no_optional_ones() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let message = &items(&page)[0];
    assert_eq!(message["role"], json!("user"));
    assert_eq!(message["content"], json!("question 1"));
    assert!(
        Uuid::parse_str(message["id"].as_str().unwrap()).is_ok(),
        "{message}"
    );
    assert!(
        Uuid::parse_str(message["request_id"].as_str().unwrap()).is_ok(),
        "{message}"
    );
    assert_eq!(message["attachments"], json!([]), "{message}");
    assert!(
        message.get("my_reaction").is_some_and(Value::is_null),
        "{message}"
    );
    for omitted in ["model", "input_tokens", "output_tokens"] {
        assert!(
            message.get(omitted).is_none(),
            "`{omitted}` is omitted on user messages: {message}"
        );
    }
}

#[tokio::test]
async fn an_assistant_message_carries_model_and_token_counts() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, text_script(&["answer"], 42, 17));
    common::send_ok(&env, &user, &chat, "question").await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let assistant = &items(&page)[1];
    assert_eq!(assistant["role"], json!("assistant"));
    assert_eq!(assistant["model"], json!(common::PREMIUM_MODEL));
    assert_eq!(assistant["input_tokens"], json!(42));
    assert_eq!(assistant["output_tokens"], json!(17));
    assert_eq!(assistant["attachments"], json!([]));
    assert!(
        assistant.get("my_reaction").is_some_and(Value::is_null),
        "{assistant}"
    );
}

#[tokio::test]
async fn zero_token_counts_are_omitted() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, text_script(&["answer"], 0, 0));
    common::send_ok(&env, &user, &chat, "question").await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let assistant = &items(&page)[1];
    assert!(assistant.get("input_tokens").is_none(), "{assistant}");
    assert!(assistant.get("output_tokens").is_none(), "{assistant}");
    assert_eq!(assistant["model"], json!(common::PREMIUM_MODEL));
}

#[tokio::test]
async fn request_id_attachments_and_my_reaction_are_present_on_every_message() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(items(&page).len(), 4);
    for message in items(&page) {
        assert!(
            message["request_id"].is_string(),
            "request_id is never null: {message}"
        );
        assert!(
            message["attachments"].is_array(),
            "attachments is always an array: {message}"
        );
        assert!(
            message.get("my_reaction").is_some(),
            "my_reaction is always present: {message}"
        );
    }
}

#[tokio::test]
async fn user_and_assistant_messages_of_a_turn_share_the_request_id_in_the_list() {
    let (env, user, chat) = env_user_chat().await;
    let first_turn = complete_turn(&env, &user, &chat, "question 1", "answer 1").await;
    let second_turn = complete_turn(&env, &user, &chat, "question 2", "answer 2").await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let request_ids: Vec<&str> = items(&page)
        .iter()
        .map(|m| m["request_id"].as_str().unwrap())
        .collect();
    let first_id = first_turn.request_id.to_string();
    let second_id = second_turn.request_id.to_string();
    let (first_id, second_id) = (first_id.as_str(), second_id.as_str());
    assert_eq!(request_ids, [first_id, first_id, second_id, second_id]);
}

#[tokio::test]
async fn the_assistant_message_id_matches_the_one_announced_by_the_stream() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "question", "answer").await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(items(&page)[1]["id"], json!(turn.message_id));
}

#[tokio::test]
async fn message_count_on_the_chat_detail_tracks_the_turns() {
    let (env, user, chat) = env_user_chat().await;
    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["message_count"],
        json!(0)
    );

    complete_turn(&env, &user, &chat, "question 1", "answer 1").await;
    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["message_count"],
        json!(2)
    );

    complete_turn(&env, &user, &chat, "question 2", "answer 2").await;
    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["message_count"],
        json!(4)
    );
}

#[tokio::test]
async fn message_count_is_reported_per_chat() {
    let (env, user, chat) = env_user_chat().await;
    let other = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "question", "answer").await;

    assert_eq!(
        get_chat(&env, &user, &chat).await.json()["message_count"],
        json!(2)
    );
    assert_eq!(
        get_chat(&env, &user, &other).await.json()["message_count"],
        json!(0)
    );
}

#[tokio::test]
async fn the_list_holds_only_the_messages_of_the_requested_chat() {
    let (env, user, chat) = env_user_chat().await;
    let other = new_chat(&env, &user).await;
    complete_turn(&env, &user, &chat, "mine", "answer").await;
    complete_turn(&env, &user, &other, "theirs", "answer").await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(message_contents(&page), ["mine", "answer"]);
}

#[tokio::test]
async fn the_list_matches_the_stored_messages() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    let stored: Vec<String> = message_rows(&env, &chat)
        .await
        .into_iter()
        .map(|row| row.id.to_string())
        .collect();
    assert_eq!(message_ids(&page), stored);
}

// ===========================================================================
// List messages: pagination
// ===========================================================================

#[tokio::test]
async fn the_default_page_size_is_20() {
    let (env, user, chat) = env_user_chat().await;

    let page = list_messages_ok(&env, &user, &chat, &[]).await;

    assert_eq!(page["page_info"]["limit"], json!(20));
}

#[tokio::test]
async fn a_limit_above_100_is_clamped_to_100() {
    let (env, user, chat) = env_user_chat().await;

    let page = list_messages_ok(&env, &user, &chat, &[("limit", "1000")]).await;

    assert_eq!(page["page_info"]["limit"], json!(100));
}

#[tokio::test]
async fn a_limit_of_zero_is_400_invalid_limit() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("limit", "0")]).await;

    assert_odata_problem(&resp, "$top", "INVALID_LIMIT");
}

#[tokio::test]
async fn an_explicit_limit_bounds_the_page_and_offers_a_next_cursor() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 3).await;

    let page = list_messages_ok(&env, &user, &chat, &[("limit", "4")]).await;

    assert_eq!(items(&page).len(), 4);
    assert_eq!(page["page_info"]["limit"], json!(4));
    assert!(next_cursor(&page).is_some(), "{page}");
}

#[tokio::test]
async fn the_last_page_has_no_next_cursor() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;

    let page = list_messages_ok(&env, &user, &chat, &[("limit", "5")]).await;

    assert_eq!(items(&page).len(), 2);
    assert_eq!(next_cursor(&page), None, "{page}");
}

#[tokio::test]
async fn cursor_pagination_visits_every_message_once_in_chronological_order() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 3).await;
    let expected = message_ids(&list_messages_ok(&env, &user, &chat, &[]).await);

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=expected.len() {
        let page = match cursor.as_deref() {
            None => list_messages_ok(&env, &user, &chat, &[("limit", "2")]).await,
            Some(cursor) => {
                list_messages_ok(&env, &user, &chat, &[("limit", "2"), ("cursor", cursor)]).await
            }
        };
        assert!(items(&page).len() <= 2, "{page}");
        seen.extend(message_ids(&page));
        cursor = next_cursor(&page);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(cursor, None, "pagination must terminate");
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn descending_order_paginates_with_the_cursor_alone() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 3).await;
    let mut expected = message_ids(&list_messages_ok(&env, &user, &chat, &[]).await);
    expected.reverse();

    let first = list_messages_ok(
        &env,
        &user,
        &chat,
        &[("$orderby", "created_at desc"), ("limit", "4")],
    )
    .await;
    let cursor = next_cursor(&first).expect("six messages at limit 4 yield a cursor");
    let second = list_messages_ok(&env, &user, &chat, &[("cursor", cursor.as_str())]).await;

    let mut seen = message_ids(&first);
    seen.extend(message_ids(&second));
    assert_eq!(seen, expected);
}

#[tokio::test]
async fn a_malformed_cursor_is_400_invalid_cursor() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("cursor", "definitely-not-a-cursor")]).await;

    assert_odata_problem(&resp, "cursor", "INVALID_CURSOR");
}

#[tokio::test]
async fn a_cursor_combined_with_orderby_is_400_order_with_cursor() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;
    let first = list_messages_ok(&env, &user, &chat, &[("limit", "1")]).await;
    let cursor = next_cursor(&first).expect("a cursor");

    let resp = list_messages(
        &env,
        &user,
        &chat,
        &[("cursor", cursor.as_str()), ("$orderby", "created_at desc")],
    )
    .await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "ORDER_WITH_CURSOR"),
        "{problem}"
    );
}

// ===========================================================================
// List messages: $filter, $orderby, $select
// ===========================================================================

#[tokio::test]
async fn filtering_by_role_assistant_lists_only_assistant_messages() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(&env, &user, &chat, &[("$filter", "role eq 'assistant'")]).await;

    assert_eq!(message_contents(&page), ["answer 1", "answer 2"]);
}

#[tokio::test]
async fn filtering_by_role_user_lists_only_user_messages() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(&env, &user, &chat, &[("$filter", "role eq 'user'")]).await;

    assert_eq!(message_contents(&page), ["question 1", "question 2"]);
}

#[tokio::test]
async fn filtering_by_id_returns_that_message() {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "question", "answer").await;
    let filter = format!("id eq {}", turn.message_id);

    let page = list_messages_ok(&env, &user, &chat, &[("$filter", filter.as_str())]).await;

    assert_eq!(message_ids(&page), [turn.message_id]);
    assert_eq!(message_contents(&page), ["answer"]);
}

#[tokio::test]
async fn filtering_by_created_at_lower_bound_keeps_every_message() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;

    let page = list_messages_ok(
        &env,
        &user,
        &chat,
        &[("$filter", "created_at gt 2000-01-01T00:00:00Z")],
    )
    .await;

    assert_eq!(items(&page).len(), 2);
}

#[tokio::test]
async fn filtering_by_created_at_upper_bound_in_the_past_keeps_nothing() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;

    let page = list_messages_ok(
        &env,
        &user,
        &chat,
        &[("$filter", "created_at lt 2000-01-01T00:00:00Z")],
    )
    .await;

    assert!(items(&page).is_empty(), "{page}");
}

#[tokio::test]
async fn filters_combine_with_and() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(
        &env,
        &user,
        &chat,
        &[(
            "$filter",
            "role eq 'assistant' and created_at gt 2000-01-01T00:00:00Z",
        )],
    )
    .await;

    assert_eq!(message_contents(&page), ["answer 1", "answer 2"]);
}

#[tokio::test]
async fn a_filter_and_pagination_combine() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 3).await;

    let first = list_messages_ok(
        &env,
        &user,
        &chat,
        &[("$filter", "role eq 'assistant'"), ("limit", "2")],
    )
    .await;
    let cursor = next_cursor(&first).expect("three assistant messages at limit 2 yield a cursor");
    let second = list_messages_ok(
        &env,
        &user,
        &chat,
        &[
            ("$filter", "role eq 'assistant'"),
            ("cursor", cursor.as_str()),
        ],
    )
    .await;

    assert_eq!(message_contents(&first), ["answer 1", "answer 2"]);
    assert_eq!(message_contents(&second), ["answer 3"]);
}

#[tokio::test]
async fn ordering_by_created_at_descending_reverses_the_conversation() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let page = list_messages_ok(&env, &user, &chat, &[("$orderby", "created_at desc")]).await;

    assert_eq!(
        message_contents(&page),
        ["answer 2", "question 2", "answer 1", "question 1"]
    );
}

#[tokio::test]
async fn ordering_by_role_groups_messages_by_role() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;

    let ascending = list_messages_ok(&env, &user, &chat, &[("$orderby", "role asc")]).await;
    let descending = list_messages_ok(&env, &user, &chat, &[("$orderby", "role desc")]).await;

    assert_eq!(
        message_roles(&ascending),
        ["assistant", "assistant", "user", "user"]
    );
    assert_eq!(
        message_roles(&descending),
        ["user", "user", "assistant", "assistant"]
    );
}

#[tokio::test]
async fn ordering_by_id_sorts_by_the_message_id() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 2).await;
    let mut expected = message_ids(&list_messages_ok(&env, &user, &chat, &[]).await);
    expected.sort();

    let ascending = list_messages_ok(&env, &user, &chat, &[("$orderby", "id asc")]).await;
    let descending = list_messages_ok(&env, &user, &chat, &[("$orderby", "id desc")]).await;

    assert_eq!(message_ids(&ascending), expected);
    expected.reverse();
    assert_eq!(message_ids(&descending), expected);
}

#[tokio::test]
async fn an_unknown_orderby_field_is_400_invalid_orderby_field() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("$orderby", "no_such_field asc")]).await;

    assert_odata_problem(&resp, "$orderby", "INVALID_ORDERBY_FIELD");
}

#[tokio::test]
async fn an_unknown_filter_field_is_400_with_the_odata_resource_type() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("$filter", "no_such_field eq 1")]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        problem["context"]["resource_type"].as_str(),
        Some(ODATA_RESOURCE_TYPE),
        "{problem}"
    );
}

#[tokio::test]
async fn filtering_on_an_internal_column_is_rejected() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("$filter", "content eq 'x'")]).await;

    assert_problem(&resp, 400, Some("invalid_argument"));
}

#[tokio::test]
async fn a_malformed_filter_is_400_invalid_filter() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("$filter", "role eq")]).await;

    assert_odata_problem(&resp, "$filter", "INVALID_FILTER");
}

#[tokio::test]
async fn an_unsupported_query_option_is_400_unsupported_query_param() {
    let (env, user, chat) = env_user_chat().await;

    let resp = list_messages(&env, &user, &chat, &[("$skip", "1")]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "UNSUPPORTED_QUERY_PARAM"),
        "{problem}"
    );
}

#[tokio::test]
async fn select_is_accepted_and_ignored() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;

    let page = list_messages_ok(&env, &user, &chat, &[("$select", "id")]).await;

    let message = &items(&page)[0];
    assert!(
        message.get("content").is_some(),
        "$select does not trim the response: {message}"
    );
    assert!(message.get("role").is_some(), "{message}");
}

// ===========================================================================
// List messages: scoping and errors
// ===========================================================================

#[tokio::test]
async fn another_users_chat_messages_are_404_naming_the_chat() {
    let (env, owner, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &owner, 1).await;
    let intruder = TestUser::in_tenant(owner.tenant_id);

    let resp = list_messages(&env, &intruder, &chat, &[]).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn another_tenants_chat_messages_are_404_naming_the_chat() {
    let (env, owner, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &owner, 1).await;

    let resp = list_messages(&env, &TestUser::new(), &chat, &[]).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn an_unknown_chat_is_404_naming_the_chat() {
    let env = TestEnv::new().await;
    let user = TestUser::new();

    let resp = list_messages(&env, &user, &Uuid::new_v4().to_string(), &[]).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn a_deleted_chat_has_no_message_list() {
    let (env, user, _) = env_user_chat().await;
    let chat = chat_with_turns(&env, &user, 1).await;
    common::delete_chat(&env, &user, &chat).await;

    let resp = list_messages(&env, &user, &chat, &[]).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_400_invalid_path_params() {
    let env = TestEnv::new().await;

    let resp = list_messages(&env, &TestUser::new(), "not-a-uuid", &[]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "invalid_path_params"),
        "{problem}"
    );
}

#[tokio::test]
async fn a_denied_message_list_is_403() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;

    let resp = list_messages(&env, &TestUser::new(), &Uuid::new_v4().to_string(), &[]).await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

// ===========================================================================
// Reactions
// ===========================================================================

async fn put_reaction(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    message: &str,
    body: Value,
) -> mini_chat::test_support::TestResponse {
    env.request(user, Method::PUT, &reaction_path(chat, message), Some(body))
        .await
}

async fn delete_reaction(
    env: &TestEnv,
    user: &TestUser,
    chat: &str,
    message: &str,
) -> mini_chat::test_support::TestResponse {
    env.request(user, Method::DELETE, &reaction_path(chat, message), None)
        .await
}

/// A chat with one completed turn: `(env, user, chat, user message id, assistant message id)`.
async fn reaction_fixture() -> (TestEnv, TestUser, String, String, String) {
    let (env, user, chat) = env_user_chat().await;
    let turn = complete_turn(&env, &user, &chat, "question", "answer").await;
    let rows = message_rows(&env, &chat).await;
    let user_message = rows
        .iter()
        .find(|row| row.role == "user")
        .unwrap()
        .id
        .to_string();
    (env, user, chat, user_message, turn.message_id)
}

#[tokio::test]
async fn liking_an_assistant_message_returns_200_with_the_reaction() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    let body = resp.json();
    assert_eq!(body["message_id"], json!(assistant));
    assert_eq!(body["reaction"], json!("like"));
    let _created_at = parse_ts(&body["created_at"]);
    let mut keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["created_at", "message_id", "reaction"]);
}

#[tokio::test]
async fn disliking_an_assistant_message_returns_200_with_the_reaction() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "dislike" }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json()["reaction"], json!("dislike"));
    assert_eq!(reaction_rows(&env, &assistant).await, ["dislike"]);
}

#[tokio::test]
async fn putting_a_reaction_twice_replaces_it_with_one_row() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "dislike" }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json()["reaction"], json!("dislike"));
    assert_eq!(
        reaction_rows(&env, &assistant).await,
        ["dislike"],
        "an upsert, not an insert"
    );
}

#[tokio::test]
async fn putting_the_same_reaction_twice_is_idempotent() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let first = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    let second = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    expect_status(&first, StatusCode::OK);
    expect_status(&second, StatusCode::OK);
    assert_eq!(reaction_rows(&env, &assistant).await, ["like"]);
}

#[tokio::test]
async fn deleting_a_reaction_returns_204_with_an_empty_body_and_removes_it() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    let resp = delete_reaction(&env, &user, &chat, &assistant).await;

    expect_status(&resp, StatusCode::NO_CONTENT);
    assert!(resp.body.is_empty(), "204 has no body");
    assert!(reaction_rows(&env, &assistant).await.is_empty());
}

#[tokio::test]
async fn deleting_a_reaction_is_idempotent() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    let first = delete_reaction(&env, &user, &chat, &assistant).await;
    let second = delete_reaction(&env, &user, &chat, &assistant).await;

    expect_status(&first, StatusCode::NO_CONTENT);
    expect_status(&second, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_when_no_reaction_exists_is_204() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let resp = delete_reaction(&env, &user, &chat, &assistant).await;

    expect_status(&resp, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_reaction_can_be_set_again_after_it_was_removed() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    delete_reaction(&env, &user, &chat, &assistant).await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "dislike" }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(reaction_rows(&env, &assistant).await, ["dislike"]);
}

#[tokio::test]
async fn reacting_to_a_user_message_is_400_failed_precondition_reaction_target() {
    let (env, user, chat, user_message, _) = reaction_fixture().await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &user_message,
        json!({ "reaction": "like" }),
    )
    .await;

    assert_failed_precondition(&resp, "reaction_target", "STATE");
    assert!(reaction_rows(&env, &user_message).await.is_empty());
}

#[tokio::test]
async fn removing_a_reaction_from_a_user_message_is_400_failed_precondition_reaction_target() {
    let (env, user, chat, user_message, _) = reaction_fixture().await;

    let resp = delete_reaction(&env, &user, &chat, &user_message).await;

    assert_failed_precondition(&resp, "reaction_target", "STATE");
}

#[tokio::test]
async fn an_invalid_reaction_value_is_400_invalid_reaction() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    for value in ["love", "", "Like", "thumbs_up"] {
        let resp = put_reaction(&env, &user, &chat, &assistant, json!({ "reaction": value })).await;
        assert_invalid_argument(&resp, "reaction", "INVALID_REACTION");
    }
    assert!(reaction_rows(&env, &assistant).await.is_empty());
}

#[tokio::test]
async fn the_reaction_value_is_validated_before_authorization() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;

    let resp = put_reaction(
        &env,
        &TestUser::new(),
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        json!({ "reaction": "love" }),
    )
    .await;

    assert_invalid_argument(&resp, "reaction", "INVALID_REACTION");
}

#[tokio::test]
async fn a_valid_reaction_with_a_denied_pdp_is_403() {
    let env = TestEnvBuilder::new().authz(AuthzMode::Deny).build().await;

    let resp = put_reaction(
        &env,
        &TestUser::new(),
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        json!({ "reaction": "like" }),
    )
    .await;

    assert_problem(&resp, 403, Some("permission_denied"));
}

#[tokio::test]
async fn a_body_without_the_reaction_field_is_422_invalid_json_body() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    for body in [
        json!({}),
        json!({ "reaction": null }),
        json!({ "reaction": 1 }),
    ] {
        let resp = put_reaction(&env, &user, &chat, &assistant, body).await;
        let problem = assert_problem(&resp, 422, Some("invalid_argument"));
        assert!(
            has_violation_reason(&problem, "invalid_json_body"),
            "{problem}"
        );
    }
}

#[tokio::test]
async fn a_reaction_body_without_a_json_content_type_is_415() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let resp = env
        .request_raw(
            &user,
            common::raw_request(
                Method::PUT,
                &reaction_path(&chat, &assistant),
                Some("text/plain"),
                "{\"reaction\":\"like\"}",
            ),
        )
        .await;

    assert_problem(&resp, 415, Some("invalid_argument"));
}

#[tokio::test]
async fn an_unknown_message_is_404_naming_the_message_resource() {
    let (env, user, chat, _, _) = reaction_fixture().await;
    let unknown = Uuid::new_v4().to_string();

    let put = put_reaction(&env, &user, &chat, &unknown, json!({ "reaction": "like" })).await;
    let delete = delete_reaction(&env, &user, &chat, &unknown).await;

    assert_not_found(&put, MESSAGE_RESOURCE_TYPE);
    assert_not_found(&delete, MESSAGE_RESOURCE_TYPE);
}

#[tokio::test]
async fn a_message_of_another_chat_is_404_naming_the_message_resource() {
    let (env, user, chat, _, _) = reaction_fixture().await;
    let other = new_chat(&env, &user).await;
    let foreign = complete_turn(&env, &user, &other, "q", "a")
        .await
        .message_id;

    let resp = put_reaction(&env, &user, &chat, &foreign, json!({ "reaction": "like" })).await;

    assert_not_found(&resp, MESSAGE_RESOURCE_TYPE);
    assert!(reaction_rows(&env, &foreign).await.is_empty());
}

#[tokio::test]
async fn another_users_chat_is_404_naming_the_chat_resource() {
    let (env, owner, chat, _, assistant) = reaction_fixture().await;
    let intruder = TestUser::in_tenant(owner.tenant_id);

    let put = put_reaction(
        &env,
        &intruder,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    let delete = delete_reaction(&env, &intruder, &chat, &assistant).await;

    assert_not_found(&put, CHAT_RESOURCE_TYPE);
    assert_not_found(&delete, CHAT_RESOURCE_TYPE);
    assert!(reaction_rows(&env, &assistant).await.is_empty());
}

#[tokio::test]
async fn another_tenants_chat_is_404_naming_the_chat_resource() {
    let (env, _owner, chat, _, assistant) = reaction_fixture().await;

    let resp = put_reaction(
        &env,
        &TestUser::new(),
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn an_unknown_chat_is_404_for_reactions() {
    let env = TestEnv::new().await;

    let resp = put_reaction(
        &env,
        &TestUser::new(),
        &Uuid::new_v4().to_string(),
        &Uuid::new_v4().to_string(),
        json!({ "reaction": "like" }),
    )
    .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn reactions_on_a_deleted_chat_are_404() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    common::delete_chat(&env, &user, &chat).await;

    let resp = put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn non_uuid_path_ids_are_400_invalid_path_params() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;

    let bad_message = put_reaction(
        &env,
        &user,
        &chat,
        "not-a-uuid",
        json!({ "reaction": "like" }),
    )
    .await;
    let bad_chat = put_reaction(
        &env,
        &user,
        "not-a-uuid",
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;

    for resp in [bad_message, bad_chat] {
        let problem = assert_problem(&resp, 400, Some("invalid_argument"));
        assert!(
            has_violation_reason(&problem, "invalid_path_params"),
            "{problem}"
        );
    }
}

#[tokio::test]
async fn my_reaction_is_reflected_in_the_message_list() {
    let (env, user, chat, user_message, assistant) = reaction_fixture().await;
    let my_reaction_of = |page: &Value, id: &str| -> Value {
        items(page)
            .iter()
            .find(|m| m["id"] == json!(id))
            .unwrap_or_else(|| panic!("message {id} not listed: {page}"))["my_reaction"]
            .clone()
    };

    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    assert_eq!(my_reaction_of(&page, &assistant), Value::Null);

    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    assert_eq!(my_reaction_of(&page, &assistant), json!("like"));
    assert_eq!(
        my_reaction_of(&page, &user_message),
        Value::Null,
        "user messages never carry one"
    );

    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "dislike" }),
    )
    .await;
    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    assert_eq!(my_reaction_of(&page, &assistant), json!("dislike"));

    delete_reaction(&env, &user, &chat, &assistant).await;
    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    assert_eq!(my_reaction_of(&page, &assistant), Value::Null);
}

#[tokio::test]
async fn my_reaction_is_present_in_a_filtered_list() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    let filter = format!("id eq {assistant}");

    let page = list_messages_ok(&env, &user, &chat, &[("$filter", filter.as_str())]).await;

    assert_eq!(items(&page)[0]["my_reaction"], json!("like"));
}

#[tokio::test]
async fn reactions_are_stored_per_user() {
    let (env, user, chat, _, assistant) = reaction_fixture().await;
    exec_sql(
        &env,
        &format!(
            "INSERT INTO message_reactions (id, message_id, user_id, tenant_id, reaction, created_at) \
             SELECT randomblob(16), id, randomblob(16), tenant_id, 'dislike', '2000-01-01T00:00:00Z' \
             FROM messages WHERE hex(id) = '{}'",
            uuid_hex(Uuid::parse_str(&assistant).unwrap())
        ),
    )
    .await;

    put_reaction(
        &env,
        &user,
        &chat,
        &assistant,
        json!({ "reaction": "like" }),
    )
    .await;
    let mut both = reaction_rows(&env, &assistant).await;
    both.sort();
    assert_eq!(both, ["dislike", "like"], "one row per (message, user)");

    delete_reaction(&env, &user, &chat, &assistant).await;
    assert_eq!(
        reaction_rows(&env, &assistant).await,
        ["dislike"],
        "removing mine leaves the other user's reaction"
    );
    let page = list_messages_ok(&env, &user, &chat, &[]).await;
    assert_eq!(items(&page)[1]["my_reaction"], Value::Null);
}

#[tokio::test]
async fn reaction_errors_are_canonical_problems() {
    let (env, user, chat, user_message, _) = reaction_fixture().await;

    let not_assistant = put_reaction(
        &env,
        &user,
        &chat,
        &user_message,
        json!({ "reaction": "like" }),
    )
    .await;
    let bad_value = put_reaction(
        &env,
        &user,
        &chat,
        &user_message,
        json!({ "reaction": "x" }),
    )
    .await;

    assert_problem(&not_assistant, 400, Some("failed_precondition"));
    assert_problem(&bad_value, 400, Some("invalid_argument"));
}
