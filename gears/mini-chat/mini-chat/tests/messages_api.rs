//! `GET /v1/chats/{id}/messages`: the message list contract (identity,
//! attachments, reaction and optional fields), ordering, OData filtering,
//! cursor pagination and its error contract. Sending messages belongs to a
//! later slice, so messages, attachments and reactions are seeded directly in
//! the database.
//!
//! Source of truth: DESIGN 3.3 "List Messages" (+ `request_id` semantics and
//! the implementation guard for a NULL `request_id`), 3.7 `messages`,
//! `message_attachments`, `attachments`, `message_reactions`, the REST error
//! table, 3.8 (owner scoping; child queries use the tenant scope after the
//! owner-scoped chat lookup) and the OpenAPI schemas `MiniChatMessageDto` /
//! `Page_MiniChatMessageDto` / `AttachmentSummaryDto` / `ImgThumbnailDto` /
//! `MessageRoleDto` / `ReactionKindDto`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;

use base64::Engine as _;
use chat_support::{
    AttachmentSeed, ChatHarness, ChatSeed, DAY, MessageSeed, TENANT_A, THUMBNAIL_BYTES,
    THUMBNAIL_HEIGHT, THUMBNAIL_WIDTH, USER_A, USER_A2, assert_not_found, assert_odata_problem,
    assert_problem, chat_harness, ctx_a, cursor_of, ids_of, key_set, keys, mixed_width_instants,
    page_ids, page_items, parse_ts, ts,
};
use serde_json::{Value, json};
use uuid::Uuid;

const MESSAGE_KEYS: [&str; 10] = [
    "id",
    "request_id",
    "role",
    "content",
    "attachments",
    "my_reaction",
    "created_at",
    "model",
    "input_tokens",
    "output_tokens",
];
const REQUIRED_MESSAGE_KEYS: [&str; 7] = [
    "id",
    "request_id",
    "role",
    "content",
    "attachments",
    "my_reaction",
    "created_at",
];

async fn chat_with_history(h: &ChatHarness) -> (Uuid, [Uuid; 3]) {
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x100)).await;
    let request = Uuid::new_v4();
    let user = h
        .seed_message(
            MessageSeed::user(chat)
                .with_id(0x1001)
                .request(Some(request))
                .content("What is Rust?")
                .at(10),
        )
        .await;
    let assistant = h
        .seed_message(
            MessageSeed::assistant(chat)
                .with_id(0x1002)
                .request(Some(request))
                .content("A systems language.")
                .model("gpt-premium")
                .tokens(120, 45)
                .at(11),
        )
        .await;
    let system = h
        .seed_message(
            MessageSeed::system(chat)
                .with_id(0x1003)
                .content("summary")
                .at(12),
        )
        .await;
    (chat, [user, assistant, system])
}

fn by_id(items: &[Value], id: Uuid) -> &Value {
    items
        .iter()
        .find(|m| m["id"] == json!(id.to_string()))
        .unwrap_or_else(|| panic!("message {id} is not in {items:?}"))
}

// ═════════════════════════════════════════════════════════════════════════════
// envelope and item contract
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn list_returns_a_page_of_messages_in_chronological_order() {
    let h = chat_harness().await;
    let (chat, [user, assistant, system]) = chat_with_history(&h).await;

    let resp = h.list_messages(chat, "", &ctx_a()).await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let page = resp.json();
    assert_eq!(key_set(&page), keys(&["items", "page_info"]));
    assert_eq!(page["page_info"]["limit"], json!(20));
    assert_eq!(cursor_of(&page, "next_cursor"), None);
    assert_eq!(cursor_of(&page, "prev_cursor"), None);
    assert_eq!(page_ids(&page), ids_of(&[user, assistant, system]));
}

#[tokio::test]
async fn list_of_a_chat_without_messages_is_an_empty_page() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a()).await;
    let page = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["page_info"]["limit"], json!(20));
}

