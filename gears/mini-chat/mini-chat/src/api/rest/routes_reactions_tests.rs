#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 6 of `cpt-cf-mini-chat-feature-chat-crud` through the REST layer: set
//! and remove like/dislike reactions on assistant messages
//! (`cpt-cf-mini-chat-flow-chat-crud-set-reaction`, `-remove-reaction`,
//! `cpt-cf-mini-chat-state-chat-crud-message-reaction`).

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_test_env::{
    CHAT_PEP_TYPE, CHAT_TYPE, ChatEnv, MESSAGE_TYPE, Options, blob, chat_id, chat_uri, msg_id,
    parse_ts, reaction_uri, request_id, ts,
};
use crate::infra::db::test_support::message_row;
use crate::test_doubles::{
    OTHER_TENANT, OTHER_USER, PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx,
    user_ctx,
};

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

fn qblob(id: Uuid) -> String {
    blob(id).to_uppercase()
}

/// A chat of the default user with a user message (1), an assistant message
/// (2) and a system message (3).
async fn chat_with_messages(env: &ChatEnv) -> Uuid {
    let chat = env.seed_chat(1, Some("History"), 10).await;
    env.seed_message(chat, 1, "user", 1).await;
    env.seed_message(chat, 2, "assistant", 2).await;
    env.seed_message(chat, 3, "system", 3).await;
    chat
}

const USER_MESSAGE: u128 = 1;
const ASSISTANT: u128 = 2;
const SYSTEM_MESSAGE: u128 = 3;

async fn reaction_rows(env: &ChatEnv) -> i64 {
    env.db.count("message_reactions").await
}

async fn reaction_of(env: &ChatEnv, message: Uuid, user: Uuid) -> Option<String> {
    let text = env
        .text(&format!(
            "SELECT COALESCE(group_concat(reaction), '') FROM message_reactions \
             WHERE message_id = {} AND user_id = {}",
            blob(message),
            blob(user)
        ))
        .await;
    (!text.is_empty()).then_some(text)
}

async fn reaction_column(env: &ChatEnv, message: Uuid, user: Uuid, column: &str) -> String {
    env.text(&format!(
        "SELECT quote({column}) FROM message_reactions WHERE message_id = {} AND user_id = {}",
        blob(message),
        blob(user)
    ))
    .await
}

async fn my_reaction(env: &ChatEnv, chat: Uuid, message: Uuid) -> Value {
    let reply = env.list_messages(chat, "").await;
    reply.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == message.to_string())
        .unwrap_or_else(|| panic!("message not listed: {}", reply.text))["my_reaction"]
        .clone()
}

// ================================================================================================
// PUT
// ================================================================================================

#[tokio::test]
async fn put_like_returns_200_with_message_id_reaction_and_created_at() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    let before = chrono::Utc::now();

    let reply = env.put_reaction(chat, message, "like").await;

    let after = chrono::Utc::now();
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        keys(&reply.json),
        ["message_id", "reaction", "created_at"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect::<BTreeSet<_>>(),
        "{}",
        reply.text
    );
    assert_eq!(reply.json["message_id"], message.to_string());
    assert_eq!(reply.json["reaction"], "like");
    let created = parse_ts(reply.json["created_at"].as_str().unwrap());
    assert!(
        created >= before - chrono::Duration::seconds(2),
        "{created}"
    );
    assert!(created <= after + chrono::Duration::seconds(2), "{created}");
    assert_eq!(my_reaction(&env, chat, message).await, "like");
}

#[tokio::test]
async fn put_stores_one_row_for_the_caller_in_the_tenant_of_the_chat() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);

    let reply = env.put_reaction(chat, message, "dislike").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reaction_rows(&env).await, 1);
    assert_eq!(
        reaction_column(&env, message, USER, "tenant_id").await,
        qblob(USER_TENANT)
    );
    assert_eq!(
        reaction_column(&env, message, USER, "reaction").await,
        "'dislike'"
    );
    let id = reaction_column(&env, message, USER, "id").await;
    assert_ne!(id, "NULL");
    let stored_at = reaction_column(&env, message, USER, "created_at").await;
    assert_eq!(
        parse_ts(&stored_at),
        parse_ts(reply.json["created_at"].as_str().unwrap()),
        "the response carries the stored time"
    );
}

