//! `GET /v1/chats/{id}/messages` over the real router, SQLite and the mock PDP: the page shape,
//! the always-present and omitted fields, attachments and reactions, the chronological order with
//! fractional-second timestamps, `$filter`, `$orderby`, `limit`, cursors, the `OData` error
//! reasons, owner isolation, admission and `message_count` across turns.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-list-messages`; algorithms `message-list-query`,
//! `message-projection`, `message-order`, `send-error-contract`; definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-messages-api` and
//! `cpt-cf-mini-chat-dod-message-streaming-message-order`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use base64::Engine;
use serde_json::Value;
use uuid::Uuid;

use super::chat_list_tests::enc;
use crate::test_support::chats::{
    a1, a2, assert_chat_not_found, assert_odata_error, assert_problem, at, b1, chat_row, chat_uri,
    item_ids, keys, parse_ts, soft_delete_message, violation,
};
use crate::test_support::quota::STANDARD;
use crate::test_support::send::{
    AttachmentSeed, MessageSeed, SendRig, footprint, link_att, messages_uri, seed_att,
    seed_completed_turn, seed_msg, seed_reaction_of, send_rig, t,
};
use crate::test_support::{Pdp, TENANT_A, USER_A1, USER_A2};

/// The offsets (µs) of the fractional fixture, chronologically: the stored texts are `…:20Z`,
/// `…:20.12Z`, `…:20.123Z`, `…:20.1234Z`, `…:20.123456Z`, `…:20.5Z`, `…:20.999999Z`, `…:21Z`.
const CHRONOLOGICAL: [i64; 8] = [
    0, 120_000, 123_000, 123_400, 123_456, 500_000, 999_999, 1_000_000,
];

async fn list(rig: &SendRig, chat_id: Uuid, query: &str) -> crate::test_support::chats::Reply {
    let uri = if query.is_empty() {
        messages_uri(chat_id)
    } else {
        format!("{}?{query}", messages_uri(chat_id))
    };
    rig.get(&uri, &a1()).await
}

fn contents(body: &Value) -> Vec<String> {
    body["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no items in {body}"))
        .iter()
        .map(|item| item["content"].as_str().unwrap().to_owned())
        .collect()
}

fn offsets(micros: &[i64]) -> Vec<String> {
    micros.iter().map(ToString::to_string).collect()
}

async fn fractional_chat(rig: &SendRig) -> crate::infra::storage::repo_impl::ChatRecord {
    let chat = rig.std_chat().await;
    for micros in [
        500_000, 0, 123_456, 1_000_000, 120_000, 999_999, 123_400, 123_000,
    ] {
        seed_msg(
            &rig.quota.db,
            &chat,
            MessageSeed::new("user", &micros.to_string(), t(micros)),
        )
        .await;
    }
    chat
}

// ── the page ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_page_carries_items_and_page_info_with_null_cursors_when_there_is_no_other_page() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_completed_turn(&rig.quota.db, &chat, "question", "answer", t(10)).await;
    let reply = list(&rig, chat.id, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("application/json"),
        "{}",
        reply.content_type
    );
    let body = reply.json();
    assert_eq!(keys(&body), BTreeSet::from(["items", "page_info"]));
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        keys(&body["page_info"]),
        BTreeSet::from(["limit", "next_cursor", "prev_cursor"])
    );
    assert_eq!(body["page_info"]["limit"], 20);
    assert!(body["page_info"]["next_cursor"].is_null());
    assert!(body["page_info"]["prev_cursor"].is_null());
}

#[tokio::test]
async fn a_chat_without_messages_returns_an_empty_items_array() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let reply = list(&rig, chat.id, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["items"], serde_json::json!([]));
    assert_eq!(body["page_info"]["limit"], 20);
}