#[tokio::test]
async fn every_message_has_the_required_fields_and_only_documented_ones() {
    let h = chat_harness().await;
    let (chat, _) = chat_with_history(&h).await;
    let page = h.list_messages(chat, "", &ctx_a()).await.json();

    for item in page_items(&page) {
        let got = key_set(&item);
        assert!(
            keys(&REQUIRED_MESSAGE_KEYS).is_subset(&got),
            "required keys missing in {item}"
        );
        assert!(
            got.is_subset(&keys(&MESSAGE_KEYS)),
            "undocumented keys exposed in {item}: {:?}",
            got.difference(&keys(&MESSAGE_KEYS)).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn message_values_come_from_the_stored_rows() {
    let h = chat_harness().await;
    let (chat, [user, assistant, system]) = chat_with_history(&h).await;
    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    let u = by_id(&items, user);
    assert_eq!(u["role"], json!("user"));
    assert_eq!(u["content"], json!("What is Rust?"));
    assert_eq!(parse_ts(&u["created_at"]), ts(10));

    let a = by_id(&items, assistant);
    assert_eq!(a["role"], json!("assistant"));
    assert_eq!(a["content"], json!("A systems language."));
    assert_eq!(parse_ts(&a["created_at"]), ts(11));

    let s = by_id(&items, system);
    assert_eq!(s["role"], json!("system"));
    assert_eq!(s["content"], json!("summary"));
}

#[tokio::test]
async fn request_id_is_always_present_and_shared_by_the_messages_of_a_turn() {
    let h = chat_harness().await;
    let (chat, [user, assistant, system]) = chat_with_history(&h).await;
    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    for item in &items {
        let request_id = item["request_id"]
            .as_str()
            .unwrap_or_else(|| panic!("request_id is a non-null string: {item}"));
        Uuid::parse_str(request_id).expect("request_id is a UUID");
    }
    assert_eq!(
        by_id(&items, user)["request_id"],
        by_id(&items, assistant)["request_id"],
        "user and assistant message of one turn share the request_id"
    );
    assert_ne!(
        by_id(&items, system)["request_id"],
        by_id(&items, user)["request_id"]
    );
}

#[tokio::test]
async fn a_stored_null_request_id_fails_the_request_with_an_internal_error() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x101)).await;
    h.seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    h.seed_message(MessageSeed::assistant(chat).with_id(2).request(None).at(2))
        .await;

    let resp = h.list_messages(chat, "", &ctx_a()).await;

    let problem = assert_problem(&resp, 500, "internal");
    assert!(
        !resp.raw.contains("request_id"),
        "the wire detail is generic: {problem}"
    );
    assert!(
        resp.json().get("items").is_none(),
        "a message is never serialized with a null request_id"
    );
}

// ── optional fields ─────────────────────────────────────────────────────────

#[tokio::test]
async fn model_and_token_counts_appear_only_when_present() {
    let h = chat_harness().await;
    let (chat, [user, assistant, _]) = chat_with_history(&h).await;
    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    let a = by_id(&items, assistant);
    assert_eq!(a["model"], json!("gpt-premium"));
    assert_eq!(a["input_tokens"], json!(120));
    assert_eq!(a["output_tokens"], json!(45));

    let u = by_id(&items, user);
    for key in ["model", "input_tokens", "output_tokens"] {
        assert!(
            u.get(key).is_none(),
            "`{key}` is omitted when absent/0: {u}"
        );
    }
}

#[tokio::test]
async fn zero_token_counts_are_omitted_even_on_assistant_messages() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x103)).await;
    let only_input = h
        .seed_message(
            MessageSeed::assistant(chat)
                .with_id(1)
                .model("gpt-standard")
                .tokens(7, 0)
                .at(1),
        )
        .await;
    let none = h
        .seed_message(MessageSeed::assistant(chat).with_id(2).at(2))
        .await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    let a = by_id(&items, only_input);
    assert_eq!(a["input_tokens"], json!(7));
    assert!(a.get("output_tokens").is_none(), "{a}");
    let n = by_id(&items, none);
    for key in ["model", "input_tokens", "output_tokens"] {
        assert!(n.get(key).is_none(), "`{key}` omitted: {n}");
    }
}

// ── attachments[] ───────────────────────────────────────────────────────────

#[tokio::test]
async fn attachments_is_always_an_array_empty_when_there_are_none() {
    let h = chat_harness().await;
    let (chat, _) = chat_with_history(&h).await;
    for item in page_items(&h.list_messages(chat, "", &ctx_a()).await.json()) {
        assert_eq!(item["attachments"], json!([]), "{item}");
    }
}

#[tokio::test]
async fn attachment_summaries_list_the_linked_non_deleted_attachments() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x110)).await;
    let message = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    let plain = h
        .seed_message(MessageSeed::user(chat).with_id(2).at(2))
        .await;
    let doc = h
        .seed_attachment(AttachmentSeed::document(chat, "report.pdf").with_id(0xA1))
        .await;
    let img = h
        .seed_attachment(AttachmentSeed::image(chat, "photo.png").with_id(0xA2))
        .await;
    let removed = h
        .seed_attachment(
            AttachmentSeed::document(chat, "removed.pdf")
                .with_id(0xA3)
                .deleted(),
        )
        .await;
    h.seed_attachment(AttachmentSeed::document(chat, "unlinked.pdf").with_id(0xA4))
        .await;
    for attachment in [doc, img, removed] {
        h.link_attachment(chat, message, attachment).await;
    }

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    let summaries = by_id(&items, message)["attachments"]
        .as_array()
        .expect("attachments array")
        .clone();
    let mut ids: Vec<String> = summaries
        .iter()
        .map(|a| a["attachment_id"].as_str().unwrap().to_owned())
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        ids_of(&[doc, img]),
        "soft-deleted and unlinked attachments are not listed"
    );
    assert_eq!(
        by_id(&items, plain)["attachments"],
        json!([]),
        "a message without links has an empty array"
    );
}