#[tokio::test]
async fn put_ignores_unknown_fields_and_takes_tenant_and_user_from_the_context() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);

    let reply = env
        .send_json(
            "PUT",
            &reaction_uri(chat, message),
            &user_ctx(),
            &json!({
                "reaction": "like",
                "user_id": OTHER_USER.to_string(),
                "tenant_id": OTHER_TENANT.to_string(),
                "message_id": msg_id(99).to_string(),
                "id": Uuid::from_u128(5).to_string()
            }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["message_id"], message.to_string());
    assert_eq!(reaction_rows(&env).await, 1);
    assert_eq!(
        reaction_of(&env, message, USER).await.as_deref(),
        Some("like")
    );
    assert_eq!(reaction_of(&env, message, OTHER_USER).await, None);
    assert_eq!(
        reaction_column(&env, message, USER, "tenant_id").await,
        qblob(USER_TENANT)
    );
}

#[tokio::test]
async fn a_second_put_with_the_other_value_replaces_the_reaction_in_place() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    let first = env.put_reaction(chat, message, "like").await;
    let first_id = reaction_column(&env, message, USER, "id").await;

    let second = env.put_reaction(chat, message, "dislike").await;

    assert_eq!(second.status, StatusCode::OK, "{}", second.text);
    assert_eq!(second.json["reaction"], "dislike");
    assert_eq!(reaction_rows(&env).await, 1, "one row per user and message");
    assert_eq!(reaction_column(&env, message, USER, "id").await, first_id);
    assert_eq!(my_reaction(&env, chat, message).await, "dislike");
    assert!(
        parse_ts(second.json["created_at"].as_str().unwrap())
            >= parse_ts(first.json["created_at"].as_str().unwrap())
    );
    // And back to like.
    let third = env.put_reaction(chat, message, "like").await;
    assert_eq!(third.json["reaction"], "like");
    assert_eq!(reaction_rows(&env).await, 1);
    assert_eq!(reaction_column(&env, message, USER, "id").await, first_id);
}

#[tokio::test]
async fn a_repeated_put_with_the_same_value_succeeds_and_refreshes_created_at() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    // The seeded reaction is from 2024.
    env.seed_reaction(1, message, USER, "like").await;
    let seeded_id = reaction_column(&env, message, USER, "id").await;

    let reply = env.put_reaction(chat, message, "like").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["reaction"], "like");
    assert!(
        parse_ts(reply.json["created_at"].as_str().unwrap()) > parse_ts(&ts(5)),
        "created_at is the time of the PUT: {}",
        reply.text
    );
    assert_eq!(reaction_rows(&env).await, 1);
    assert_eq!(reaction_column(&env, message, USER, "id").await, seeded_id);
    let again = env.put_reaction(chat, message, "like").await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text);
    assert_eq!(reaction_rows(&env).await, 1);
}

#[tokio::test]
async fn put_does_not_change_the_chat_or_the_messages() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let chat_before = env.chat_snapshot(chat).await;
    let messages_before = env.dump("messages").await;

    let like = env.put_reaction(chat, msg_id(ASSISTANT), "like").await;
    let dislike = env.put_reaction(chat, msg_id(ASSISTANT), "dislike").await;

    assert_eq!(like.status, StatusCode::OK, "{}", like.text);
    assert_eq!(dislike.status, StatusCode::OK, "{}", dislike.text);
    assert_eq!(
        env.chat_snapshot(chat).await,
        chat_before,
        "updated_at is not bumped"
    );
    assert_eq!(env.dump("messages").await, messages_before);
}

#[tokio::test]
async fn concurrent_puts_leave_exactly_one_row() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);

    let (a, b, c, d) = tokio::join!(
        env.put_reaction(chat, message, "like"),
        env.put_reaction(chat, message, "dislike"),
        env.put_reaction(chat, message, "like"),
        env.put_reaction(chat, message, "dislike"),
    );

    for reply in [&a, &b, &c, &d] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(reaction_rows(&env).await, 1);
    let stored = reaction_of(&env, message, USER).await.unwrap();
    assert!(stored == "like" || stored == "dislike");
}