#[tokio::test]
async fn every_item_carries_the_required_fields_and_attachments_and_my_reaction_are_always_present()
{
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, assistant) =
        seed_completed_turn(&rig.quota.db, &chat, "question", "answer", t(10)).await;
    let reply = list(&rig, chat.id, "").await;
    let body = reply.json();
    let user = &body["items"][0];
    let assistant_item = &body["items"][1];

    assert_eq!(
        keys(user),
        BTreeSet::from([
            "id",
            "request_id",
            "role",
            "content",
            "attachments",
            "my_reaction",
            "created_at"
        ])
    );
    assert_eq!(user["role"], "user");
    assert_eq!(user["content"], "question");
    assert_eq!(user["request_id"], request_id.to_string());
    assert_eq!(user["attachments"], serde_json::json!([]));
    assert!(user["my_reaction"].is_null());
    assert!(
        reply.text.contains("\"my_reaction\":null"),
        "{}",
        reply.text
    );
    assert_eq!(parse_ts(&user["created_at"]), t(10));

    assert_eq!(assistant_item["id"], assistant.to_string());
    assert_eq!(assistant_item["role"], "assistant");
    assert_eq!(assistant_item["request_id"], request_id.to_string());
    assert_eq!(assistant_item["attachments"], serde_json::json!([]));
    assert!(assistant_item["my_reaction"].is_null());
    assert_eq!(assistant_item["model"], STANDARD);
    assert_eq!(assistant_item["input_tokens"], 120);
    assert_eq!(assistant_item["output_tokens"], 30);
}

#[tokio::test]
async fn model_and_token_counts_are_omitted_when_absent_or_zero_never_null() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "no usage", t(1)),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "output only", t(2))
            .model("m")
            .tokens(0, 9),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "plain", t(3)),
    )
    .await;
    let body = list(&rig, chat.id, "").await.json();
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        keys(&items[0]),
        BTreeSet::from([
            "id",
            "request_id",
            "role",
            "content",
            "attachments",
            "my_reaction",
            "created_at"
        ])
    );
    assert!(items[1].get("input_tokens").is_none());
    assert_eq!(items[1]["output_tokens"], 9);
    assert_eq!(items[1]["model"], "m");
    assert!(items[2].get("model").is_none());
}

#[tokio::test]
async fn the_items_never_expose_internal_or_provider_fields() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, assistant) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let image = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").thumbnail(8, 8, &[1]),
    )
    .await;
    link_att(&rig.quota.db, &chat, assistant, image).await;
    let reply = list(&rig, chat.id, "").await;
    for forbidden in [
        "tenant_id",
        "chat_id",
        "deleted_at",
        "is_compressed",
        "provider_response_id",
        "token_estimate",
        "cache_read",
        "cache_write",
        "reasoning",
        "provider_file_id",
        "file-",
        "content_type\":\"text",
        "request_kind",
        "features_used",
    ] {
        assert!(
            !reply.text.contains(forbidden),
            "`{forbidden}` in {}",
            reply.text
        );
    }
}

// ── attachments and reactions ────────────────────────────────────────────────

#[tokio::test]
async fn attachments_list_only_live_rows_and_a_thumbnail_only_for_a_ready_image() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let message = seed_msg(&rig.quota.db, &chat, MessageSeed::new("user", "see", t(1))).await;
    let thumbnail = [0x52, 0x49, 0x46, 0x46];
    let ready_image = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image")
            .filename("cat.png")
            .thumbnail(64, 48, &thumbnail),
    )
    .await;
    let document = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .filename("a.pdf")
            .thumbnail(1, 1, &[1]),
    )
    .await;
    let uploading_image = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image")
            .status("uploaded")
            .thumbnail(1, 1, &[1]),
    )
    .await;
    let bare_image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let deleted = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("image").deleted(t(5)),
    )
    .await;
    for attachment in [ready_image, document, uploading_image, bare_image, deleted] {
        link_att(&rig.quota.db, &chat, message, attachment).await;
    }
    let body = list(&rig, chat.id, "").await.json();
    let attachments = body["items"][0]["attachments"].as_array().unwrap();
    assert_eq!(
        attachments.len(),
        4,
        "the soft-deleted attachment is not listed: {body}"
    );
    let by_id = |id: Uuid| {
        attachments
            .iter()
            .find(|a| a["attachment_id"] == id.to_string())
            .unwrap_or_else(|| panic!("{id} missing in {body}"))
    };
    let shown = by_id(ready_image);
    assert_eq!(shown["kind"], "image");
    assert_eq!(shown["filename"], "cat.png");
    assert_eq!(shown["status"], "ready");
    let thumb = &shown["img_thumbnail"];
    assert_eq!(
        keys(thumb),
        BTreeSet::from(["content_type", "width", "height", "data_base64"])
    );
    assert_eq!(
        (thumb["width"].as_i64(), thumb["height"].as_i64()),
        (Some(64), Some(48))
    );
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(thumb["data_base64"].as_str().unwrap())
            .unwrap(),
        thumbnail
    );
    assert_eq!(
        keys(by_id(document)),
        BTreeSet::from(["attachment_id", "kind", "filename", "status"])
    );
    assert_eq!(by_id(document)["kind"], "document");
    assert!(by_id(uploading_image).get("img_thumbnail").is_none());
    assert!(by_id(bare_image).get("img_thumbnail").is_none());
}