#[tokio::test]
async fn attachment_summary_has_the_documented_fields_and_nothing_internal() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x111)).await;
    let message = h.seed_message(MessageSeed::user(chat).with_id(1)).await;
    let doc = h
        .seed_attachment(AttachmentSeed::document(chat, "report.pdf").with_id(0xB1))
        .await;
    h.link_attachment(chat, message, doc).await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    let summary = &by_id(&items, message)["attachments"][0];

    assert_eq!(summary["attachment_id"], json!(doc.to_string()));
    assert_eq!(summary["kind"], json!("document"));
    assert_eq!(summary["filename"], json!("report.pdf"));
    assert_eq!(summary["status"], json!("ready"));
    let got = key_set(summary);
    assert!(
        got.is_subset(&keys(&[
            "attachment_id",
            "kind",
            "filename",
            "status",
            "img_thumbnail"
        ])),
        "AttachmentSummary exposes no size, content type, error code or provider id: {summary}"
    );
    assert!(
        summary.get("img_thumbnail").is_none_or(Value::is_null),
        "a document has no thumbnail: {summary}"
    );
    assert!(
        !summary.to_string().contains("file-"),
        "provider file ids never leave the service"
    );
}

#[tokio::test]
async fn ready_image_attachments_carry_the_thumbnail() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x112)).await;
    let message = h.seed_message(MessageSeed::user(chat).with_id(1)).await;
    let img = h
        .seed_attachment(AttachmentSeed::image(chat, "photo.png").with_id(0xC1))
        .await;
    h.link_attachment(chat, message, img).await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    let summary = &by_id(&items, message)["attachments"][0];

    assert_eq!(summary["kind"], json!("image"));
    assert_eq!(summary["status"], json!("ready"));
    let thumb = &summary["img_thumbnail"];
    assert_eq!(
        key_set(thumb),
        keys(&["content_type", "width", "height", "data_base64"])
    );
    assert_eq!(thumb["content_type"], json!("image/webp"));
    assert_eq!(thumb["width"], json!(THUMBNAIL_WIDTH));
    assert_eq!(thumb["height"], json!(THUMBNAIL_HEIGHT));
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(thumb["data_base64"].as_str().expect("data_base64"))
        .expect("standard base64");
    assert_eq!(decoded, THUMBNAIL_BYTES);
}

#[tokio::test]
async fn thumbnails_are_present_only_for_ready_images() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x113)).await;
    let message = h.seed_message(MessageSeed::user(chat).with_id(1)).await;
    let mut not_ready = Vec::new();
    for (n, status) in [(0xD1u128, "pending"), (0xD2, "uploaded"), (0xD3, "failed")] {
        // The thumbnail bytes are stored, but the attachment is not ready.
        let id = h
            .seed_attachment(
                AttachmentSeed::image(chat, "img.png")
                    .with_id(n)
                    .status(status),
            )
            .await;
        h.link_attachment(chat, message, id).await;
        not_ready.push((id, status));
    }

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    let summaries = by_id(&items, message)["attachments"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(summaries.len(), 3);
    for (id, status) in not_ready {
        let summary = summaries
            .iter()
            .find(|s| s["attachment_id"] == json!(id.to_string()))
            .unwrap();
        assert_eq!(summary["status"], json!(status));
        assert!(
            summary.get("img_thumbnail").is_none_or(Value::is_null),
            "a {status} image has no thumbnail: {summary}"
        );
    }
}

#[tokio::test]
async fn one_attachment_can_be_listed_on_several_messages() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x114)).await;
    let first = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    let second = h
        .seed_message(MessageSeed::user(chat).with_id(2).at(2))
        .await;
    let doc = h
        .seed_attachment(AttachmentSeed::document(chat, "shared.pdf").with_id(0xE1))
        .await;
    h.link_attachment(chat, first, doc).await;
    h.link_attachment(chat, second, doc).await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    for message in [first, second] {
        assert_eq!(
            by_id(&items, message)["attachments"][0]["attachment_id"],
            json!(doc.to_string())
        );
    }
}

// ── my_reaction ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn my_reaction_is_always_present_and_null_without_a_reaction() {
    let h = chat_harness().await;
    let (chat, _) = chat_with_history(&h).await;
    for item in page_items(&h.list_messages(chat, "", &ctx_a()).await.json()) {
        assert!(
            item.as_object().unwrap().contains_key("my_reaction"),
            "my_reaction is always in the JSON: {item}"
        );
        assert_eq!(item["my_reaction"], Value::Null);
    }
}