#[tokio::test]
async fn an_invalid_reaction_value_is_400_invalid_reaction_before_any_pdp_call() {
    for value in [
        "love",
        "LIKE",
        "Like",
        "Dislike",
        "",
        " like",
        "like ",
        "thumbs_up",
        "\u{1f44d}",
    ] {
        // A denying PDP, an unknown chat and an unknown message prove the
        // value check comes first.
        let env = ChatEnv::with(Options::with_pdp(PdpReply::Deny)).await;

        let reply = env.put_reaction(chat_id(99), msg_id(99), value).await;

        reply.assert_invalid("reaction", "INVALID_REACTION");
        assert!(env.pdp.requests().is_empty(), "{value:?}");
        assert_eq!(reaction_rows(&env).await, 0);
    }
}

#[tokio::test]
async fn an_invalid_reaction_value_on_a_real_message_stores_nothing() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    env.seed_reaction(1, msg_id(ASSISTANT), USER, "like").await;

    let reply = env.put_reaction(chat, msg_id(ASSISTANT), "love").await;

    reply.assert_invalid("reaction", "INVALID_REACTION");
    assert_eq!(
        reaction_of(&env, msg_id(ASSISTANT), USER).await.as_deref(),
        Some("like"),
        "an existing reaction is not replaced by an invalid value"
    );
}

#[tokio::test]
async fn a_body_without_a_string_reaction_is_422_and_malformed_input_is_400_or_415() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let uri = reaction_uri(chat, msg_id(ASSISTANT));

    for body in [
        json!({}),
        json!({ "reaction": null }),
        json!({ "reaction": 1 }),
        json!({ "reaction": true }),
        json!({ "reaction": ["like"] }),
        json!({ "type": "like" }),
        json!("like"),
    ] {
        let reply = env.send_json("PUT", &uri, &user_ctx(), &body).await;

        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            reply.text
        );
        assert_eq!(reply.first_violation().1, "invalid_json_body");
    }
    let malformed = env
        .send_raw(
            "PUT",
            &uri,
            &user_ctx(),
            Some("application/json"),
            Some("{\"reaction\":".to_owned()),
        )
        .await;
    let wrong_type = env
        .send_raw(
            "PUT",
            &uri,
            &user_ctx(),
            Some("text/plain"),
            Some(r#"{"reaction":"like"}"#.to_owned()),
        )
        .await;
    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "{}",
        malformed.text
    );
    assert_eq!(
        wrong_type.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        wrong_type.text
    );
    assert_eq!(reaction_rows(&env).await, 0);
}

// ================================================================================================
// The assistant-only rule
// ================================================================================================

fn assert_reaction_target_violation(reply: &crate::chat_test_env::Reply) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        reply.category().contains("failed_precondition"),
        "{}",
        reply.text
    );
    let violations = reply.json["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{}", reply.text);
    assert_eq!(violations[0]["subject"], "reaction_target");
    assert_eq!(violations[0]["type"], "STATE");
    assert!(violations[0]["description"].is_string());
}

#[tokio::test]
async fn put_and_delete_on_a_user_or_system_message_are_failed_precondition_reaction_target() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;

    for message in [msg_id(USER_MESSAGE), msg_id(SYSTEM_MESSAGE)] {
        let put = env.put_reaction(chat, message, "like").await;
        let delete = env.delete_reaction(chat, message).await;

        assert_reaction_target_violation(&put);
        assert_reaction_target_violation(&delete);
    }
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn a_reaction_on_a_non_assistant_message_removes_nothing_either() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    // A row the API cannot create: it must survive a rejected DELETE.
    env.seed_reaction(1, msg_id(USER_MESSAGE), USER, "like")
        .await;

    let delete = env.delete_reaction(chat, msg_id(USER_MESSAGE)).await;

    assert_reaction_target_violation(&delete);
    assert_eq!(reaction_rows(&env).await, 1);
}

// ================================================================================================
// 404
// ================================================================================================

