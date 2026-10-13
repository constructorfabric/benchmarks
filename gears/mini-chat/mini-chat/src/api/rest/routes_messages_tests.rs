#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5 of `cpt-cf-mini-chat-feature-chat-crud` through the REST layer: the
//! message history API, its ordering, pagination and `OData` behaviour, and the
//! message response contract (`request_id`, `attachments`, `my_reaction`, the
//! omitted keys).

use std::collections::BTreeSet;

use axum::http::StatusCode;
use base64::Engine as _;
use serde_json::{Value, json};
use tracing::Level;
use uuid::Uuid;

use crate::chat_test_env::{
    CHAT_PEP_TYPE, CHAT_TYPE, ChatEnv, ODATA_TYPE, att_id, chat_id, chat_uri, enc, msg_id,
    next_cursor, parse_ts, request_id, rfc3339, ts,
};
use crate::infra::db::test_support::{Row, attachment_row, message_row};
use crate::test_doubles::{
    LogCapture, OTHER_TENANT, OTHER_USER, USER, USER_TENANT, other_tenant_ctx, other_user_ctx,
};

const REQUIRED_KEYS: [&str; 7] = [
    "id",
    "request_id",
    "role",
    "content",
    "attachments",
    "my_reaction",
    "created_at",
];

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("not an object: {value}"))
        .keys()
        .cloned()
        .collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn ids(reply: &crate::chat_test_env::Reply) -> Vec<String> {
    crate::chat_test_env::item_ids(reply)
}

fn msg(chat: Uuid, n: u128, role: &str, created: i64) -> Row {
    message_row(msg_id(n), USER_TENANT, chat)
        .set("role", role)
        .set("content", format!("content-{n}"))
        .set("request_id", request_id(n))
        .set("created_at", ts(created))
}

async fn seed(env: &ChatEnv, chat: Uuid, n: u128, row: Row) -> Uuid {
    env.seed_message_row(chat, n, row).await
}

async fn seed_attachment_row(env: &ChatEnv, row: Row) {
    env.db.insert("attachments", row).await.expect("attachment");
}

fn standard_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// A chat of the default user with a user and an assistant message of one turn.
async fn chat_with_turn(env: &ChatEnv) -> (Uuid, Uuid, Uuid) {
    let chat = env.seed_chat(1, Some("History"), 10).await;
    let user = seed(env, chat, 1, msg(chat, 1, "user", 1)).await;
    let assistant = seed(
        env,
        chat,
        2,
        msg(chat, 2, "assistant", 2).set("request_id", request_id(1)),
    )
    .await;
    (chat, user, assistant)
}

// ================================================================================================
// Order and shape
// ================================================================================================

#[tokio::test]
async fn messages_are_listed_in_created_at_asc_then_id_asc_order() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    // Inserted out of order; 8 and 9 share a timestamp.
    seed(&env, chat, 3, msg(chat, 3, "user", 30)).await;
    seed(&env, chat, 9, msg(chat, 9, "assistant", 20)).await;
    seed(&env, chat, 1, msg(chat, 1, "user", 10)).await;
    seed(&env, chat, 8, msg(chat, 8, "user", 20)).await;

    let reply = env.list_messages(chat, "").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let expected: Vec<String> = [1, 8, 9, 3]
        .iter()
        .map(|n| msg_id(*n).to_string())
        .collect();
    assert_eq!(ids(&reply), expected);
}

#[tokio::test]
async fn the_page_has_items_and_page_info_in_the_platform_shape() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    let reply = env.list_messages(chat, "").await;

    assert_eq!(keys(&reply.json), key_set(&["items", "page_info"]));
    assert_eq!(
        reply.json["page_info"],
        json!({ "limit": 20, "next_cursor": null, "prev_cursor": null })
    );
    assert_eq!(reply.json["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_chat_without_messages_has_an_empty_page() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("Empty"), 10).await;

    let reply = env.list_messages(chat, "").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.json,
        json!({
            "items": [],
            "page_info": { "limit": 20, "next_cursor": null, "prev_cursor": null }
        })
    );
}

// ================================================================================================
// The message response contract
// ================================================================================================