#[tokio::test]
async fn assistant_messages_always_list_no_attachments() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, assistant) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let document = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("document")).await;
    link_att(&rig.quota.db, &chat, assistant, document).await;
    let body = list(&rig, chat.id, "").await.json();
    assert_eq!(body["items"][1]["attachments"], serde_json::json!([]));
}

#[tokio::test]
async fn my_reaction_is_the_callers_own_on_assistant_messages_and_null_everywhere_else() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let user_msg = seed_msg(&rig.quota.db, &chat, MessageSeed::new("user", "q", t(1))).await;
    let liked = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "liked", t(2)),
    )
    .await;
    let disliked = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "disliked", t(3)),
    )
    .await;
    let others = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "others", t(4)),
    )
    .await;
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A1, liked, "like").await;
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A1, disliked, "dislike").await;
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A2, others, "like").await;
    seed_reaction_of(&rig.quota.db, TENANT_A, USER_A1, user_msg, "like").await;
    let body = list(&rig, chat.id, "").await.json();
    let reactions: Vec<Value> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["my_reaction"].clone())
        .collect();
    assert_eq!(
        reactions,
        vec![
            Value::Null,
            Value::from("like"),
            Value::from("dislike"),
            Value::Null
        ]
    );
}

// ── order ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_default_and_explicit_created_at_orders_are_chronological_with_fractional_seconds() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    assert_eq!(
        contents(&list(&rig, chat.id, "").await.json()),
        offsets(&CHRONOLOGICAL)
    );
    for (orderby, expected) in [
        ("created_at asc", offsets(&CHRONOLOGICAL)),
        ("created_at desc", offsets(&newest_first)),
    ] {
        let reply = list(&rig, chat.id, &format!("$orderby={}", enc(orderby))).await;
        assert_eq!(reply.status, StatusCode::OK, "{orderby}: {}", reply.text);
        assert_eq!(contents(&reply.json()), expected, "{orderby}");
    }
}

#[tokio::test]
async fn every_order_is_total_equal_values_fall_back_to_the_id_in_the_same_direction() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let mut seeded = Vec::new();
    for role in [
        "user",
        "assistant",
        "user",
        "assistant",
        "user",
        "assistant",
    ] {
        seeded.push(seed_msg(&rig.quota.db, &chat, MessageSeed::new(role, "same", t(7))).await);
    }
    seeded.sort();
    let mut reversed = seeded.clone();
    reversed.reverse();
    let cases: Vec<(Option<&str>, Vec<Uuid>)> = vec![
        (None, seeded.clone()),
        (Some("created_at asc"), seeded.clone()),
        (Some("created_at desc"), reversed.clone()),
        (Some("id asc"), seeded.clone()),
        (Some("id desc"), reversed.clone()),
    ];
    for (orderby, expected) in cases {
        let query = orderby.map_or_else(String::new, |o| format!("$orderby={}", enc(o)));
        let reply = list(&rig, chat.id, &query).await;
        assert_eq!(reply.status, StatusCode::OK, "{orderby:?}: {}", reply.text);
        assert_eq!(item_ids(&reply.json()), expected, "{orderby:?}");
    }
    let by_role = list(&rig, chat.id, &format!("$orderby={}", enc("role desc")))
        .await
        .json();
    let roles: Vec<&str> = by_role["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec![
            "user",
            "user",
            "user",
            "assistant",
            "assistant",
            "assistant"
        ]
    );
}

// ── filters ──────────────────────────────────────────────────────────────────