#[tokio::test]
async fn my_reaction_shows_the_callers_reaction_on_assistant_messages() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x120)).await;
    let liked = h
        .seed_message(MessageSeed::assistant(chat).with_id(1).at(1))
        .await;
    let disliked = h
        .seed_message(MessageSeed::assistant(chat).with_id(2).at(2))
        .await;
    let untouched = h
        .seed_message(MessageSeed::assistant(chat).with_id(3).at(3))
        .await;
    h.seed_reaction(TENANT_A, USER_A, liked, "like").await;
    h.seed_reaction(TENANT_A, USER_A, disliked, "dislike").await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    assert_eq!(by_id(&items, liked)["my_reaction"], json!("like"));
    assert_eq!(by_id(&items, disliked)["my_reaction"], json!("dislike"));
    assert_eq!(by_id(&items, untouched)["my_reaction"], Value::Null);
}

#[tokio::test]
async fn my_reaction_is_always_null_for_user_and_system_messages() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x121)).await;
    let user = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    let system = h
        .seed_message(MessageSeed::system(chat).with_id(2).at(2))
        .await;
    // Even a stray row (the API never writes one) must not surface.
    h.seed_reaction(TENANT_A, USER_A, user, "like").await;
    h.seed_reaction(TENANT_A, USER_A, system, "dislike").await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    assert_eq!(by_id(&items, user)["my_reaction"], Value::Null);
    assert_eq!(by_id(&items, system)["my_reaction"], Value::Null);
}

#[tokio::test]
async fn my_reaction_never_shows_another_users_reaction() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x122)).await;
    let message = h
        .seed_message(MessageSeed::assistant(chat).with_id(1).at(1))
        .await;
    // A second user of the same tenant reacted (they cannot see the chat via
    // the API, but a row exists), and so did a user of another tenant.
    h.seed_reaction(TENANT_A, USER_A2, message, "dislike").await;
    h.seed_reaction(
        chat_support::TENANT_B,
        chat_support::USER_B,
        message,
        "like",
    )
    .await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    assert_eq!(by_id(&items, message)["my_reaction"], Value::Null);

    h.seed_reaction(TENANT_A, USER_A, message, "like").await;
    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());
    assert_eq!(by_id(&items, message)["my_reaction"], json!("like"));
}

// ── what is listed ──────────────────────────────────────────────────────────

#[tokio::test]
async fn soft_deleted_messages_are_not_listed() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x130)).await;
    let kept = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    h.seed_message(MessageSeed::assistant(chat).with_id(2).at(2).deleted())
        .await;
    let page = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page_ids(&page), ids_of(&[kept]));
}

#[tokio::test]
async fn messages_of_other_chats_are_not_listed() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x131)).await;
    let other = h.seed_chat(ChatSeed::of_a().with_id(0x132)).await;
    let mine = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(1))
        .await;
    h.seed_message(MessageSeed::user(other).with_id(2).at(1))
        .await;
    let page = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page_ids(&page), ids_of(&[mine]));
}

#[tokio::test]
async fn child_queries_are_not_blanked_by_the_owner_narrowed_chat_scope() {
    // The chat scope carries `owner_id = subject`; messages, attachments and
    // reactions have no owner column, so applying that scope to them would
    // deny everything. The owner check happens once, on the chat.
    let h = chat_harness().await;
    let (chat, [user, assistant, _]) = chat_with_history(&h).await;
    let doc = h
        .seed_attachment(AttachmentSeed::document(chat, "d.pdf").with_id(0xF1))
        .await;
    h.link_attachment(chat, user, doc).await;
    h.seed_reaction(TENANT_A, USER_A, assistant, "like").await;

    let items = page_items(&h.list_messages(chat, "", &ctx_a()).await.json());

    assert_eq!(items.len(), 3);
    assert_eq!(
        by_id(&items, user)["attachments"].as_array().unwrap().len(),
        1
    );
    assert_eq!(by_id(&items, assistant)["my_reaction"], json!("like"));
}

#[tokio::test]
async fn unknown_or_deleted_chat_is_404_for_the_chat_resource() {
    let h = chat_harness().await;
    let deleted = h.seed_chat(ChatSeed::of_a().deleted()).await;
    h.seed_message(MessageSeed::user(deleted)).await;
    for chat in [Uuid::new_v4(), deleted] {
        assert_not_found(&h.list_messages(chat, "", &ctx_a()).await, "chat");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// ordering and pagination
// ═════════════════════════════════════════════════════════════════════════════

async fn numbered_chat(h: &ChatHarness, count: u128) -> (Uuid, Vec<Uuid>) {
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x200)).await;
    let mut ids = Vec::new();
    for n in 1..=count {
        let role = if n % 2 == 1 { "user" } else { "assistant" };
        ids.push(
            h.seed_message(
                MessageSeed::new(chat, role)
                    .with_id(n)
                    .content(&format!("m{n}"))
                    .at(i64::try_from(n).unwrap() * 10),
            )
            .await,
        );
    }
    (chat, ids)
}