#[tokio::test]
async fn a_user_message_has_exactly_the_required_keys_and_nothing_optional() {
    let env = ChatEnv::new().await;
    let (chat, user, _) = chat_with_turn(&env).await;

    let reply = env.list_messages(chat, "").await;

    let item = &reply.json["items"][0];
    assert_eq!(keys(item), key_set(&REQUIRED_KEYS), "{}", reply.text);
    assert_eq!(item["id"], user.to_string());
    assert_eq!(item["request_id"], request_id(1).to_string());
    assert_eq!(item["role"], "user");
    assert_eq!(item["content"], "content-1");
    assert_eq!(item["attachments"], json!([]));
    assert_eq!(item["my_reaction"], Value::Null);
    assert_eq!(
        parse_ts(item["created_at"].as_str().unwrap()),
        parse_ts(&ts(1))
    );
}

#[tokio::test]
async fn an_assistant_message_adds_model_and_token_counts_when_present() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(
        &env,
        chat,
        1,
        msg(chat, 1, "assistant", 1)
            .set("model", "gpt-5.2")
            .set("input_tokens", 120_i64)
            .set("output_tokens", 45_i64),
    )
    .await;

    let reply = env.list_messages(chat, "").await;

    let item = &reply.json["items"][0];
    let mut expected: Vec<&str> = REQUIRED_KEYS.to_vec();
    expected.extend(["model", "input_tokens", "output_tokens"]);
    assert_eq!(keys(item), key_set(&expected), "{}", reply.text);
    assert_eq!(item["model"], "gpt-5.2");
    assert_eq!(item["input_tokens"], 120);
    assert_eq!(item["output_tokens"], 45);
}

#[tokio::test]
async fn zero_token_counts_and_a_null_model_are_omitted_key_by_key() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(
        &env,
        chat,
        1,
        msg(chat, 1, "assistant", 1)
            .set("model", "gpt-5.2")
            .set("input_tokens", 0_i64)
            .set("output_tokens", 7_i64),
    )
    .await;
    seed(
        &env,
        chat,
        2,
        msg(chat, 2, "assistant", 2)
            .null("model")
            .set("input_tokens", 9_i64)
            .set("output_tokens", 0_i64),
    )
    .await;

    let reply = env.list_messages(chat, "").await;

    let first = &reply.json["items"][0];
    let second = &reply.json["items"][1];
    assert!(first.get("input_tokens").is_none(), "{}", reply.text);
    assert_eq!(first["output_tokens"], 7);
    assert_eq!(first["model"], "gpt-5.2");
    assert!(second.get("output_tokens").is_none(), "{}", reply.text);
    assert_eq!(second["input_tokens"], 9);
    assert!(second.get("model").is_none(), "{}", reply.text);
}

#[tokio::test]
async fn no_internal_or_provider_field_is_exposed() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(
        &env,
        chat,
        1,
        msg(chat, 1, "assistant", 1)
            .set("provider_response_id", "resp_SECRET_PROVIDER_ID")
            .set("model", "gpt-5.2")
            .set("input_tokens", 5_i64)
            .set("output_tokens", 6_i64)
            .set("cache_read_input_tokens", 3_i64)
            .set("cache_write_input_tokens", 2_i64)
            .set("reasoning_tokens", 4_i64)
            .set("is_compressed", true)
            .set("token_estimate", 77_i64),
    )
    .await;

    let reply = env.list_messages(chat, "").await;

    let item = &reply.json["items"][0];
    let mut expected: Vec<&str> = REQUIRED_KEYS.to_vec();
    expected.extend(["model", "input_tokens", "output_tokens"]);
    assert_eq!(keys(item), key_set(&expected), "{}", reply.text);
    assert!(
        !reply.text.contains("resp_SECRET_PROVIDER_ID"),
        "{}",
        reply.text
    );
    for forbidden in [
        "tenant_id",
        "chat_id",
        "provider_response_id",
        "content_type",
        "token_estimate",
        "request_kind",
        "features_used",
        "cache_read_input_tokens",
        "cache_write_input_tokens",
        "reasoning_tokens",
        "is_compressed",
        "deleted_at",
        "user_id",
    ] {
        assert!(
            !reply.text.contains(forbidden),
            "`{forbidden}`: {}",
            reply.text
        );
    }
}