fn literal(micros: i64) -> String {
    t(micros)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[tokio::test]
async fn created_at_filters_compare_chronologically_not_as_text() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let at_123 = literal(123_000);
    let cases: Vec<(String, Vec<i64>)> = vec![
        (
            format!("created_at ge {at_123}"),
            CHRONOLOGICAL[2..].to_vec(),
        ),
        (
            format!("created_at gt {at_123}"),
            CHRONOLOGICAL[3..].to_vec(),
        ),
        (
            format!("created_at le {at_123}"),
            CHRONOLOGICAL[..=2].to_vec(),
        ),
        (
            format!("created_at lt {at_123}"),
            CHRONOLOGICAL[..2].to_vec(),
        ),
        (format!("created_at eq {at_123}"), vec![123_000]),
        (
            format!("created_at in ({}, {})", literal(0), literal(999_999)),
            vec![0, 999_999],
        ),
    ];
    for (filter, expected) in cases {
        let reply = list(&rig, chat.id, &format!("$filter={}", enc(&filter))).await;
        assert_eq!(reply.status, StatusCode::OK, "{filter}: {}", reply.text);
        assert_eq!(contents(&reply.json()), offsets(&expected), "{filter}");
    }
}

#[tokio::test]
async fn role_and_id_filters_select_the_expected_messages() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (_, assistant) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    seed_msg(&rig.quota.db, &chat, MessageSeed::new("system", "s", t(30))).await;
    let cases: Vec<(String, Vec<&str>)> = vec![
        ("role eq 'user'".to_owned(), vec!["q"]),
        ("role ne 'user'".to_owned(), vec!["a", "s"]),
        ("contains(role, 'assist')".to_owned(), vec!["a"]),
        ("startswith(role, 'sys')".to_owned(), vec!["s"]),
        ("endswith(role, 'ant')".to_owned(), vec!["a"]),
        ("role in ('user', 'system')".to_owned(), vec!["q", "s"]),
        (format!("id eq {assistant}"), vec!["a"]),
    ];
    for (filter, expected) in cases {
        let reply = list(&rig, chat.id, &format!("$filter={}", enc(&filter))).await;
        assert_eq!(reply.status, StatusCode::OK, "{filter}: {}", reply.text);
        assert_eq!(contents(&reply.json()), expected, "{filter}");
    }
}

#[tokio::test]
async fn deleted_messages_are_hidden_and_compressed_messages_stay_listed() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_msg(&rig.quota.db, &chat, MessageSeed::new("user", "live", t(1))).await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "compressed", t(2)).compressed(),
    )
    .await;
    let gone = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "deleted", t(3)),
    )
    .await;
    soft_delete_message(&rig.quota.db, gone).await;
    let other = rig.std_chat().await;
    seed_msg(
        &rig.quota.db,
        &other,
        MessageSeed::new("user", "elsewhere", t(4)),
    )
    .await;
    assert_eq!(
        contents(&list(&rig, chat.id, "").await.json()),
        vec!["live", "compressed"]
    );
}

// ── limit and cursors ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_limit_defaults_to_20_clamps_to_100_and_zero_is_rejected() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    for i in 0..105 {
        seed_msg(
            &rig.quota.db,
            &chat,
            MessageSeed::new("user", &format!("{i:03}"), t(i)),
        )
        .await;
    }
    let default = list(&rig, chat.id, "").await.json();
    assert_eq!(default["items"].as_array().unwrap().len(), 20);
    assert_eq!(default["page_info"]["limit"], 20);
    assert!(default["page_info"]["next_cursor"].is_string());
    let clamped = list(&rig, chat.id, "limit=5000").await.json();
    assert_eq!(clamped["items"].as_array().unwrap().len(), 100);
    assert_eq!(clamped["page_info"]["limit"], 100);
    let three = list(&rig, chat.id, "limit=3").await.json();
    assert_eq!(three["items"].as_array().unwrap().len(), 3);
    assert_eq!(three["page_info"]["limit"], 3);

    let zero = list(&rig, chat.id, "limit=0").await;
    assert_odata_error(&zero, "$top", "INVALID_LIMIT");
}