#[tokio::test]
async fn default_order_is_created_at_ascending_regardless_of_insertion_order() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x201)).await;
    let late = h
        .seed_message(MessageSeed::user(chat).with_id(1).at(300))
        .await;
    let early = h
        .seed_message(MessageSeed::user(chat).with_id(2).at(100))
        .await;
    let mid = h
        .seed_message(MessageSeed::user(chat).with_id(3).at(200))
        .await;
    let page = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page_ids(&page), ids_of(&[early, mid, late]));
}

#[tokio::test]
async fn messages_with_the_same_timestamp_are_ordered_deterministically_across_pages() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x202)).await;
    for n in 1..=5u128 {
        h.seed_message(MessageSeed::user(chat).with_id(n).at(50))
            .await;
    }
    let full = page_ids(&h.list_messages(chat, "limit=100", &ctx_a()).await.json());
    assert_eq!(full.len(), 5);

    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let query = match &cursor {
            Some(c) => format!("limit=2&cursor={c}"),
            None => "limit=2".to_owned(),
        };
        let page = h.list_messages(chat, &query, &ctx_a()).await.json();
        walked.extend(page_ids(&page));
        cursor = cursor_of(&page, "next_cursor");
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(walked, full, "(created_at, id) is a strict total order");
}

#[tokio::test]
async fn limit_defaults_to_20_and_is_clamped_to_100() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 105).await;

    let default = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page_items(&default).len(), 20);
    assert_eq!(default["page_info"]["limit"], json!(20));
    assert!(cursor_of(&default, "next_cursor").is_some());

    let clamped = h.list_messages(chat, "limit=500", &ctx_a()).await;
    assert_eq!(clamped.status.as_u16(), 200, "{}", clamped.raw);
    let clamped = clamped.json();
    assert_eq!(page_items(&clamped).len(), 100);
    assert_eq!(clamped["page_info"]["limit"], json!(100));

    let three = h.list_messages(chat, "limit=3", &ctx_a()).await.json();
    assert_eq!(page_items(&three).len(), 3);
    assert_eq!(three["page_info"]["limit"], json!(3));
}

#[tokio::test]
async fn limit_zero_is_400_invalid_limit() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    let resp = h.list_messages(chat, "limit=0", &ctx_a()).await;
    assert_odata_problem(&resp, "$top", "INVALID_LIMIT");
}