#[tokio::test]
async fn the_user_and_assistant_message_of_one_turn_share_the_request_id() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    let reply = env.list_messages(chat, "").await;

    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(items[0]["request_id"], request_id(1).to_string());
    assert_eq!(items[1]["request_id"], request_id(1).to_string());
    for item in items {
        assert!(Uuid::parse_str(item["request_id"].as_str().unwrap()).is_ok());
    }
}

#[tokio::test]
async fn a_stored_null_request_id_on_the_page_fails_the_request_with_500_and_logs_the_message_id() {
    let logs = LogCapture::start();
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(&env, chat, 1, msg(chat, 1, "user", 1)).await;
    let broken = seed(
        &env,
        chat,
        2,
        msg(chat, 2, "assistant", 2).null("request_id"),
    )
    .await;

    let reply = env.list_messages(chat, "").await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(reply.category().contains("internal"), "{}", reply.text);
    assert!(!reply.text.contains(&broken.to_string()), "{}", reply.text);
    assert!(!reply.text.contains("content-2"), "{}", reply.text);
    let errors = logs.at_containing(Level::ERROR, &broken.to_string());
    assert!(!errors.is_empty(), "{}", logs.all_text());
    assert!(
        !logs.all_text().contains("content-2"),
        "no message content in logs"
    );
}

#[tokio::test]
async fn a_null_request_id_outside_the_page_does_not_fail_the_page() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(&env, chat, 1, msg(chat, 1, "user", 1)).await;
    seed(
        &env,
        chat,
        2,
        msg(chat, 2, "assistant", 2).null("request_id"),
    )
    .await;

    let first = env.list_messages(chat, "?limit=1").await;
    let cursor = next_cursor(&first).expect("a second page");
    let second = env
        .list_messages(chat, &format!("?limit=1&cursor={}", enc(&cursor)))
        .await;

    assert_eq!(first.status, StatusCode::OK, "{}", first.text);
    assert_eq!(
        second.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        second.text
    );
}

#[tokio::test]
async fn soft_deleted_messages_are_never_returned_and_do_not_use_page_slots() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    seed(&env, chat, 1, msg(chat, 1, "user", 1)).await;
    let gone = seed(&env, chat, 2, msg(chat, 2, "assistant", 2)).await;
    seed(&env, chat, 3, msg(chat, 3, "user", 3)).await;
    env.soft_delete_message(gone).await;

    let all = env.list_messages(chat, "").await;
    let paged = env.list_messages(chat, "?limit=2").await;
    let by_id = env
        .list_messages(chat, &format!("?$filter={}", enc(&format!("id eq {gone}"))))
        .await;

    assert_eq!(
        ids(&all),
        vec![msg_id(1).to_string(), msg_id(3).to_string()]
    );
    assert_eq!(ids(&paged), ids(&all));
    assert_eq!(next_cursor(&paged), None);
    assert_eq!(by_id.json["items"], json!([]));
}

// ================================================================================================
// Attachments
// ================================================================================================