async fn follow(rig: &SendRig, chat_id: Uuid, limit: u32, extra: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut query = format!("limit={limit}{extra}");
    for _ in 0..50 {
        let reply = list(rig, chat_id, &query).await;
        assert_eq!(reply.status, StatusCode::OK, "{query}: {}", reply.text);
        let body = reply.json();
        out.extend(contents(&body));
        match body["page_info"]["next_cursor"].as_str() {
            Some(cursor) => query = format!("limit={limit}&cursor={}{extra}", enc(cursor)),
            None => return out,
        }
    }
    panic!("the cursor never ended");
}

#[tokio::test]
async fn cursor_pages_have_no_gaps_or_repeats_chronologically_and_in_reverse() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    assert_eq!(follow(&rig, chat.id, 3, "").await, offsets(&CHRONOLOGICAL));
    let mut newest_first = CHRONOLOGICAL;
    newest_first.reverse();
    let desc = format!("&$orderby={}", enc("created_at desc"));
    // a cursor carries its own order, so only the first request names it
    let first = list(&rig, chat.id, &format!("limit=3{desc}")).await.json();
    let mut seen = contents(&first);
    let mut cursor = first["page_info"]["next_cursor"]
        .as_str()
        .map(ToOwned::to_owned);
    while let Some(token) = cursor {
        let body = list(&rig, chat.id, &format!("limit=3&cursor={}", enc(&token)))
            .await
            .json();
        seen.extend(contents(&body));
        cursor = body["page_info"]["next_cursor"]
            .as_str()
            .map(ToOwned::to_owned);
    }
    assert_eq!(seen, offsets(&newest_first));
}

#[tokio::test]
async fn cursor_pages_hold_across_equal_created_at_values() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let mut pairs = Vec::new();
    for i in 0..7 {
        let id = seed_msg(
            &rig.quota.db,
            &chat,
            MessageSeed::new("user", &format!("m{i}"), t(5)),
        )
        .await;
        pairs.push((id, format!("m{i}")));
    }
    pairs.sort();
    let want: Vec<String> = pairs.into_iter().map(|(_, text)| text).collect();
    assert_eq!(follow(&rig, chat.id, 2, "").await, want);
}

#[tokio::test]
async fn the_cursor_serves_previous_page_navigation() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let first = list(&rig, chat.id, "limit=3").await.json();
    assert!(first["page_info"]["prev_cursor"].is_null());
    let second = list(
        &rig,
        chat.id,
        &format!(
            "limit=3&cursor={}",
            enc(first["page_info"]["next_cursor"].as_str().unwrap())
        ),
    )
    .await
    .json();
    assert_eq!(contents(&second), offsets(&CHRONOLOGICAL[3..6]));
    let prev = second["page_info"]["prev_cursor"]
        .as_str()
        .expect("a previous cursor");
    let back = list(&rig, chat.id, &format!("limit=3&cursor={}", enc(prev)))
        .await
        .json();
    assert_eq!(contents(&back), contents(&first));
}

#[tokio::test]
async fn a_cursor_continues_a_filtered_query_only_with_the_same_filter() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let filter = format!("created_at ge {}", literal(120_000));
    let first = list(&rig, chat.id, &format!("limit=2&$filter={}", enc(&filter)))
        .await
        .json();
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let same = list(
        &rig,
        chat.id,
        &format!("limit=2&cursor={}&$filter={}", enc(&cursor), enc(&filter)),
    )
    .await;
    assert_eq!(same.status, StatusCode::OK, "{}", same.text);
    for other in ["", &format!("&$filter={}", enc("role eq 'user'"))] {
        let reply = list(
            &rig,
            chat.id,
            &format!("limit=2&cursor={}{other}", enc(&cursor)),
        )
        .await;
        assert_odata_error(&reply, "cursor", "FILTER_MISMATCH");
    }
}