#[tokio::test]
async fn cursor_pagination_walks_forward_and_backward() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 5).await;

    let p1 = h.list_messages(chat, "limit=2", &ctx_a()).await.json();
    assert_eq!(page_ids(&p1), ids_of(&ids[0..2]));
    assert_eq!(cursor_of(&p1, "prev_cursor"), None);
    let next1 = cursor_of(&p1, "next_cursor").expect("next");

    let p2 = h
        .list_messages(chat, &format!("limit=2&cursor={next1}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p2), ids_of(&ids[2..4]));
    let prev2 = cursor_of(&p2, "prev_cursor").expect("prev");
    let next2 = cursor_of(&p2, "next_cursor").expect("next");

    let p3 = h
        .list_messages(chat, &format!("limit=2&cursor={next2}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p3), ids_of(&ids[4..5]));
    assert_eq!(cursor_of(&p3, "next_cursor"), None);
    let prev3 = cursor_of(&p3, "prev_cursor").expect("prev");

    let back = h
        .list_messages(chat, &format!("limit=2&cursor={prev3}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&back), ids_of(&ids[2..4]));
    let back = h
        .list_messages(chat, &format!("limit=2&cursor={prev2}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&back), ids_of(&ids[0..2]));
}

#[tokio::test]
async fn malformed_cursor_is_400_invalid_cursor() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    for cursor in ["not-a-cursor!!", "e30", "AAAA"] {
        let resp = h
            .list_messages(chat, &format!("cursor={cursor}"), &ctx_a())
            .await;
        assert_odata_problem(&resp, "cursor", "INVALID_CURSOR");
    }
}

#[tokio::test]
async fn cursor_combined_with_orderby_is_400_order_with_cursor() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 4).await;
    let next = cursor_of(
        &h.list_messages(chat, "limit=1", &ctx_a()).await.json(),
        "next_cursor",
    )
    .unwrap();
    let resp = h
        .list_messages(
            chat,
            &format!("limit=1&cursor={next}&$orderby=created_at%20desc"),
            &ctx_a(),
        )
        .await;
    assert_odata_problem(&resp, "cursor", "ORDER_WITH_CURSOR");
}

#[tokio::test]
async fn cursor_of_another_filter_is_400_filter_mismatch() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 6).await;
    let first = h
        .list_messages(chat, "limit=1&$filter=role%20eq%20'user'", &ctx_a())
        .await
        .json();
    let next = cursor_of(&first, "next_cursor").expect("more");
    let resp = h
        .list_messages(
            chat,
            &format!("limit=1&cursor={next}&$filter=role%20eq%20'assistant'"),
            &ctx_a(),
        )
        .await;
    assert_odata_problem(&resp, "cursor", "FILTER_MISMATCH");
}

// ── $orderby ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn orderby_created_at_descending_and_by_id() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 4).await;

    let desc = h
        .list_messages(chat, "$orderby=created_at%20desc", &ctx_a())
        .await;
    assert_eq!(desc.status.as_u16(), 200, "{}", desc.raw);
    let reversed: Vec<Uuid> = ids.iter().rev().copied().collect();
    assert_eq!(page_ids(&desc.json()), ids_of(&reversed));

    let by_id_desc = h
        .list_messages(chat, "$orderby=id%20desc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&by_id_desc), ids_of(&reversed));
    let by_id = h
        .list_messages(chat, "$orderby=id%20asc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&by_id), ids_of(&ids));
}

#[tokio::test]
async fn orderby_role_groups_messages_by_role() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 4).await; // user, assistant, user, assistant
    let asc = h
        .list_messages(chat, "$orderby=role%20asc,created_at%20asc", &ctx_a())
        .await;
    assert_eq!(asc.status.as_u16(), 200, "{}", asc.raw);
    assert_eq!(
        page_ids(&asc.json()),
        ids_of(&[ids[1], ids[3], ids[0], ids[2]]),
        "assistant < user"
    );
    let desc = h
        .list_messages(chat, "$orderby=role%20desc,created_at%20asc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&desc), ids_of(&[ids[0], ids[2], ids[1], ids[3]]));
}

#[tokio::test]
async fn orderby_unknown_field_is_400_invalid_orderby_field() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    for orderby in [
        "content%20asc",
        "request_id%20asc",
        "model%20desc",
        "my_reaction%20asc",
        "nope",
        "created_at%20sideways",
    ] {
        let resp = h
            .list_messages(chat, &format!("$orderby={orderby}"), &ctx_a())
            .await;
        assert_odata_problem(&resp, "$orderby", "INVALID_ORDERBY_FIELD");
    }
}

// ── $filter ─────────────────────────────────────────────────────────────────

async fn filtered(h: &ChatHarness, chat: Uuid, filter: &str) -> Vec<String> {
    let resp = h
        .list_messages(
            chat,
            &format!("$filter={}", filter.replace(' ', "%20")),
            &ctx_a(),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200, "$filter={filter}: {}", resp.raw);
    page_ids(&resp.json())
}

#[tokio::test]
async fn filter_by_role() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 4).await;
    assert_eq!(
        filtered(&h, chat, "role eq 'assistant'").await,
        ids_of(&[ids[1], ids[3]])
    );
    assert_eq!(
        filtered(&h, chat, "role ne 'assistant'").await,
        ids_of(&[ids[0], ids[2]])
    );
    assert_eq!(
        filtered(&h, chat, "contains(role,'assist')").await,
        ids_of(&[ids[1], ids[3]])
    );
    assert_eq!(
        filtered(&h, chat, "startswith(role,'us')").await,
        ids_of(&[ids[0], ids[2]])
    );
    assert_eq!(
        filtered(&h, chat, "endswith(role,'ant')").await,
        ids_of(&[ids[1], ids[3]])
    );
    assert_eq!(
        filtered(&h, chat, "role in ('user','system')").await,
        ids_of(&[ids[0], ids[2]])
    );
    assert!(filtered(&h, chat, "role eq 'system'").await.is_empty());
}

#[tokio::test]
async fn filter_by_created_at() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x210)).await;
    let mut ids = Vec::new();
    for n in 1..=4u128 {
        ids.push(
            h.seed_message(
                MessageSeed::user(chat)
                    .with_id(n)
                    .at(i64::try_from(n).unwrap() * DAY),
            )
            .await,
        );
    }
    // messages at 2025-01-02, 01-03, 01-04, 01-05 (00:00:00Z)
    assert_eq!(
        filtered(&h, chat, "created_at gt 2025-01-03T12:00:00Z").await,
        ids_of(&ids[2..])
    );
    assert_eq!(
        filtered(&h, chat, "created_at lt 2025-01-03T12:00:00Z").await,
        ids_of(&ids[..2])
    );
    assert_eq!(
        filtered(
            &h,
            chat,
            "created_at ge 2025-01-02T12:00:00Z and created_at le 2025-01-04T12:00:00Z"
        )
        .await,
        ids_of(&ids[1..3])
    );
}