#[tokio::test]
async fn attachments_are_summarised_from_message_attachments_in_created_at_then_id_order() {
    let env = ChatEnv::new().await;
    let (chat, user, assistant) = chat_with_turn(&env).await;
    let late = env
        .seed_attachment(chat, 3, "document", "ready", None)
        .await;
    let tie_b = env
        .seed_attachment(chat, 2, "document", "uploaded", None)
        .await;
    let tie_a = env.seed_attachment(chat, 1, "image", "pending", None).await;
    env.link_attachment(chat, user, late, 5).await;
    env.link_attachment(chat, user, tie_b, 3).await;
    env.link_attachment(chat, user, tie_a, 3).await;

    let reply = env.list_messages(chat, "").await;

    let user_item = &reply.json["items"][0];
    let attachments = user_item["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 3, "{}", reply.text);
    // created_at 3 twice (by attachment id), then created_at 5.
    let order: Vec<&str> = attachments
        .iter()
        .map(|a| a["attachment_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        order,
        vec![
            tie_a.to_string().as_str(),
            tie_b.to_string().as_str(),
            late.to_string().as_str()
        ]
    );
    assert_eq!(
        keys(&attachments[0]),
        key_set(&["attachment_id", "kind", "filename", "status"]),
        "{}",
        reply.text
    );
    assert_eq!(attachments[0]["kind"], "image");
    assert_eq!(attachments[0]["filename"], "file-1.bin");
    assert_eq!(attachments[0]["status"], "pending");
    assert_eq!(attachments[1]["kind"], "document");
    assert_eq!(attachments[1]["status"], "uploaded");
    assert_eq!(attachments[2]["status"], "ready");
    // The assistant message of the turn has none.
    assert_eq!(reply.json["items"][1]["attachments"], json!([]));
    assert_eq!(assistant, msg_id(2));
}

#[tokio::test]
async fn soft_deleted_attachments_are_not_listed() {
    let env = ChatEnv::new().await;
    let (chat, user, _) = chat_with_turn(&env).await;
    let kept = env
        .seed_attachment(chat, 1, "document", "ready", None)
        .await;
    let removed = env
        .seed_attachment(chat, 2, "document", "ready", None)
        .await;
    env.link_attachment(chat, user, kept, 1).await;
    env.link_attachment(chat, user, removed, 2).await;
    env.db.soft_delete("attachments", removed).await;

    let reply = env.list_messages(chat, "").await;

    let attachments = reply.json["items"][0]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1, "{}", reply.text);
    assert_eq!(attachments[0]["attachment_id"], kept.to_string());
}

#[tokio::test]
async fn a_message_whose_only_attachment_is_deleted_has_an_empty_array() {
    let env = ChatEnv::new().await;
    let (chat, user, _) = chat_with_turn(&env).await;
    let removed = env
        .seed_attachment(chat, 1, "document", "ready", None)
        .await;
    env.link_attachment(chat, user, removed, 1).await;
    env.db.soft_delete("attachments", removed).await;

    let reply = env.list_messages(chat, "").await;

    assert_eq!(reply.json["items"][0]["attachments"], json!([]));
}

#[tokio::test]
async fn img_thumbnail_is_present_only_for_ready_images_with_stored_bytes() {
    let env = ChatEnv::new().await;
    let (chat, user, _) = chat_with_turn(&env).await;
    // Bytes whose standard base64 differs from the URL-safe one.
    let bytes: Vec<u8> = vec![0xfb, 0xff, 0xfe, 0x01, 0x02];
    let thumb = |n: u128, kind: &str, status: &str, with_bytes: bool| {
        let mut row = attachment_row(att_id(n), USER_TENANT, chat)
            .set("filename", format!("pic-{n}.png"))
            .set("attachment_kind", kind)
            .set("status", status)
            .set("created_at", ts(0))
            .set("updated_at", ts(0));
        if with_bytes {
            row = row
                .set("img_thumbnail", bytes.clone())
                .set("img_thumbnail_width", 64_i32)
                .set("img_thumbnail_height", 48_i32);
        }
        row
    };
    seed_attachment_row(&env, thumb(1, "image", "ready", true)).await;
    seed_attachment_row(&env, thumb(2, "image", "pending", true)).await;
    seed_attachment_row(&env, thumb(3, "image", "failed", true)).await;
    seed_attachment_row(&env, thumb(4, "document", "ready", true)).await;
    seed_attachment_row(&env, thumb(5, "image", "ready", false)).await;
    for n in 1..=5_u128 {
        env.link_attachment(chat, user, att_id(n), i64::try_from(n).unwrap())
            .await;
    }

    let reply = env.list_messages(chat, "").await;

    let attachments = reply.json["items"][0]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 5, "{}", reply.text);
    assert_eq!(
        attachments[0]["img_thumbnail"],
        json!({
            "content_type": "image/webp",
            "width": 64,
            "height": 48,
            "data_base64": standard_base64(&bytes)
        }),
        "{}",
        reply.text
    );
    assert_eq!(
        keys(&attachments[0]),
        key_set(&[
            "attachment_id",
            "kind",
            "filename",
            "status",
            "img_thumbnail"
        ])
    );
    for (index, label) in [
        (1, "pending image"),
        (2, "failed image"),
        (3, "ready document"),
        (4, "ready image without stored bytes"),
    ] {
        assert!(
            attachments[index].get("img_thumbnail").is_none(),
            "{label} has no img_thumbnail key (omitted, not null): {}",
            reply.text
        );
    }
}