#[tokio::test]
async fn a_cursor_together_with_orderby_or_one_that_cannot_be_decoded_is_rejected() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let first = list(&rig, chat.id, "limit=2").await.json();
    let cursor = first["page_info"]["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = list(
        &rig,
        chat.id,
        &format!(
            "cursor={}&$orderby={}",
            enc(&cursor),
            enc("created_at desc")
        ),
    )
    .await;
    let body = assert_problem(&reply, 400, "invalid_argument");
    assert_eq!(violation(&body, "cursor")["reason"], "ORDER_WITH_CURSOR");
    for garbage in ["garbage", "!!!", "e30", "bm90IGpzb24"] {
        let reply = list(&rig, chat.id, &format!("cursor={}", enc(garbage))).await;
        assert_odata_error(&reply, "cursor", "INVALID_CURSOR");
    }
}

// ── query errors ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn malformed_queries_are_400_with_the_odata_resource_type_and_the_library_reason() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let cases: Vec<(String, &str, &str)> = vec![
        (
            format!("$filter={}", enc("content eq 'x'")),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            format!("$filter={}", enc("role gt 'a'")),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            format!("$filter={}", enc("contains(created_at, 'x')")),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            format!("$filter={}", enc("role eq")),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            format!("$orderby={}", enc("content asc")),
            "$orderby",
            "INVALID_ORDERBY_FIELD",
        ),
        (
            format!("$orderby={}", enc("model desc")),
            "$orderby",
            "INVALID_ORDERBY_FIELD",
        ),
        ("$skip=1".to_owned(), "$skip", "UNSUPPORTED_QUERY_PARAM"),
        (
            "$count=true".to_owned(),
            "$count",
            "UNSUPPORTED_QUERY_PARAM",
        ),
    ];
    for (query, field, reason) in cases {
        let reply = list(&rig, chat.id, &query).await;
        assert_odata_error(&reply, field, reason);
    }
    for query in ["limit=abc", "limit=-1", "limit=1.5", "limit="] {
        let reply = list(&rig, chat.id, query).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        assert_eq!(
            body["context"]["resource_type"], "gts.cf.core.odata.query.v1~",
            "{query}: {body}"
        );
    }
}

#[tokio::test]
async fn a_valid_select_is_accepted_and_ignored() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "hello", t(1)),
    )
    .await;
    let reply = list(&rig, chat.id, &format!("$select={}", enc("id"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json()["items"][0]["content"], "hello");
}

// ── the stored data guard ────────────────────────────────────────────────────

#[tokio::test]
async fn a_stored_null_request_id_is_a_generic_500_never_a_message_without_it() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_msg(&rig.quota.db, &chat, MessageSeed::new("user", "fine", t(1))).await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "legacy secret text", t(2)).request(None),
    )
    .await;
    let reply = list(&rig, chat.id, "").await;
    let body = assert_problem(&reply, 500, "internal");
    assert!(!body.to_string().contains("legacy secret text"), "{body}");
}

// ── isolation and admission ──────────────────────────────────────────────────

#[tokio::test]
async fn a_chat_that_is_not_visible_is_a_404_for_the_chat_type() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "private", t(1)),
    )
    .await;
    let foreign_user = rig.quota.chat(&a2(), STANDARD).await;
    let deleted = rig.std_chat().await;
    crate::test_support::raw_exec(
        &rig.quota.db.raw,
        &format!(
            "UPDATE chats SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            crate::test_support::chats::blob(deleted.id)
        ),
    )
    .await
    .unwrap();
    for (who, id) in [
        (a2(), chat.id),
        (b1(), chat.id),
        (a1(), foreign_user.id),
        (a1(), deleted.id),
        (a1(), Uuid::new_v4()),
    ] {
        let reply = rig.get(&messages_uri(id), &who).await;
        let body = assert_chat_not_found(&reply);
        assert!(!reply.text.contains("private"), "{body}");
    }
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_a_400_invalid_path_params_before_authorization() {
    let rig = send_rig().await;
    for id in ["not-a-uuid", "123"] {
        let reply = rig
            .get(&format!("/mini-chat/v1/chats/{id}/messages"), &a1())
            .await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        let violations = body["context"]["field_violations"].as_array().unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v["reason"] == "invalid_path_params"),
            "{body}"
        );
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn admission_evaluates_list_messages_with_the_chat_id_as_the_resource() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let reply = list(&rig, chat.id, "").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "list_messages");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_denied_or_failing_pdp_refuses_the_list() {
    let denied = crate::test_support::send::send_rig_with(
        crate::test_support::send::SendRigOptions::new().with_pdp(Pdp::Deny),
    )
    .await;
    let chat = denied.std_chat().await;
    let reply = list(&denied, chat.id, "").await;
    let body = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");

    let down = crate::test_support::send::send_rig_with(
        crate::test_support::send::SendRigOptions::new().with_pdp(Pdp::Unreachable),
    )
    .await;
    let chat = down.std_chat().await;
    let reply = list(&down, chat.id, "").await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
}