#[tokio::test]
async fn filter_by_id() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 3).await;
    assert_eq!(
        filtered(&h, chat, &format!("id eq {}", ids[1])).await,
        ids_of(&ids[1..2])
    );
    assert_eq!(
        filtered(&h, chat, &format!("id ne {}", ids[1])).await,
        ids_of(&[ids[0], ids[2]])
    );
    assert_eq!(
        filtered(&h, chat, &format!("id in ({},{})", ids[0], ids[2])).await,
        ids_of(&[ids[0], ids[2]])
    );
}

#[tokio::test]
async fn filter_is_limited_to_the_chat_and_to_live_messages() {
    let h = chat_harness().await;
    let (chat, ids) = numbered_chat(&h, 2).await;
    let other = h.seed_chat(ChatSeed::of_a().with_id(0x211)).await;
    let foreign = h
        .seed_message(MessageSeed::user(other).with_id(0x9001).at(5))
        .await;
    h.seed_message(MessageSeed::user(chat).with_id(0x9002).at(6).deleted())
        .await;

    assert_eq!(
        filtered(&h, chat, "role eq 'user'").await,
        ids_of(&[ids[0]])
    );
    assert_eq!(
        filtered(&h, chat, &format!("id eq {foreign}")).await,
        Vec::<String>::new(),
        "a message of another chat cannot be reached through this chat"
    );
}

#[tokio::test]
async fn filter_with_an_unknown_field_or_bad_syntax_is_400_invalid_filter() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    for filter in [
        "content eq 'm1'",
        "request_id eq 5",
        "chat_id eq 5",
        "model eq 'x'",
        "role eq",
        "role eqq 'user'",
        "role eq 'unterminated",
        "created_at gt notadate",
        "((role eq 'user')",
    ] {
        let resp = h
            .list_messages(
                chat,
                &format!("$filter={}", filter.replace(' ', "%20")),
                &ctx_a(),
            )
            .await;
        assert_odata_problem(&resp, "$filter", "INVALID_FILTER");
    }
}

#[tokio::test]
async fn select_is_accepted_and_ignored_and_bad_select_is_400() {
    let h = chat_harness().await;
    let (chat, _) = chat_with_history(&h).await;
    let full = h.list_messages(chat, "", &ctx_a()).await.json();

    let resp = h.list_messages(chat, "$select=id,content", &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert_eq!(resp.json(), full, "the full item is always returned");

    let bad = h.list_messages(chat, "$select=", &ctx_a()).await;
    assert_odata_problem(&bad, "$select", "INVALID_SELECT");
}

#[tokio::test]
async fn unsupported_odata_options_are_400_unsupported_query_param() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    for option in ["$skip=1", "$count=true", "$bogus=1"] {
        let resp = h.list_messages(chat, option, &ctx_a()).await;
        let name = option.split('=').next().unwrap();
        assert_odata_problem(&resp, name, "UNSUPPORTED_QUERY_PARAM");
    }
}