#[tokio::test]
async fn attachments_of_one_message_do_not_leak_into_another() {
    let env = ChatEnv::new().await;
    let (chat, user, assistant) = chat_with_turn(&env).await;
    let second_user = seed(
        &env,
        chat,
        3,
        msg(chat, 3, "user", 3).set("request_id", request_id(3)),
    )
    .await;
    let first = env
        .seed_attachment(chat, 1, "document", "ready", None)
        .await;
    let second = env
        .seed_attachment(chat, 2, "document", "ready", None)
        .await;
    env.link_attachment(chat, user, first, 1).await;
    env.link_attachment(chat, second_user, second, 1).await;

    let reply = env.list_messages(chat, "").await;

    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(items[0]["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(
        items[0]["attachments"][0]["attachment_id"],
        first.to_string()
    );
    assert_eq!(items[1]["id"], assistant.to_string());
    assert_eq!(items[1]["attachments"], json!([]));
    assert_eq!(
        items[2]["attachments"][0]["attachment_id"],
        second.to_string()
    );
}

// ================================================================================================
// my_reaction
// ================================================================================================

#[tokio::test]
async fn my_reaction_is_always_present_and_reflects_only_the_callers_own_reaction() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    let liked = seed(&env, chat, 1, msg(chat, 1, "assistant", 1)).await;
    let disliked = seed(&env, chat, 2, msg(chat, 2, "assistant", 2)).await;
    let untouched = seed(&env, chat, 3, msg(chat, 3, "assistant", 3)).await;
    let only_others = seed(&env, chat, 4, msg(chat, 4, "assistant", 4)).await;
    env.seed_reaction(1, liked, USER, "like").await;
    env.seed_reaction(2, disliked, USER, "dislike").await;
    env.seed_reaction(3, liked, OTHER_USER, "dislike").await;
    env.seed_reaction(4, only_others, OTHER_USER, "like").await;
    assert_ne!(untouched, only_others);

    let reply = env.list_messages(chat, "").await;

    let items = reply.json["items"].as_array().unwrap();
    for item in items {
        assert!(item.get("my_reaction").is_some(), "key always present");
    }
    assert_eq!(items[0]["my_reaction"], "like");
    assert_eq!(items[1]["my_reaction"], "dislike");
    assert_eq!(items[2]["my_reaction"], Value::Null);
    assert_eq!(items[3]["my_reaction"], Value::Null);
}

#[tokio::test]
async fn user_and_system_messages_always_have_a_null_my_reaction() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 10).await;
    let user = seed(&env, chat, 1, msg(chat, 1, "user", 1)).await;
    let system = seed(&env, chat, 2, msg(chat, 2, "system", 2)).await;
    // Rows that the API cannot create, seeded to prove the role guard.
    env.seed_reaction(1, user, USER, "like").await;
    env.seed_reaction(2, system, USER, "dislike").await;

    let reply = env.list_messages(chat, "").await;

    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[0]["my_reaction"], Value::Null);
    assert_eq!(items[1]["role"], "system");
    assert_eq!(items[1]["my_reaction"], Value::Null);
    assert_eq!(items[1]["attachments"], json!([]));
}

// ================================================================================================
// Pagination and OData
// ================================================================================================

#[tokio::test]
async fn the_default_limit_is_20_and_a_larger_limit_is_clamped_to_100() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("Long"), 10).await;
    for n in 1..=101_u128 {
        seed(
            &env,
            chat,
            n,
            msg(chat, n, "user", i64::try_from(n).unwrap()),
        )
        .await;
    }

    let default = env.list_messages(chat, "").await;
    let clamped = env.list_messages(chat, "?limit=500").await;
    let exact = env.list_messages(chat, "?limit=100").await;

    assert_eq!(default.json["items"].as_array().unwrap().len(), 20);
    assert_eq!(default.json["page_info"]["limit"], 20);
    assert!(next_cursor(&default).is_some());
    assert_eq!(clamped.status, StatusCode::OK, "{}", clamped.text);
    assert_eq!(clamped.json["items"].as_array().unwrap().len(), 100);
    assert_eq!(clamped.json["page_info"]["limit"], 100);
    assert_eq!(exact.json["page_info"]["limit"], 100);
}