#[tokio::test]
async fn a_list_changes_nothing() {
    let rig = send_rig().await;
    let chat = fractional_chat(&rig).await;
    let before = footprint(&rig.quota.db, &chat).await;
    for query in ["", "limit=2", "$orderby=id desc"] {
        let encoded = query.replace(' ', "%20");
        assert_eq!(list(&rig, chat.id, &encoded).await.status, StatusCode::OK);
    }
    assert_eq!(footprint(&rig.quota.db, &chat).await, before);
    assert_eq!(
        chat_row(&rig.quota.db, chat.id).await.unwrap().updated_at,
        at(0)
    );
}

// ── message_count across turns ───────────────────────────────────────────────

async fn message_count(rig: &SendRig, chat_id: Uuid) -> i64 {
    rig.get(&chat_uri(chat_id), &a1()).await.json()["message_count"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn three_completed_turns_list_alternating_messages_in_send_order_and_count_six() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    // equal and differently precise timestamps across turns
    let mut expected = Vec::new();
    let mut request_ids = Vec::new();
    for (i, base) in [0_i64, 120_000, 123_000].into_iter().enumerate() {
        let (request_id, _) = seed_completed_turn(
            &rig.quota.db,
            &chat,
            &format!("q{i}"),
            &format!("a{i}"),
            t(base),
        )
        .await;
        expected.push(format!("q{i}"));
        expected.push(format!("a{i}"));
        request_ids.push(request_id);
    }
    let body = list(&rig, chat.id, "").await.json();
    assert_eq!(contents(&body), expected);
    let roles: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        vec![
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    for (pair, request_id) in body["items"]
        .as_array()
        .unwrap()
        .chunks(2)
        .zip(&request_ids)
    {
        assert_eq!(pair[0]["request_id"], request_id.to_string());
        assert_eq!(pair[1]["request_id"], request_id.to_string());
    }
    assert_eq!(message_count(&rig, chat.id).await, 6);
}

#[tokio::test]
async fn message_count_changes_only_by_what_the_turn_durably_wrote() {
    use crate::domain::streaming::{SendOutcome, ValidatedSend};
    use tokio_util::sync::CancellationToken;
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    assert_eq!(message_count(&rig, chat.id).await, 0);

    // a rejected send adds none
    rig.gateway.set_limits((1, 1), (1, 1));
    let rejected = rig
        .service
        .send(
            &a1(),
            chat.id,
            ValidatedSend {
                content: "hi".into(),
                request_id: None,
                attachment_ids: vec![],
                web_search_enabled: false,
            },
            &CancellationToken::new(),
        )
        .await;
    assert!(rejected.is_err());
    assert_eq!(message_count(&rig, chat.id).await, 0);
    rig.gateway
        .set_limits((60_000_000, 600_000_000), (22_000_000, 300_000_000));

    // a started turn adds the user message only
    let started = rig
        .service
        .send(
            &a1(),
            chat.id,
            ValidatedSend {
                content: "hi".into(),
                request_id: None,
                attachment_ids: vec![],
                web_search_enabled: false,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(started, SendOutcome::Streaming(_)));
    assert_eq!(message_count(&rig, chat.id).await, 1);

    // a replay adds none
    let other = rig.std_chat().await;
    let (replayed, _) = seed_completed_turn(&rig.quota.db, &other, "q", "a", t(1)).await;
    let before = message_count(&rig, other.id).await;
    assert_eq!(before, 2);
    let outcome = rig
        .service
        .send(
            &a1(),
            other.id,
            ValidatedSend {
                content: "q".into(),
                request_id: Some(replayed),
                attachment_ids: vec![],
                web_search_enabled: false,
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, SendOutcome::Replay(_)));
    assert_eq!(message_count(&rig, other.id).await, before);
}