#[tokio::test]
async fn non_numeric_limit_is_a_400_odata_error() {
    let h = chat_harness().await;
    let (chat, _) = numbered_chat(&h, 2).await;
    for query in ["limit=abc", "limit=-1"] {
        let resp = h.list_messages(chat, query, &ctx_a()).await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.odata.query.v1~"),
            "{query}"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// registration
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn list_messages_is_registered_with_odata_fields_and_status_codes() {
    let h = chat_harness().await;
    let spec = h
        .openapi
        .operation_specs
        .get("GET:/mini-chat/v1/chats/{id}/messages")
        .expect("registered");
    assert_eq!(
        spec.operation_id.as_deref(),
        Some("mini_chat.list_messages")
    );
    assert!(spec.authenticated);
    let statuses: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
    for status in [200, 400, 403, 404, 500, 503] {
        assert!(
            statuses.contains(&status),
            "declares {status}: {statuses:?}"
        );
    }
    let filter = spec
        .vendor_extensions
        .x_odata_filter
        .as_ref()
        .expect("x-odata-filter");
    assert_eq!(
        filter.allowed_fields.keys().cloned().collect::<Vec<_>>(),
        vec!["created_at".to_owned(), "id".to_owned(), "role".to_owned()]
    );
    let mut orderby = spec
        .vendor_extensions
        .x_odata_orderby
        .as_ref()
        .expect("x-odata-orderby")
        .allowed_fields
        .clone();
    orderby.sort();
    let mut want: Vec<String> = [
        "created_at asc",
        "created_at desc",
        "id asc",
        "id desc",
        "role asc",
        "role desc",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    want.sort();
    assert_eq!(orderby, want);
}

#[tokio::test]
async fn non_uuid_chat_id_is_400_invalid_path_params() {
    let h = chat_harness().await;
    let resp = h
        .get(
            &format!("{}/v1/chats/not-a-uuid/messages", h.prefix),
            &ctx_a(),
        )
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    chat_support::assert_violation_reason(&problem, "invalid_path_params");
    assert!(h.pdp.recorded().is_empty());
}

// ── timestamps: exact $filter and chronological order ───────────────────────

/// Five messages of one chat whose `created_at` instants share one second but
/// have different fraction widths; message `n` (1..=5) is the n-th instant in
/// ascending order. Returns the chat, the ids and the `created_at` strings
/// exactly as the API renders them.
async fn mixed_width_messages(h: &ChatHarness) -> (Uuid, Vec<Uuid>, Vec<String>) {
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x2F0)).await;
    let instants = mixed_width_instants();
    // Insert out of chronological order so insertion order cannot hide a bad sort.
    let mut ids = vec![Uuid::nil(); 5];
    for index in [3usize, 0, 4, 2, 1] {
        ids[index] = h
            .seed_message(
                MessageSeed::user(chat)
                    .with_id(u128::try_from(index + 1).unwrap())
                    .at_time(instants[index]),
            )
            .await;
    }
    let page = h.list_messages(chat, "limit=100", &ctx_a()).await.json();
    let mut stamps = vec![String::new(); 5];
    for item in page_items(&page) {
        let id = Uuid::parse_str(item["id"].as_str().unwrap()).unwrap();
        let index = ids.iter().position(|i| *i == id).unwrap();
        item["created_at"]
            .as_str()
            .unwrap()
            .clone_into(&mut stamps[index]);
        assert_eq!(parse_ts(&item["created_at"]), instants[index]);
    }
    (chat, ids, stamps)
}

fn asc_ids(ids: &[Uuid], indexes: impl IntoIterator<Item = usize>) -> Vec<String> {
    let mut picked: Vec<usize> = indexes.into_iter().collect();
    picked.sort_unstable();
    picked.into_iter().map(|i| ids[i].to_string()).collect()
}

#[tokio::test]
async fn created_at_filters_are_exact_on_the_timestamps_the_api_returns() {
    let h = chat_harness().await;
    let (chat, ids, stamps) = mixed_width_messages(&h).await;
    for (i, stamp) in stamps.iter().enumerate() {
        let all = 0..5usize;
        // The default order is created_at ascending.
        assert_eq!(
            filtered(&h, chat, &format!("created_at eq {stamp}")).await,
            asc_ids(&ids, [i]),
            "eq {stamp}"
        );
        assert_eq!(
            filtered(&h, chat, &format!("created_at ne {stamp}")).await,
            asc_ids(&ids, all.clone().filter(|j| *j != i)),
            "ne {stamp}"
        );
        assert_eq!(
            filtered(&h, chat, &format!("created_at gt {stamp}")).await,
            asc_ids(&ids, all.clone().filter(|j| *j > i)),
            "gt {stamp}"
        );
        assert_eq!(
            filtered(&h, chat, &format!("created_at ge {stamp}")).await,
            asc_ids(&ids, all.clone().filter(|j| *j >= i)),
            "ge {stamp}"
        );
        assert_eq!(
            filtered(&h, chat, &format!("created_at lt {stamp}")).await,
            asc_ids(&ids, all.clone().filter(|j| *j < i)),
            "lt {stamp}"
        );
        assert_eq!(
            filtered(&h, chat, &format!("created_at le {stamp}")).await,
            asc_ids(&ids, all.filter(|j| *j <= i)),
            "le {stamp}"
        );
    }
    assert_eq!(
        filtered(
            &h,
            chat,
            &format!("created_at in ({},{},{})", stamps[0], stamps[2], stamps[4])
        )
        .await,
        asc_ids(&ids, [0, 2, 4])
    );
}

#[tokio::test]
async fn same_second_timestamps_of_different_fraction_widths_are_ordered_chronologically() {
    let h = chat_harness().await;
    let (chat, ids, _) = mixed_width_messages(&h).await;
    let ascending: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let descending: Vec<String> = ascending.iter().rev().cloned().collect();

    let default = h.list_messages(chat, "", &ctx_a()).await.json();
    assert_eq!(page_ids(&default), ascending, "chronological by default");
    let desc = h
        .list_messages(chat, "$orderby=created_at%20desc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&desc), descending);

    for (orderby, expected) in [("asc", &ascending), ("desc", &descending)] {
        let mut walked = Vec::new();
        let mut query = format!("limit=1&$orderby=created_at%20{orderby}");
        for _ in 0..10 {
            let page = h.list_messages(chat, &query, &ctx_a()).await.json();
            walked.extend(page_ids(&page));
            match cursor_of(&page, "next_cursor") {
                Some(cursor) => query = format!("limit=1&cursor={cursor}"),
                None => break,
            }
        }
        assert_eq!(&walked, expected, "created_at {orderby} keyset walk");
    }
}