#[tokio::test]
async fn limit_zero_is_400_invalid_limit() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    let reply = env.list_messages(chat, "?limit=0").await;

    reply.assert_odata_error("INVALID_LIMIT");
}

#[tokio::test]
async fn walking_next_cursor_returns_every_message_once_in_order_including_timestamp_ties() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("Walk"), 10).await;
    // 7 messages; pairs share a timestamp.
    for n in 1..=7_u128 {
        let created = i64::try_from(n.div_ceil(2)).unwrap();
        seed(&env, chat, n, msg(chat, n, "user", created)).await;
    }
    let expected: Vec<String> = (1..=7).map(|n| msg_id(n).to_string()).collect();

    let mut walked = Vec::new();
    let mut sizes = Vec::new();
    let mut query = "?limit=3".to_owned();
    loop {
        let page = env.list_messages(chat, &query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text);
        sizes.push(page.json["items"].as_array().unwrap().len());
        walked.extend(ids(&page));
        match next_cursor(&page) {
            Some(cursor) => query = format!("?limit=3&cursor={}", enc(&cursor)),
            None => break,
        }
    }

    assert_eq!(sizes, vec![3, 3, 1]);
    assert_eq!(walked, expected);
}

#[tokio::test]
async fn the_prev_cursor_of_a_later_page_leads_back_to_the_previous_page() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("Walk"), 10).await;
    for n in 1..=6_u128 {
        seed(
            &env,
            chat,
            n,
            msg(chat, n, "user", i64::try_from(n).unwrap()),
        )
        .await;
    }
    let first = env.list_messages(chat, "?limit=2").await;
    let cursor = next_cursor(&first).unwrap();
    let second = env
        .list_messages(chat, &format!("?limit=2&cursor={}", enc(&cursor)))
        .await;
    let prev = second.json["page_info"]["prev_cursor"]
        .as_str()
        .expect("a prev_cursor")
        .to_owned();

    let back = env
        .list_messages(chat, &format!("?limit=2&cursor={}", enc(&prev)))
        .await;

    assert_eq!(back.status, StatusCode::OK, "{}", back.text);
    assert_eq!(ids(&back), ids(&first));
}

#[tokio::test]
async fn filters_over_role_id_and_created_at_narrow_the_page() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("History"), 50).await;
    let user = seed(&env, chat, 1, msg(chat, 1, "user", 10)).await;
    let assistant = seed(&env, chat, 2, msg(chat, 2, "assistant", 20)).await;
    let system = seed(&env, chat, 3, msg(chat, 3, "system", 30)).await;
    let list = |filter: String| {
        let env = &env;
        async move {
            env.list_messages(chat, &format!("?$filter={}", enc(&filter)))
                .await
        }
    };

    let by_role = list("role eq 'assistant'".to_owned()).await;
    let by_other_role = list("role ne 'user'".to_owned()).await;
    let by_id = list(format!("id eq {user}")).await;
    let by_ids = list(format!("id in ({user},{system})")).await;
    let later = list(format!("created_at gt {}", rfc3339(15))).await;
    let window = list(format!(
        "created_at ge {} and created_at lt {}",
        rfc3339(15),
        rfc3339(25)
    ))
    .await;

    for reply in [&by_role, &by_other_role, &by_id, &by_ids, &later, &window] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(ids(&by_role), vec![assistant.to_string()]);
    assert_eq!(
        ids(&by_other_role),
        vec![assistant.to_string(), system.to_string()]
    );
    assert_eq!(ids(&by_id), vec![user.to_string()]);
    assert_eq!(ids(&by_ids), vec![user.to_string(), system.to_string()]);
    assert_eq!(ids(&later), vec![assistant.to_string(), system.to_string()]);
    assert_eq!(ids(&window), vec![assistant.to_string()]);
}