#[tokio::test]
async fn an_unknown_message_is_404_with_the_message_resource_type() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;

    let put = env.put_reaction(chat, msg_id(99), "like").await;
    let delete = env.delete_reaction(chat, msg_id(99)).await;

    put.assert_not_found(MESSAGE_TYPE);
    delete.assert_not_found(MESSAGE_TYPE);
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn a_soft_deleted_message_is_404_with_the_message_resource_type() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    env.soft_delete_message(msg_id(ASSISTANT)).await;

    let put = env.put_reaction(chat, msg_id(ASSISTANT), "like").await;
    let delete = env.delete_reaction(chat, msg_id(ASSISTANT)).await;

    put.assert_not_found(MESSAGE_TYPE);
    delete.assert_not_found(MESSAGE_TYPE);
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn a_message_of_another_chat_is_404_with_the_message_resource_type() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let other = env.seed_chat(2, Some("Other"), 10).await;
    env.db
        .insert(
            "messages",
            message_row(msg_id(50), USER_TENANT, other)
                .set("role", "assistant")
                .set("request_id", request_id(50))
                .set("created_at", ts(1)),
        )
        .await
        .unwrap();

    let put = env.put_reaction(chat, msg_id(50), "like").await;
    let delete = env.delete_reaction(chat, msg_id(50)).await;

    put.assert_not_found(MESSAGE_TYPE);
    delete.assert_not_found(MESSAGE_TYPE);
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn an_unknown_deleted_or_foreign_chat_is_404_with_the_chat_resource_type() {
    let env = ChatEnv::new().await;
    let deleted = env.seed_chat(1, Some("gone"), 10).await;
    env.seed_message(deleted, 11, "assistant", 1).await;
    env.db.soft_delete("chats", deleted).await;
    let foreign_user = env
        .seed_chat_of(2, USER_TENANT, OTHER_USER, Some("theirs"), 10)
        .await;
    env.seed_message(foreign_user, 12, "assistant", 1).await;
    let foreign_tenant = env
        .seed_chat_of(3, OTHER_TENANT, USER, Some("other tenant"), 10)
        .await;

    for (chat, message) in [
        (chat_id(99), msg_id(11)),
        (deleted, msg_id(11)),
        (foreign_user, msg_id(12)),
        (foreign_tenant, msg_id(99)),
    ] {
        let put = env.put_reaction(chat, message, "like").await;
        let delete = env.delete_reaction(chat, message).await;

        put.assert_not_found(CHAT_TYPE);
        delete.assert_not_found(CHAT_TYPE);
    }
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn another_user_cannot_react_on_or_unreact_a_message_of_the_chat() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    env.seed_reaction(1, message, USER, "like").await;
    let uri = reaction_uri(chat, message);

    for ctx in [other_user_ctx(), other_tenant_ctx()] {
        let put = env
            .send_json("PUT", &uri, &ctx, &json!({ "reaction": "dislike" }))
            .await;
        let delete = env.send_raw("DELETE", &uri, &ctx, None, None).await;

        put.assert_not_found(CHAT_TYPE);
        delete.assert_not_found(CHAT_TYPE);
    }
    assert_eq!(
        reaction_of(&env, message, USER).await.as_deref(),
        Some("like")
    );
    assert_eq!(reaction_rows(&env).await, 1);
}

#[tokio::test]
async fn path_parameters_that_are_not_uuids_are_400_invalid_path_params() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;

    for uri in [
        format!(
            "/mini-chat/v1/chats/not-a-uuid/messages/{}/reaction",
            msg_id(ASSISTANT)
        ),
        format!("/mini-chat/v1/chats/{chat}/messages/not-a-uuid/reaction"),
    ] {
        let put = env
            .send_json("PUT", &uri, &user_ctx(), &json!({ "reaction": "like" }))
            .await;
        let delete = env.send_raw("DELETE", &uri, &user_ctx(), None, None).await;

        for reply in [&put, &delete] {
            assert_eq!(
                reply.status,
                StatusCode::BAD_REQUEST,
                "{uri}: {}",
                reply.text
            );
            assert_eq!(reply.first_violation().1, "invalid_path_params");
        }
    }
    assert!(env.pdp.requests().is_empty());
}

// ================================================================================================
// DELETE
// ================================================================================================