#[tokio::test]
async fn orderby_over_created_at_id_and_role_with_the_id_tiebreaker() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("Order"), 10).await;
    let first = seed(&env, chat, 1, msg(chat, 1, "user", 10)).await;
    let second = seed(&env, chat, 2, msg(chat, 2, "assistant", 20)).await;
    let third = seed(&env, chat, 3, msg(chat, 3, "user", 30)).await;
    let fourth = seed(&env, chat, 4, msg(chat, 4, "assistant", 40)).await;
    let s = |id: Uuid| id.to_string();
    let order = |expr: &str| {
        let env = &env;
        let query = format!("?$orderby={}", enc(expr));
        async move { env.list_messages(chat, &query).await }
    };

    let created_desc = order("created_at desc").await;
    let role_asc = order("role asc").await;
    let role_desc = order("role desc").await;
    let id_desc = order("id desc").await;
    let two_keys = order("role asc,created_at desc").await;

    assert_eq!(
        ids(&created_desc),
        vec![s(fourth), s(third), s(second), s(first)]
    );
    // role asc: assistant (b, d by id asc), then user (a, c by id asc).
    assert_eq!(
        ids(&role_asc),
        vec![s(second), s(fourth), s(first), s(third)]
    );
    // role desc: user (c, a by id desc), then assistant (d, b by id desc).
    assert_eq!(
        ids(&role_desc),
        vec![s(third), s(first), s(fourth), s(second)]
    );
    assert_eq!(
        ids(&id_desc),
        vec![s(fourth), s(third), s(second), s(first)]
    );
    assert_eq!(
        ids(&two_keys),
        vec![s(fourth), s(second), s(third), s(first)]
    );
}

#[tokio::test]
async fn fields_outside_created_at_id_and_role_are_rejected() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    for filter in [
        "content eq 'x'",
        "contains(content,'x')",
        "chat_id eq 00000000-0000-0000-0000-000000000001",
        "tenant_id eq 00000000-0000-0000-0000-000000000001",
        "request_id eq 00000000-0000-0000-0000-000000000001",
        "model eq 'gpt-5.2'",
        "input_tokens gt 1",
        "my_reaction eq 'like'",
        "deleted_at eq null",
    ] {
        let reply = env
            .list_messages(chat, &format!("?$filter={}", enc(filter)))
            .await;

        reply.assert_odata_error("INVALID_FILTER");
    }
    for field in [
        "content",
        "chat_id",
        "tenant_id",
        "request_id",
        "model",
        "input_tokens",
        "deleted_at",
    ] {
        let reply = env
            .list_messages(chat, &format!("?$orderby={}", enc(&format!("{field} asc"))))
            .await;

        reply.assert_odata_error("INVALID_ORDERBY_FIELD");
    }
}

#[tokio::test]
async fn malformed_odata_input_is_rejected_with_the_odata_resource_type() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;
    let first = env.list_messages(chat, "?limit=1").await;
    let cursor = next_cursor(&first).unwrap();
    let filtered = env
        .list_messages(
            chat,
            &format!("?limit=1&$filter={}", enc("role ne 'system'")),
        )
        .await;
    let filtered_cursor = next_cursor(&filtered).unwrap();

    let bad_filter = env
        .list_messages(chat, &format!("?$filter={}", enc("role eq")))
        .await;
    let bad_cursor = env.list_messages(chat, "?cursor=garbage").await;
    let both = env
        .list_messages(
            chat,
            &format!("?cursor={}&$orderby={}", enc(&cursor), enc("role asc")),
        )
        .await;
    let skip = env.list_messages(chat, "?$skip=1").await;
    let count = env.list_messages(chat, "?$count=true").await;
    let select = env.list_messages(chat, "?$select=").await;
    let too_long = env
        .list_messages(
            chat,
            &format!(
                "?$filter={}",
                enc(&format!("role eq '{}'", "x".repeat(9000)))
            ),
        )
        .await;
    let other_filter = env
        .list_messages(
            chat,
            &format!(
                "?$filter={}&cursor={}",
                enc("role eq 'user'"),
                enc(&filtered_cursor)
            ),
        )
        .await;

    bad_filter.assert_odata_error("INVALID_FILTER");
    bad_cursor.assert_odata_error("INVALID_CURSOR");
    both.assert_odata_error("ORDER_WITH_CURSOR");
    skip.assert_odata_error("UNSUPPORTED_QUERY_PARAM");
    count.assert_odata_error("UNSUPPORTED_QUERY_PARAM");
    select.assert_odata_error("INVALID_SELECT");
    too_long.assert_odata_error("FILTER_TOO_LONG");
    other_filter.assert_odata_error("FILTER_MISMATCH");
    assert_eq!(bad_filter.resource_type(), ODATA_TYPE);
}