#[tokio::test]
async fn delete_removes_the_callers_reaction_and_returns_204_with_an_empty_body() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    env.put_reaction(chat, message, "like").await;
    assert_eq!(my_reaction(&env, chat, message).await, "like");

    let reply = env.delete_reaction(chat, message).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty(), "{}", reply.text);
    assert_eq!(reaction_rows(&env).await, 0);
    assert_eq!(my_reaction(&env, chat, message).await, Value::Null);
}

#[tokio::test]
async fn delete_is_idempotent_with_and_without_an_existing_reaction() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);

    let without = env.delete_reaction(chat, message).await;
    env.put_reaction(chat, message, "dislike").await;
    let with = env.delete_reaction(chat, message).await;
    let again = env.delete_reaction(chat, message).await;

    for reply in [&without, &with, &again] {
        assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
        assert!(reply.text.is_empty());
    }
    assert_eq!(reaction_rows(&env).await, 0);
}

#[tokio::test]
async fn another_users_reaction_is_neither_visible_nor_removable() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    let message = msg_id(ASSISTANT);
    // A reaction of another user on the same message (only seedable).
    env.seed_reaction(1, message, OTHER_USER, "like").await;

    let seen = my_reaction(&env, chat, message).await;
    assert_eq!(seen, Value::Null, "the other user's reaction is not mine");

    let deleted = env.delete_reaction(chat, message).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.text);
    assert_eq!(
        reaction_of(&env, message, OTHER_USER).await.as_deref(),
        Some("like"),
        "a DELETE never removes another user's reaction"
    );

    let put = env.put_reaction(chat, message, "dislike").await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.text);
    assert_eq!(my_reaction(&env, chat, message).await, "dislike");
    assert_eq!(reaction_rows(&env).await, 2, "one row per user");
    assert_eq!(
        reaction_of(&env, message, OTHER_USER).await.as_deref(),
        Some("like"),
        "a PUT never replaces another user's reaction"
    );

    let deleted_own = env.delete_reaction(chat, message).await;
    assert_eq!(deleted_own.status, StatusCode::NO_CONTENT);
    assert_eq!(reaction_rows(&env).await, 1);
    assert_eq!(
        reaction_of(&env, message, OTHER_USER).await.as_deref(),
        Some("like")
    );
}

#[tokio::test]
async fn reactions_on_different_messages_are_independent() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    env.seed_message(chat, 4, "assistant", 4).await;

    env.put_reaction(chat, msg_id(ASSISTANT), "like").await;
    env.put_reaction(chat, msg_id(4), "dislike").await;
    env.delete_reaction(chat, msg_id(ASSISTANT)).await;

    assert_eq!(
        my_reaction(&env, chat, msg_id(ASSISTANT)).await,
        Value::Null
    );
    assert_eq!(my_reaction(&env, chat, msg_id(4)).await, "dislike");
    assert_eq!(reaction_rows(&env).await, 1);
}

#[tokio::test]
async fn delete_does_not_change_the_chat_or_the_messages() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;
    env.put_reaction(chat, msg_id(ASSISTANT), "like").await;
    let chat_before = env.chat_snapshot(chat).await;
    let messages_before = env.dump("messages").await;

    let reply = env.delete_reaction(chat, msg_id(ASSISTANT)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(env.chat_snapshot(chat).await, chat_before);
    assert_eq!(env.dump("messages").await, messages_before);
}

// ================================================================================================
// PEP
// ================================================================================================

#[tokio::test]
async fn put_and_delete_ask_the_pdp_for_their_action_on_the_chat_id_not_the_message_id() {
    let env = ChatEnv::new().await;
    let chat = chat_with_messages(&env).await;

    env.put_reaction(chat, msg_id(ASSISTANT), "like").await;
    let put_request = env.only_pdp_request();
    let env2 = ChatEnv::new().await;
    let chat2 = chat_with_messages(&env2).await;
    env2.delete_reaction(chat2, msg_id(ASSISTANT)).await;
    let delete_request = env2.only_pdp_request();

    assert_eq!(put_request.action.name, "set_reaction");
    assert_eq!(put_request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(put_request.resource.id, Some(chat));
    assert!(put_request.context.require_constraints);
    assert_eq!(delete_request.action.name, "delete_reaction");
    assert_eq!(delete_request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(delete_request.resource.id, Some(chat2));
    assert!(delete_request.context.require_constraints);
    let _ = chat_uri(chat);
}