#[tokio::test]
async fn a_cursor_of_the_chat_list_is_not_accepted_by_the_message_list() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;
    env.seed_chat(2, Some("second"), 20).await;
    let chats = env.list_chats("?limit=1").await;
    let chat_cursor = next_cursor(&chats).expect("a chat cursor");

    let reply = env
        .list_messages(chat, &format!("?cursor={}", enc(&chat_cursor)))
        .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.resource_type(), ODATA_TYPE);
}

#[tokio::test]
async fn select_is_accepted_and_the_full_message_is_returned() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    let reply = env.list_messages(chat, "?$select=content").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json["items"][0]), key_set(&REQUIRED_KEYS));
}

#[tokio::test]
async fn a_message_of_another_chat_is_not_reachable_through_filters() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;
    let other = env.seed_chat(2, Some("Other"), 10).await;
    let foreign = seed(&env, other, 9, msg(other, 9, "user", 1)).await;

    let reply = env
        .list_messages(
            chat,
            &format!("?$filter={}", enc(&format!("id eq {foreign}"))),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["items"], json!([]));
    assert!(!reply.text.contains("content-9"));
}

// ================================================================================================
// 404 and PEP
// ================================================================================================

#[tokio::test]
async fn an_unknown_deleted_or_foreign_chat_is_404_with_the_chat_resource_type() {
    let env = ChatEnv::new().await;
    let deleted = env.seed_chat(1, Some("gone"), 10).await;
    seed(&env, deleted, 1, msg(deleted, 1, "user", 1)).await;
    env.db.soft_delete("chats", deleted).await;
    let foreign_user = env
        .seed_chat_of(2, USER_TENANT, OTHER_USER, Some("theirs"), 10)
        .await;
    let theirs = seed(&env, foreign_user, 2, msg(foreign_user, 2, "user", 1)).await;
    let foreign_tenant = env
        .seed_chat_of(3, OTHER_TENANT, USER, Some("other tenant"), 10)
        .await;
    assert_ne!(theirs, msg_id(99));

    for id in [chat_id(99), deleted, foreign_user, foreign_tenant] {
        let reply = env.list_messages(id, "").await;

        reply.assert_not_found(CHAT_TYPE);
        assert!(!reply.text.contains("content-"), "{}", reply.text);
    }
}

#[tokio::test]
async fn the_owner_of_a_chat_is_the_only_one_who_can_list_its_messages() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    let same_tenant_other_user = env
        .get_as(&other_user_ctx(), &format!("{}/messages", chat_uri(chat)))
        .await;
    let other_tenant = env
        .get_as(&other_tenant_ctx(), &format!("{}/messages", chat_uri(chat)))
        .await;
    let owner = env.list_messages(chat, "").await;

    same_tenant_other_user.assert_not_found(CHAT_TYPE);
    other_tenant.assert_not_found(CHAT_TYPE);
    assert_eq!(owner.status, StatusCode::OK, "{}", owner.text);
}

#[tokio::test]
async fn listing_messages_asks_the_pdp_for_list_messages_on_the_chat() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;

    env.list_messages(chat, "").await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "list_messages");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, Some(chat));
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn listing_messages_does_not_change_anything() {
    let env = ChatEnv::new().await;
    let (chat, _, _) = chat_with_turn(&env).await;
    let before = (
        env.chat_snapshot(chat).await,
        env.dump("messages").await,
        env.dump("message_reactions").await,
    );

    let reply = env.list_messages(chat, "").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let after = (
        env.chat_snapshot(chat).await,
        env.dump("messages").await,
        env.dump("message_reactions").await,
    );
    assert_eq!(before, after);
}
