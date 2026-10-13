//! Chat CRUD, chat list (OData, cursor pagination, ordering by activity) and
//! chat deletion (soft delete + chat-cleanup outbox enqueue), exercised
//! in-process: the gear's chat routes on a migrated SQLite database with a fake
//! model-policy plugin, a fake PDP and a running outbox.
//!
//! Source of truth: DESIGN 3.3 "Create Chat", "List Chats", "Get Chat",
//! "Update Chat Title", "Delete Chat" and the REST error table under "Error
//! Codes", 3.6 / section 4 "Cleanup on Chat Deletion", 3.7 `chats`,
//! `attachments`, 3.8 (authorization matrix), ADR-0004, ADR-0009 and the
//! OpenAPI schemas `ChatDetailDto` / `CreateChatReq` / `UpdateChatReq` /
//! `Page_ChatDetailDto` / `PageInfo`.

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

use chat_support::{
    AttachmentSeed, CHAT_PDP_TYPE, ChatHarnessOptions, ChatSeed, DAY, FakePdp, MessageSeed,
    TENANT_A, USER_A, assert_field_violation, assert_invalid_argument, assert_not_found,
    assert_odata_problem, assert_problem, assert_violation_reason, chat_harness, chat_harness_with,
    ctx_a, cursor_of, from_db, ids_of, key_set, keys, mixed_width_instants, model_entry, page_ids,
    page_items, parse_ts, send, send_body, ts, uid,
};
use serde_json::json;
use uuid::Uuid;

const CHAT_DETAIL_KEYS: [&str; 7] = [
    "id",
    "model",
    "title",
    "is_temporary",
    "message_count",
    "created_at",
    "updated_at",
];

fn title_of(chars: usize) -> String {
    "a".repeat(chars)
}

// ═════════════════════════════════════════════════════════════════════════════
// POST /v1/chats
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn create_returns_201_with_a_location_header_and_the_chat_detail() {
    let h = chat_harness().await;
    let resp = h
        .post_json(
            &h.chats_uri(),
            &ctx_a(),
            &json!({"title": "Q3 Financial Analysis"}),
        )
        .await;

    assert_eq!(resp.status.as_u16(), 201, "body: {}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    let id = body["id"].as_str().expect("id is a string");
    Uuid::parse_str(id).expect("id is a UUID");
    assert_eq!(
        resp.headers.get("location").and_then(|v| v.to_str().ok()),
        Some(format!("/mini-chat/v1/chats/{id}").as_str()),
        "Location is the request path plus the new id (DESIGN 3.3 Create Chat)"
    );
    assert_eq!(body["title"], json!("Q3 Financial Analysis"));
    assert_eq!(body["is_temporary"], json!(false));
    assert_eq!(body["message_count"], json!(0));
    parse_ts(&body["created_at"]);
    parse_ts(&body["updated_at"]);
}

#[tokio::test]
async fn create_response_has_exactly_the_public_chat_fields() {
    let h = chat_harness().await;
    let body = h.create_chat(&ctx_a(), &json!({"title": "T"})).await;

    assert_eq!(
        key_set(&body),
        keys(&CHAT_DETAIL_KEYS),
        "ChatDetailDto only; user_id / tenant_id never appear (DESIGN 3.3)"
    );
    assert!(!body.to_string().contains(&USER_A.to_string()));
}

#[tokio::test]
async fn create_without_a_title_omits_the_field_from_the_json() {
    let h = chat_harness().await;
    for payload in [json!({}), json!({"title": null})] {
        let body = h.create_chat(&ctx_a(), &payload).await;
        assert!(
            body.get("title").is_none(),
            "`title` is omitted, not null, for an untitled chat: {body}"
        );
        assert_eq!(
            key_set(&body),
            keys(&[
                "id",
                "model",
                "is_temporary",
                "message_count",
                "created_at",
                "updated_at"
            ])
        );
    }
}

#[tokio::test]
async fn create_persists_the_chat_for_the_caller_only() {
    let h = chat_harness().await;
    let body = h.create_chat(&ctx_a(), &json!({"title": "Mine"})).await;
    let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();

    let row = h.chat_row(id).await.expect("row was inserted");
    assert_eq!(row.tenant_id, TENANT_A);
    assert_eq!(row.user_id, USER_A);
    assert_eq!(row.title.as_deref(), Some("Mine"));
    assert!(!row.is_temporary);
    assert!(row.deleted_at.is_none());
    assert_eq!(row.model, body["model"].as_str().unwrap());
}

#[tokio::test]
async fn create_ignores_identity_fields_supplied_by_the_client() {
    let h = chat_harness().await;
    let body = h
        .create_chat(
            &ctx_a(),
            &json!({"title": "X", "user_id": Uuid::new_v4(), "tenant_id": Uuid::new_v4(),
                    "is_temporary": true, "unknown": 1}),
        )
        .await;
    let row = h
        .chat_row(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap())
        .await
        .unwrap();
    assert_eq!((row.tenant_id, row.user_id), (TENANT_A, USER_A));
    assert!(!row.is_temporary, "is_temporary is false at P1");
}

#[tokio::test]
async fn create_without_a_model_uses_the_first_enabled_default_model() {
    let h = chat_harness().await;
    // `gpt-standard` is listed first but only `gpt-premium` has `is_default`.
    let body = h.create_chat(&ctx_a(), &json!({})).await;
    assert_eq!(body["model"], json!("gpt-premium"));

    let body = h.create_chat(&ctx_a(), &json!({"model": null})).await;
    assert_eq!(body["model"], json!("gpt-premium"), "null means absent");
}

#[tokio::test]
async fn create_without_a_default_flag_falls_back_to_the_first_enabled_model() {
    let h = chat_harness_with(ChatHarnessOptions {
        models: vec![
            model_entry("m-off", "Standard", false, false),
            model_entry("m-first", "Standard", true, false),
            model_entry("m-second", "Premium", true, false),
        ],
        ..Default::default()
    })
    .await;
    let body = h.create_chat(&ctx_a(), &json!({})).await;
    assert_eq!(body["model"], json!("m-first"));
}

#[tokio::test]
async fn create_default_resolution_ignores_a_disabled_default_model() {
    let h = chat_harness_with(ChatHarnessOptions {
        models: vec![
            model_entry("m-off-default", "Premium", false, true),
            model_entry("m-first", "Standard", true, false),
            model_entry("m-default", "Standard", true, true),
        ],
        ..Default::default()
    })
    .await;
    let body = h.create_chat(&ctx_a(), &json!({})).await;
    assert_eq!(
        body["model"],
        json!("m-default"),
        "the first ENABLED model with is_default wins over a disabled default"
    );
}

#[tokio::test]
async fn create_default_resolution_uses_the_current_catalog() {
    let h = chat_harness().await;
    assert_eq!(
        h.create_chat(&ctx_a(), &json!({})).await["model"],
        json!("gpt-premium")
    );
    h.plugin.set_catalog(vec![
        model_entry("gpt-new", "Standard", true, true),
        model_entry("gpt-premium", "Premium", false, true),
    ]);
    assert_eq!(
        h.create_chat(&ctx_a(), &json!({})).await["model"],
        json!("gpt-new"),
        "no local snapshot cache: every create reads the policy plugin"
    );
}

#[tokio::test]
async fn create_with_an_explicit_enabled_model_stores_it() {
    let h = chat_harness().await;
    let body = h
        .create_chat(&ctx_a(), &json!({"model": "gpt-standard"}))
        .await;
    assert_eq!(body["model"], json!("gpt-standard"));
    let row = h
        .chat_row(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap())
        .await
        .unwrap();
    assert_eq!(row.model, "gpt-standard");
}

#[tokio::test]
async fn create_with_an_unknown_or_disabled_model_is_400_invalid_model() {
    let h = chat_harness().await;
    for model in ["gpt-off", "does-not-exist", "", "GPT-PREMIUM"] {
        let resp = h
            .post_json(&h.chats_uri(), &ctx_a(), &json!({"model": model}))
            .await;
        let problem = assert_invalid_argument(&resp, "model", "INVALID_MODEL");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.chat.v1~"),
            "model {model:?}"
        );
    }
    assert!(h.all_chats().await.is_empty(), "nothing was created");
}

#[tokio::test]
async fn create_reads_the_catalog_for_the_calling_user() {
    let h = chat_harness().await;
    h.create_chat(&ctx_a(), &json!({})).await;
    assert!(
        h.plugin.seen_users.lock().unwrap().contains(&USER_A),
        "the model-policy plugin is asked on behalf of the caller"
    );
}

#[tokio::test]
async fn create_trims_the_title() {
    let h = chat_harness().await;
    let body = h
        .create_chat(&ctx_a(), &json!({"title": "  \t Hello  world \n "}))
        .await;
    assert_eq!(body["title"], json!("Hello  world"));
    let row = h
        .chat_row(Uuid::parse_str(body["id"].as_str().unwrap()).unwrap())
        .await
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("Hello  world"));
}

#[tokio::test]
async fn create_accepts_titles_of_1_to_255_characters_after_trim() {
    let h = chat_harness().await;
    for title in [
        title_of(1),
        title_of(255),
        format!("   {}   ", title_of(255)),
        "\u{e9}".repeat(255), // 255 characters, 510 bytes
    ] {
        let resp = h
            .post_json(&h.chats_uri(), &ctx_a(), &json!({"title": title}))
            .await;
        assert_eq!(
            resp.status.as_u16(),
            201,
            "{} chars: {}",
            title.len(),
            resp.raw
        );
    }
}

#[tokio::test]
async fn create_rejects_empty_whitespace_and_overlong_titles_with_invalid_title() {
    let h = chat_harness().await;
    for title in [
        String::new(),
        "   ".to_owned(),
        "\n\t \r".to_owned(),
        title_of(256),
        format!("  {}  ", title_of(256)),
        "\u{e9}".repeat(256),
    ] {
        let resp = h
            .post_json(&h.chats_uri(), &ctx_a(), &json!({"title": title}))
            .await;
        let problem = assert_invalid_argument(&resp, "title", "INVALID_TITLE");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.chat.v1~")
        );
    }
    assert!(h.all_chats().await.is_empty());
}

#[tokio::test]
async fn the_title_is_validated_before_authorization_and_the_model_lookup() {
    let h = chat_harness_with(ChatHarnessOptions {
        pdp: FakePdp::deny("no grant"),
        ..Default::default()
    })
    .await;

    let resp = h
        .post_json(
            &h.chats_uri(),
            &ctx_a(),
            &json!({"title": "   ", "model": "does-not-exist"}),
        )
        .await;

    assert_invalid_argument(&resp, "title", "INVALID_TITLE");
    assert!(
        h.pdp.recorded().is_empty(),
        "an invalid title never reaches the PDP"
    );
    assert_eq!(h.plugin.total_calls(), 0, "nor the model-policy plugin");
}

#[tokio::test]
async fn authorization_precedes_the_model_lookup_on_create() {
    let h = chat_harness_with(ChatHarnessOptions {
        pdp: FakePdp::deny("no grant"),
        ..Default::default()
    })
    .await;

    let resp = h
        .post_json(
            &h.chats_uri(),
            &ctx_a(),
            &json!({"model": "does-not-exist"}),
        )
        .await;

    let problem = assert_problem(&resp, 403, "permission_denied");
    assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    assert_eq!(h.plugin.total_calls(), 0);
    assert!(h.all_chats().await.is_empty());
}

#[tokio::test]
async fn create_rejects_bodies_the_json_extractor_refuses() {
    let h = chat_harness().await;
    let uri = h.chats_uri();

    // Schema mismatch: wrong types -> 422 `invalid_json_body`.
    for payload in [
        r#"{"title": 5}"#,
        r#"{"model": 5}"#,
        r#"{"title": ["a"]}"#,
        "[]",
        "5",
    ] {
        let resp = h
            .raw("POST", &uri, &ctx_a(), Some("application/json"), payload)
            .await;
        let problem = assert_problem(&resp, 422, "invalid_argument");
        assert_field_violation(&problem, "body", "invalid_json_body");
    }
    // Malformed JSON -> 400 `json_syntax_error`.
    let resp = h
        .raw(
            "POST",
            &uri,
            &ctx_a(),
            Some("application/json"),
            r#"{"title": "#,
        )
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "body", "json_syntax_error");

    // No JSON content type -> 415 `missing_json_content_type`.
    for content_type in [None, Some("text/plain")] {
        let resp = h.raw("POST", &uri, &ctx_a(), content_type, "{}").await;
        let problem = assert_problem(&resp, 415, "invalid_argument");
        assert_field_violation(&problem, "body", "missing_json_content_type");
    }
    assert!(h.all_chats().await.is_empty());
}

#[tokio::test]
async fn created_chat_can_be_read_back_and_is_listed() {
    let h = chat_harness().await;
    let created = h
        .create_chat(&ctx_a(), &json!({"title": "Round trip"}))
        .await;
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();

    let got = h.get(&h.chat_uri(id), &ctx_a()).await;
    assert_eq!(got.status.as_u16(), 200, "{}", got.raw);
    assert_eq!(got.json(), created, "GET returns what POST returned");

    let list = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_items(&list), vec![created]);
}

#[tokio::test]
async fn location_header_follows_the_configured_url_prefix() {
    let h = chat_harness_with(ChatHarnessOptions {
        prefix: "/custom".to_owned(),
        ..Default::default()
    })
    .await;
    let resp = h.post_json(&h.chats_uri(), &ctx_a(), &json!({})).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    let id = resp.json()["id"].as_str().unwrap().to_owned();
    assert_eq!(
        resp.headers.get("location").and_then(|v| v.to_str().ok()),
        Some(format!("/custom/v1/chats/{id}").as_str())
    );
    // The default prefix is not mounted.
    let other = send(&h.router, "POST", "/mini-chat/v1/chats", Some(&ctx_a())).await;
    assert_eq!(other.status.as_u16(), 404);
}

// ═════════════════════════════════════════════════════════════════════════════
// GET /v1/chats/{id}
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_returns_the_chat_detail_with_the_live_message_count() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x10).titled("Counted").at(100))
        .await;
    h.seed_message(MessageSeed::user(chat).at(1)).await;
    h.seed_message(MessageSeed::assistant(chat).at(2)).await;
    h.seed_message(MessageSeed::user(chat).at(3).deleted())
        .await;
    // A message of another chat is not counted.
    let other = h.seed_chat(ChatSeed::of_a().with_id(0x11).at(100)).await;
    h.seed_message(MessageSeed::user(other).at(1)).await;

    let resp = h.get(&h.chat_uri(chat), &ctx_a()).await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(key_set(&body), keys(&CHAT_DETAIL_KEYS));
    assert_eq!(body["id"], json!(chat.to_string()));
    assert_eq!(body["model"], json!("gpt-premium"));
    assert_eq!(body["title"], json!("Counted"));
    assert_eq!(body["is_temporary"], json!(false));
    assert_eq!(
        body["message_count"],
        json!(2),
        "soft-deleted messages and other chats' messages are not counted"
    );
    assert_eq!(parse_ts(&body["created_at"]), ts(100));
    assert_eq!(parse_ts(&body["updated_at"]), ts(100));
}

#[tokio::test]
async fn get_omits_the_title_of_an_untitled_chat() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a()).await;
    let body = h.get(&h.chat_uri(chat), &ctx_a()).await.json();
    assert!(body.get("title").is_none(), "{body}");
}

#[tokio::test]
async fn get_unknown_chat_is_404_for_the_chat_resource() {
    let h = chat_harness().await;
    let resp = h.get(&h.chat_uri(Uuid::new_v4()), &ctx_a()).await;
    assert_not_found(&resp, "chat");
}

#[tokio::test]
async fn get_soft_deleted_chat_is_404() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().deleted()).await;
    assert_not_found(&h.get(&h.chat_uri(chat), &ctx_a()).await, "chat");
}

#[tokio::test]
async fn get_keeps_the_chat_model_even_when_the_catalog_no_longer_enables_it() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_model("gpt-retired"))
        .await;
    let resp = h.get(&h.chat_uri(chat), &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert_eq!(resp.json()["model"], json!("gpt-retired"));
    assert_eq!(h.plugin.total_calls(), 0, "reads never consult the catalog");
}

// ═════════════════════════════════════════════════════════════════════════════
// PATCH /v1/chats/{id}
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn patch_renames_the_chat_and_returns_the_updated_detail() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x20).titled("Old").at(100))
        .await;
    h.seed_message(MessageSeed::user(chat).at(1)).await;

    let resp = h
        .patch_json(
            &h.chat_uri(chat),
            &ctx_a(),
            &json!({"title": "Renamed Chat"}),
        )
        .await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(key_set(&body), keys(&CHAT_DETAIL_KEYS));
    assert_eq!(body["title"], json!("Renamed Chat"));
    assert_eq!(body["message_count"], json!(1));
    assert_eq!(
        parse_ts(&body["created_at"]),
        ts(100),
        "created_at never changes"
    );
    assert!(
        parse_ts(&body["updated_at"]) > ts(100),
        "updated_at = now() on rename (DESIGN 3.3): {body}"
    );
    assert_eq!(
        h.get(&h.chat_uri(chat), &ctx_a()).await.json(),
        body,
        "the PATCH response equals a following GET"
    );
    assert_eq!(
        h.chat_row(chat).await.unwrap().title.as_deref(),
        Some("Renamed Chat")
    );
}

#[tokio::test]
async fn patch_trims_the_title_and_can_title_an_untitled_chat() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a()).await;
    let body = h
        .patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": "  Fresh  "}))
        .await
        .json();
    assert_eq!(body["title"], json!("Fresh"));
}

#[tokio::test]
async fn patch_never_changes_the_model_or_other_fields_and_ignores_unknown_fields() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().titled("Old").with_model("gpt-premium"))
        .await;

    let resp = h
        .patch_json(
            &h.chat_uri(chat),
            &ctx_a(),
            &json!({"title": "Renamed", "model": "gpt-standard", "is_temporary": true,
                    "user_id": Uuid::new_v4(), "whatever": [1, 2]}),
        )
        .await;

    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    let body = resp.json();
    assert_eq!(body["title"], json!("Renamed"));
    assert_eq!(body["model"], json!("gpt-premium"), "the model is locked");
    assert_eq!(body["is_temporary"], json!(false));
    let row = h.chat_row(chat).await.unwrap();
    assert_eq!(row.model, "gpt-premium");
    assert!(!row.is_temporary);
    assert_eq!((row.tenant_id, row.user_id), (TENANT_A, USER_A));
}

#[tokio::test]
async fn patch_does_not_validate_or_consult_the_model_catalog() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_model("gpt-retired"))
        .await;
    let resp = h
        .patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": "Still fine"}))
        .await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert_eq!(h.plugin.total_calls(), 0);
}

#[tokio::test]
async fn patch_body_that_does_not_match_the_schema_is_422() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().titled("Keep")).await;
    let uri = h.chat_uri(chat);

    for payload in [
        "{}",
        r#"{"title": null}"#,
        r#"{"title": 42}"#,
        r#"{"title": ["x"]}"#,
        r#"{"title": {"a": 1}}"#,
        r#"{"model": "gpt-standard"}"#,
        "null",
        "[]",
    ] {
        let resp = h
            .raw("PATCH", &uri, &ctx_a(), Some("application/json"), payload)
            .await;
        let problem = assert_problem(&resp, 422, "invalid_argument");
        assert_field_violation(&problem, "body", "invalid_json_body");
    }
    assert_eq!(
        h.chat_row(chat).await.unwrap().title.as_deref(),
        Some("Keep")
    );
}

#[tokio::test]
async fn patch_malformed_json_is_400_and_a_non_json_content_type_is_415() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().titled("Keep")).await;
    let uri = h.chat_uri(chat);

    let resp = h
        .raw(
            "PATCH",
            &uri,
            &ctx_a(),
            Some("application/json"),
            r#"{"title": "x"#,
        )
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "body", "json_syntax_error");

    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let resp = h
            .raw("PATCH", &uri, &ctx_a(), content_type, r#"{"title": "x"}"#)
            .await;
        let problem = assert_problem(&resp, 415, "invalid_argument");
        assert_field_violation(&problem, "body", "missing_json_content_type");
    }
    assert_eq!(
        h.chat_row(chat).await.unwrap().title.as_deref(),
        Some("Keep")
    );
}

#[tokio::test]
async fn patch_with_an_invalid_title_is_400_invalid_title_and_changes_nothing() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x21).titled("Keep").at(100))
        .await;

    for title in [
        String::new(),
        "  \t ".to_owned(),
        title_of(256),
        format!(" {} ", title_of(256)),
    ] {
        let resp = h
            .patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": title}))
            .await;
        let problem = assert_invalid_argument(&resp, "title", "INVALID_TITLE");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.chat.v1~")
        );
    }
    let row = h.chat_row(chat).await.unwrap();
    assert_eq!(row.title.as_deref(), Some("Keep"));
    assert_eq!(
        from_db(row.updated_at),
        ts(100),
        "a rejected rename does not bump updated_at"
    );
}

#[tokio::test]
async fn patch_accepts_titles_up_to_255_characters() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a()).await;
    for title in [title_of(1), title_of(255), "\u{e9}".repeat(255)] {
        let resp = h
            .patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": title}))
            .await;
        assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
        assert_eq!(resp.json()["title"], json!(title));
    }
}

#[tokio::test]
async fn patch_unknown_or_deleted_chat_is_404() {
    let h = chat_harness().await;
    let deleted = h.seed_chat(ChatSeed::of_a().titled("Gone").deleted()).await;
    for id in [Uuid::new_v4(), deleted] {
        let resp = h
            .patch_json(&h.chat_uri(id), &ctx_a(), &json!({"title": "x"}))
            .await;
        assert_not_found(&resp, "chat");
    }
    assert_eq!(
        h.chat_row(deleted).await.unwrap().title.as_deref(),
        Some("Gone")
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// GET /v1/chats - envelope, ordering, pagination
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn list_with_no_chats_is_an_empty_page() {
    let h = chat_harness().await;
    let resp = h.list_chats("", &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let page = resp.json();
    assert_eq!(page["items"], json!([]));
    assert_eq!(page["page_info"]["limit"], json!(20));
    assert_eq!(cursor_of(&page, "next_cursor"), None);
    assert_eq!(cursor_of(&page, "prev_cursor"), None);
}

#[tokio::test]
async fn list_body_is_a_page_of_chat_details() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().titled("Listed").at(10)).await;
    h.seed_message(MessageSeed::user(chat).at(1)).await;

    let page = h.list_chats("", &ctx_a()).await.json();

    assert_eq!(key_set(&page), keys(&["items", "page_info"]));
    assert!(
        key_set(&page["page_info"]).is_subset(&keys(&["limit", "next_cursor", "prev_cursor"])),
        "PageInfo: {}",
        page["page_info"]
    );
    let items = page_items(&page);
    assert_eq!(items.len(), 1);
    assert_eq!(key_set(&items[0]), keys(&CHAT_DETAIL_KEYS));
    assert_eq!(
        items[0],
        h.get(&h.chat_uri(chat), &ctx_a()).await.json(),
        "each item has the shape and content of GET /chats/{{id}}"
    );
    assert_eq!(items[0]["message_count"], json!(1));
}

#[tokio::test]
async fn list_returns_only_live_chats() {
    let h = chat_harness().await;
    let live = h.seed_chat(ChatSeed::of_a().with_id(1).at(10)).await;
    h.seed_chat(ChatSeed::of_a().with_id(2).at(20).deleted())
        .await;
    let page = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_ids(&page), ids_of(&[live]));
}

#[tokio::test]
async fn list_default_order_is_updated_at_descending() {
    let h = chat_harness().await;
    let oldest = h
        .seed_chat(ChatSeed::of_a().with_id(1).at(0).updated_at(100))
        .await;
    let newest = h
        .seed_chat(ChatSeed::of_a().with_id(2).at(0).updated_at(300))
        .await;
    let middle = h
        .seed_chat(ChatSeed::of_a().with_id(3).at(0).updated_at(200))
        .await;
    let page = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_ids(&page), ids_of(&[newest, middle, oldest]));
}

#[tokio::test]
async fn list_orders_by_updated_at_not_created_at() {
    let h = chat_harness().await;
    let created_first_active_last = h
        .seed_chat(ChatSeed::of_a().with_id(1).at(10).updated_at(900))
        .await;
    let created_last_idle = h
        .seed_chat(ChatSeed::of_a().with_id(2).at(500).updated_at(500))
        .await;
    let page = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(
        page_ids(&page),
        ids_of(&[created_first_active_last, created_last_idle])
    );
}

#[tokio::test]
async fn list_ties_on_updated_at_are_broken_deterministically_across_pages() {
    let h = chat_harness().await;
    let mut all = Vec::new();
    for n in 1..=5u128 {
        all.push(h.seed_chat(ChatSeed::of_a().with_id(n).at(100)).await);
    }

    let full = page_ids(&h.list_chats("limit=100", &ctx_a()).await.json());
    assert_eq!(full.len(), 5);
    let again = page_ids(&h.list_chats("limit=100", &ctx_a()).await.json());
    assert_eq!(full, again, "the order is stable");

    // Walking the pages by cursor visits every chat exactly once, in that order.
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..5 {
        let query = match &cursor {
            Some(c) => format!("limit=2&cursor={c}"),
            None => "limit=2".to_owned(),
        };
        let page = h.list_chats(&query, &ctx_a()).await.json();
        walked.extend(page_ids(&page));
        cursor = cursor_of(&page, "next_cursor");
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(walked, full, "id is the tiebreaker: no duplicates, no gaps");
}

#[tokio::test]
async fn list_limit_defaults_to_20_and_is_clamped_to_100() {
    let h = chat_harness().await;
    for n in 1..=105u128 {
        h.seed_chat(ChatSeed::of_a().with_id(n).at(i64::try_from(n).unwrap()))
            .await;
    }

    let default = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_items(&default).len(), 20);
    assert_eq!(default["page_info"]["limit"], json!(20));
    assert!(cursor_of(&default, "next_cursor").is_some());

    let clamped = h.list_chats("limit=1000", &ctx_a()).await;
    assert_eq!(
        clamped.status.as_u16(),
        200,
        "a limit above 100 is clamped, not rejected: {}",
        clamped.raw
    );
    let clamped = clamped.json();
    assert_eq!(page_items(&clamped).len(), 100);
    assert_eq!(clamped["page_info"]["limit"], json!(100));
    assert!(cursor_of(&clamped, "next_cursor").is_some());

    let exact = h.list_chats("limit=100", &ctx_a()).await.json();
    assert_eq!(page_items(&exact).len(), 100);
    assert_eq!(exact["page_info"]["limit"], json!(100));

    let one = h.list_chats("limit=1", &ctx_a()).await.json();
    assert_eq!(page_items(&one).len(), 1);
    assert_eq!(one["page_info"]["limit"], json!(1));
}

#[tokio::test]
async fn list_limit_zero_is_400_invalid_limit() {
    let h = chat_harness().await;
    h.seed_chat(ChatSeed::of_a()).await;
    let resp = h.list_chats("limit=0", &ctx_a()).await;
    assert_odata_problem(&resp, "$top", "INVALID_LIMIT");
}

#[tokio::test]
async fn list_non_numeric_or_negative_limit_is_a_400_odata_error() {
    let h = chat_harness().await;
    for query in ["limit=abc", "limit=-1", "limit=1.5"] {
        let resp = h.list_chats(query, &ctx_a()).await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.odata.query.v1~"),
            "{query}"
        );
    }
}

#[tokio::test]
async fn list_cursor_pagination_walks_forward_and_backward() {
    let h = chat_harness().await;
    let mut ids = Vec::new();
    for n in 1..=5u128 {
        // chat n was last active at time n: the default order is 5,4,3,2,1.
        ids.push(
            h.seed_chat(ChatSeed::of_a().with_id(n).at(i64::try_from(n).unwrap()))
                .await,
        );
    }
    let [c1, c2, c3, c4, c5] = [ids[0], ids[1], ids[2], ids[3], ids[4]];

    let p1 = h.list_chats("limit=2", &ctx_a()).await.json();
    assert_eq!(page_ids(&p1), ids_of(&[c5, c4]));
    assert_eq!(p1["page_info"]["limit"], json!(2));
    assert_eq!(
        cursor_of(&p1, "prev_cursor"),
        None,
        "first page has no prev"
    );
    let next1 = cursor_of(&p1, "next_cursor").expect("more pages follow");

    let p2 = h
        .list_chats(&format!("limit=2&cursor={next1}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p2), ids_of(&[c3, c2]));
    let prev2 = cursor_of(&p2, "prev_cursor").expect("a middle page has a prev cursor");
    let next2 = cursor_of(&p2, "next_cursor").expect("a middle page has a next cursor");

    let p3 = h
        .list_chats(&format!("limit=2&cursor={next2}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p3), ids_of(&[c1]));
    assert_eq!(cursor_of(&p3, "next_cursor"), None, "last page has no next");
    let prev3 = cursor_of(&p3, "prev_cursor").expect("last page has a prev cursor");

    let back_to_p2 = h
        .list_chats(&format!("limit=2&cursor={prev3}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&back_to_p2), ids_of(&[c3, c2]));
    let back_to_p1 = h
        .list_chats(&format!("limit=2&cursor={prev2}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&back_to_p1), ids_of(&[c5, c4]));
}

#[tokio::test]
async fn list_cursor_stays_valid_when_a_chat_is_created_in_between() {
    let h = chat_harness().await;
    for n in 1..=4u128 {
        h.seed_chat(ChatSeed::of_a().with_id(n).at(i64::try_from(n).unwrap()))
            .await;
    }
    let p1 = h.list_chats("limit=2", &ctx_a()).await.json();
    let next = cursor_of(&p1, "next_cursor").unwrap();

    h.create_chat(&ctx_a(), &json!({"title": "newest"})).await;

    let p2 = h
        .list_chats(&format!("limit=2&cursor={next}"), &ctx_a())
        .await
        .json();
    assert_eq!(
        page_ids(&p2),
        ids_of(&[uid(2), uid(1)]),
        "keyset pagination continues after the cursor position"
    );
}

#[tokio::test]
async fn list_malformed_cursor_is_400_invalid_cursor() {
    let h = chat_harness().await;
    h.seed_chat(ChatSeed::of_a()).await;
    for cursor in ["not-a-cursor!!", "e30", "AAAA"] {
        let resp = h.list_chats(&format!("cursor={cursor}"), &ctx_a()).await;
        assert_odata_problem(&resp, "cursor", "INVALID_CURSOR");
    }
}

#[tokio::test]
async fn list_cursor_combined_with_orderby_is_400_order_with_cursor() {
    let h = chat_harness().await;
    for n in 1..=3u128 {
        h.seed_chat(ChatSeed::of_a().with_id(n).at(i64::try_from(n).unwrap()))
            .await;
    }
    let next = cursor_of(
        &h.list_chats("limit=1", &ctx_a()).await.json(),
        "next_cursor",
    )
    .unwrap();
    let resp = h
        .list_chats(
            &format!("limit=1&cursor={next}&$orderby=title%20asc"),
            &ctx_a(),
        )
        .await;
    let problem = assert_odata_problem(&resp, "cursor", "ORDER_WITH_CURSOR");
    assert_violation_reason(&problem, "ORDER_WITH_CURSOR");
}

#[tokio::test]
async fn list_cursor_of_another_filter_is_400_filter_mismatch() {
    let h = chat_harness().await;
    for (n, title) in [(1u128, "alpha"), (2, "beta"), (3, "gamma")] {
        h.seed_chat(
            ChatSeed::of_a()
                .with_id(n)
                .titled(title)
                .at(i64::try_from(n).unwrap()),
        )
        .await;
    }
    let first = h
        .list_chats("limit=1&$filter=title%20ne%20'zzz'", &ctx_a())
        .await
        .json();
    let next = cursor_of(&first, "next_cursor").expect("more results");

    let resp = h
        .list_chats(
            &format!("limit=1&cursor={next}&$filter=title%20ne%20'yyy'"),
            &ctx_a(),
        )
        .await;
    assert_odata_problem(&resp, "cursor", "FILTER_MISMATCH");

    // The same filter continues fine.
    let ok = h
        .list_chats(
            &format!("limit=1&cursor={next}&$filter=title%20ne%20'zzz'"),
            &ctx_a(),
        )
        .await;
    assert_eq!(ok.status.as_u16(), 200, "{}", ok.raw);
}

// ── $orderby ────────────────────────────────────────────────────────────────

async fn titled_trio(h: &chat_support::ChatHarness) -> [Uuid; 3] {
    [
        h.seed_chat(ChatSeed::of_a().with_id(1).titled("Gamma notes").at(100))
            .await,
        h.seed_chat(ChatSeed::of_a().with_id(2).titled("Alpha plan").at(300))
            .await,
        h.seed_chat(ChatSeed::of_a().with_id(3).titled("Beta plan").at(200))
            .await,
    ]
}

#[tokio::test]
async fn list_orderby_title_ascending_and_descending() {
    let h = chat_harness().await;
    let [gamma, alpha, beta] = titled_trio(&h).await;

    let asc = h.list_chats("$orderby=title%20asc", &ctx_a()).await;
    assert_eq!(asc.status.as_u16(), 200, "{}", asc.raw);
    assert_eq!(page_ids(&asc.json()), ids_of(&[alpha, beta, gamma]));

    let desc = h.list_chats("$orderby=title%20desc", &ctx_a()).await.json();
    assert_eq!(page_ids(&desc), ids_of(&[gamma, beta, alpha]));
}

#[tokio::test]
async fn list_orderby_updated_at_ascending_and_by_id() {
    let h = chat_harness().await;
    let [gamma, alpha, beta] = titled_trio(&h).await;

    let asc = h
        .list_chats("$orderby=updated_at%20asc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&asc), ids_of(&[gamma, beta, alpha]));

    let by_id = h.list_chats("$orderby=id%20asc", &ctx_a()).await.json();
    assert_eq!(page_ids(&by_id), ids_of(&[gamma, alpha, beta]));
    let by_id_desc = h.list_chats("$orderby=id%20desc", &ctx_a()).await.json();
    assert_eq!(page_ids(&by_id_desc), ids_of(&[beta, alpha, gamma]));
}

#[tokio::test]
async fn list_orderby_supports_cursor_pagination() {
    let h = chat_harness().await;
    let [gamma, alpha, beta] = titled_trio(&h).await;
    let p1 = h
        .list_chats("limit=2&$orderby=title%20asc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p1), ids_of(&[alpha, beta]));
    let next = cursor_of(&p1, "next_cursor").unwrap();
    // The cursor carries the order: no $orderby is sent with it.
    let p2 = h
        .list_chats(&format!("limit=2&cursor={next}"), &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&p2), ids_of(&[gamma]));
}

#[tokio::test]
async fn list_orderby_unknown_field_or_direction_is_400_invalid_orderby_field() {
    let h = chat_harness().await;
    h.seed_chat(ChatSeed::of_a().titled("x")).await;
    for orderby in [
        "model%20asc",
        "message_count%20desc",
        "user_id%20asc",
        "created_at%20asc",
        "nope",
        "title%20sideways",
    ] {
        let resp = h.list_chats(&format!("$orderby={orderby}"), &ctx_a()).await;
        assert_odata_problem(&resp, "$orderby", "INVALID_ORDERBY_FIELD");
    }
}

// ── $filter ─────────────────────────────────────────────────────────────────

async fn filter_fixture(h: &chat_support::ChatHarness) -> [Uuid; 4] {
    // updated_at spread over days so string/instant comparison cannot differ.
    [
        h.seed_chat(ChatSeed::of_a().with_id(1).titled("Alpha plan").at(DAY))
            .await,
        h.seed_chat(ChatSeed::of_a().with_id(2).titled("Beta plan").at(2 * DAY))
            .await,
        h.seed_chat(
            ChatSeed::of_a()
                .with_id(3)
                .titled("Gamma notes")
                .at(3 * DAY),
        )
        .await,
        h.seed_chat(ChatSeed::of_a().with_id(4).titled("Delta").at(4 * DAY))
            .await,
    ]
}

async fn filtered(h: &chat_support::ChatHarness, filter: &str) -> Vec<String> {
    let resp = h
        .list_chats(&format!("$filter={}", filter.replace(' ', "%20")), &ctx_a())
        .await;
    assert_eq!(resp.status.as_u16(), 200, "$filter={filter}: {}", resp.raw);
    page_ids(&resp.json())
}

#[tokio::test]
async fn list_filter_by_title_operators() {
    let h = chat_harness().await;
    let [alpha, beta, gamma, delta] = filter_fixture(&h).await;

    assert_eq!(filtered(&h, "title eq 'Beta plan'").await, ids_of(&[beta]));
    assert_eq!(
        filtered(&h, "title ne 'Beta plan'").await,
        ids_of(&[delta, gamma, alpha])
    );
    assert_eq!(
        filtered(&h, "contains(title,'plan')").await,
        ids_of(&[beta, alpha])
    );
    assert_eq!(
        filtered(&h, "startswith(title,'Gam')").await,
        ids_of(&[gamma])
    );
    assert_eq!(
        filtered(&h, "endswith(title,'notes')").await,
        ids_of(&[gamma])
    );
    assert_eq!(
        filtered(&h, "title in ('Alpha plan','Delta')").await,
        ids_of(&[delta, alpha])
    );
    assert!(filtered(&h, "title eq 'nothing'").await.is_empty());
}

#[tokio::test]
async fn list_filter_by_updated_at_operators() {
    let h = chat_harness().await;
    let [alpha, beta, gamma, delta] = filter_fixture(&h).await;
    // 2025-01-03T12:00:00Z lies between chat 2 (day 2) and chat 3 (day 3).
    let mid = "2025-01-03T12:00:00Z";

    assert_eq!(
        filtered(&h, &format!("updated_at gt {mid}")).await,
        ids_of(&[delta, gamma])
    );
    assert_eq!(
        filtered(&h, &format!("updated_at ge {mid}")).await,
        ids_of(&[delta, gamma])
    );
    assert_eq!(
        filtered(&h, &format!("updated_at lt {mid}")).await,
        ids_of(&[beta, alpha])
    );
    assert_eq!(
        filtered(&h, &format!("updated_at le {mid}")).await,
        ids_of(&[beta, alpha])
    );
    assert_eq!(
        filtered(
            &h,
            &format!("updated_at gt 2025-01-02T12:00:00Z and updated_at lt {mid}")
        )
        .await,
        ids_of(&[beta]),
        "only chat 2 (2025-01-03T00:00:00Z) is inside the window"
    );
}

#[tokio::test]
async fn list_filter_by_id_operators() {
    let h = chat_harness().await;
    let [alpha, beta, gamma, delta] = filter_fixture(&h).await;

    assert_eq!(
        filtered(&h, &format!("id eq {beta}")).await,
        ids_of(&[beta])
    );
    assert_eq!(
        filtered(&h, &format!("id ne {beta}")).await,
        ids_of(&[delta, gamma, alpha])
    );
    assert_eq!(
        filtered(&h, &format!("id in ({alpha},{gamma})")).await,
        ids_of(&[gamma, alpha])
    );
}

#[tokio::test]
async fn list_filter_combines_with_and_or_and_ordering() {
    let h = chat_harness().await;
    let [alpha, beta, _gamma, delta] = filter_fixture(&h).await;

    assert_eq!(
        filtered(&h, "contains(title,'plan') and title ne 'Alpha plan'").await,
        ids_of(&[beta])
    );
    assert_eq!(
        filtered(&h, "title eq 'Delta' or title eq 'Alpha plan'").await,
        ids_of(&[delta, alpha])
    );
    let resp = h
        .list_chats(
            "$filter=contains(title,'plan')&$orderby=title%20asc",
            &ctx_a(),
        )
        .await;
    assert_eq!(page_ids(&resp.json()), ids_of(&[alpha, beta]));
}

#[tokio::test]
async fn list_filter_never_widens_to_other_users_chats() {
    let h = chat_harness().await;
    let mine = h
        .seed_chat(ChatSeed::of_a().with_id(1).titled("shared title"))
        .await;
    h.seed_chat(
        ChatSeed::new(chat_support::TENANT_B, chat_support::USER_B)
            .with_id(2)
            .titled("shared title"),
    )
    .await;
    h.seed_chat(
        ChatSeed::new(TENANT_A, chat_support::USER_A2)
            .with_id(3)
            .titled("shared title"),
    )
    .await;
    assert_eq!(
        filtered(&h, "title eq 'shared title'").await,
        ids_of(&[mine])
    );
}

#[tokio::test]
async fn list_filter_with_an_unknown_field_or_bad_syntax_is_400_invalid_filter() {
    let h = chat_harness().await;
    h.seed_chat(ChatSeed::of_a().titled("x")).await;
    for filter in [
        "model eq 'gpt-premium'",
        "user_id eq 5",
        "message_count gt 1",
        "tenant_id eq 5",
        "title eq",
        "title eqq 'x'",
        "title eq 'unterminated",
        "updated_at gt notadate",
        "((title eq 'x')",
        "id eq not-a-uuid",
    ] {
        let resp = h
            .list_chats(&format!("$filter={}", filter.replace(' ', "%20")), &ctx_a())
            .await;
        assert_odata_problem(&resp, "$filter", "INVALID_FILTER");
    }
}

// ── $select and unsupported options ─────────────────────────────────────────

#[tokio::test]
async fn list_select_is_accepted_and_ignored() {
    let h = chat_harness().await;
    h.seed_chat(ChatSeed::of_a().titled("Selected")).await;
    let full = h.list_chats("", &ctx_a()).await.json();

    for select in ["id", "id,title", "title%2Cmodel"] {
        let resp = h.list_chats(&format!("$select={select}"), &ctx_a()).await;
        assert_eq!(resp.status.as_u16(), 200, "$select={select}: {}", resp.raw);
        assert_eq!(
            resp.json(),
            full,
            "the full item is always returned (DESIGN 3.3 List Chats)"
        );
    }
}

#[tokio::test]
async fn list_select_with_bad_syntax_is_400_invalid_select() {
    let h = chat_harness().await;
    for select in ["", "%20", "id,id", ","] {
        let resp = h.list_chats(&format!("$select={select}"), &ctx_a()).await;
        assert_odata_problem(&resp, "$select", "INVALID_SELECT");
    }
}

#[tokio::test]
async fn list_unsupported_odata_options_are_400_unsupported_query_param() {
    let h = chat_harness().await;
    for option in ["$skip=1", "$count=true", "$expand=x", "$bogus=1"] {
        let resp = h.list_chats(option, &ctx_a()).await;
        let name = option.split('=').next().unwrap();
        assert_odata_problem(&resp, name, "UNSUPPORTED_QUERY_PARAM");
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// ordering reflects activity (message sends bump updated_at in a later slice)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn renaming_a_chat_moves_it_to_the_top_of_the_list() {
    let h = chat_harness().await;
    let oldest = h
        .seed_chat(ChatSeed::of_a().with_id(1).titled("old").at(100))
        .await;
    let middle = h
        .seed_chat(ChatSeed::of_a().with_id(2).titled("mid").at(200))
        .await;
    let newest = h
        .seed_chat(ChatSeed::of_a().with_id(3).titled("new").at(300))
        .await;
    assert_eq!(
        page_ids(&h.list_chats("", &ctx_a()).await.json()),
        ids_of(&[newest, middle, oldest])
    );

    let resp = h
        .patch_json(
            &h.chat_uri(oldest),
            &ctx_a(),
            &json!({"title": "old, renamed"}),
        )
        .await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);

    assert_eq!(
        page_ids(&h.list_chats("", &ctx_a()).await.json()),
        ids_of(&[oldest, newest, middle]),
        "rename bumps updated_at, the chat is now the most recently active"
    );
}

#[tokio::test]
async fn a_newly_created_chat_is_listed_first() {
    let h = chat_harness().await;
    let old = h.seed_chat(ChatSeed::of_a().with_id(1).at(100)).await;
    let created = h.create_chat(&ctx_a(), &json!({"title": "fresh"})).await;
    let page = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(
        page_ids(&page),
        vec![created["id"].as_str().unwrap().to_owned(), old.to_string()]
    );
}

#[tokio::test]
async fn a_failed_rename_does_not_reorder_the_list() {
    let h = chat_harness().await;
    let old = h.seed_chat(ChatSeed::of_a().with_id(1).at(100)).await;
    let new = h.seed_chat(ChatSeed::of_a().with_id(2).at(200)).await;
    let resp = h
        .patch_json(&h.chat_uri(old), &ctx_a(), &json!({"title": " "}))
        .await;
    assert_eq!(resp.status.as_u16(), 400);
    assert_eq!(
        page_ids(&h.list_chats("", &ctx_a()).await.json()),
        ids_of(&[new, old])
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// DELETE /v1/chats/{id}
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delete_returns_204_without_a_body_and_soft_deletes_the_chat() {
    let h = chat_harness().await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x30).titled("Bye"))
        .await;

    let resp = h.delete(&h.chat_uri(chat), &ctx_a()).await;

    assert_eq!(resp.status.as_u16(), 204, "{}", resp.raw);
    assert!(resp.raw.is_empty(), "204 has no body: {:?}", resp.raw);
    let row = h.chat_row(chat).await.expect("soft delete keeps the row");
    assert!(row.deleted_at.is_some(), "deleted_at is set");
    assert_eq!(row.title.as_deref(), Some("Bye"));
}

#[tokio::test]
async fn delete_leaves_messages_reactions_and_attachment_rows_untouched() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x31)).await;
    let user_msg = h.seed_message(MessageSeed::user(chat).at(1)).await;
    let assistant_msg = h.seed_message(MessageSeed::assistant(chat).at(2)).await;
    h.seed_reaction(TENANT_A, USER_A, assistant_msg, "like")
        .await;
    let attachment = h
        .seed_attachment(AttachmentSeed::document(chat, "doc.pdf").with_id(0x3100))
        .await;
    h.link_attachment(chat, user_msg, attachment).await;

    let resp = h.delete(&h.chat_uri(chat), &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 204, "{}", resp.raw);

    let messages = h.all_messages().await;
    assert_eq!(messages.len(), 2);
    assert!(
        messages.iter().all(|m| m.deleted_at.is_none()),
        "child rows are not modified (DESIGN 3.3 Delete Chat, ADR-0009)"
    );
    assert_eq!(h.reactions_of(assistant_msg).await.len(), 1);
    assert!(h.attachment_row(attachment).await.deleted_at.is_none());
}

#[tokio::test]
async fn deleted_chat_is_hidden_from_get_patch_delete_and_list() {
    let h = chat_harness().await;
    let keep = h.seed_chat(ChatSeed::of_a().with_id(1).at(100)).await;
    let gone = h.seed_chat(ChatSeed::of_a().with_id(2).at(200)).await;

    assert_eq!(
        h.delete(&h.chat_uri(gone), &ctx_a()).await.status.as_u16(),
        204
    );

    assert_not_found(&h.get(&h.chat_uri(gone), &ctx_a()).await, "chat");
    assert_not_found(
        &h.patch_json(&h.chat_uri(gone), &ctx_a(), &json!({"title": "x"}))
            .await,
        "chat",
    );
    assert_not_found(&h.delete(&h.chat_uri(gone), &ctx_a()).await, "chat");
    assert_eq!(
        page_ids(&h.list_chats("", &ctx_a()).await.json()),
        ids_of(&[keep])
    );
}

#[tokio::test]
async fn second_delete_of_the_same_chat_is_404_and_enqueues_nothing_more() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a()).await;
    assert_eq!(
        h.delete(&h.chat_uri(chat), &ctx_a()).await.status.as_u16(),
        204
    );
    h.cleanup.wait_for(1).await;

    assert_not_found(&h.delete(&h.chat_uri(chat), &ctx_a()).await, "chat");

    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(
        h.cleanup.received().len(),
        1,
        "exactly one cleanup message per chat"
    );
}

#[tokio::test]
async fn delete_of_an_unknown_chat_is_404_and_enqueues_nothing() {
    let h = chat_harness().await;
    assert_not_found(
        &h.delete(&h.chat_uri(Uuid::new_v4()), &ctx_a()).await,
        "chat",
    );
    h.cleanup.assert_none().await;
}

#[tokio::test]
async fn delete_enqueues_one_chat_cleanup_message_with_the_documented_payload() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x32)).await;
    assert_eq!(
        h.delete(&h.chat_uri(chat), &ctx_a()).await.status.as_u16(),
        204
    );

    let delivered = h.cleanup.wait_for(1).await;
    assert_eq!(delivered.len(), 1);
    let payload = &delivered[0];

    // DESIGN 3.6 "Outbox payload and execution semantics": at minimum these keys.
    assert_eq!(payload["tenant_id"], json!(TENANT_A.to_string()));
    assert_eq!(payload["chat_id"], json!(chat.to_string()));
    assert_eq!(payload["reason"], json!("chat_soft_delete"));
    let system_request_id = Uuid::parse_str(payload["system_request_id"].as_str().unwrap())
        .expect("system_request_id is a UUID");
    assert_eq!(
        system_request_id.get_version_num(),
        4,
        "server-generated UUID v4"
    );
    assert_eq!(
        parse_ts(&payload["chat_deleted_at"]),
        from_db(h.chat_row(chat).await.unwrap().deleted_at.unwrap()),
        "chat_deleted_at is the soft-delete timestamp stored on the chat"
    );
}

#[tokio::test]
async fn every_deleted_chat_gets_its_own_cleanup_message() {
    let h = chat_harness().await;
    let a = h.seed_chat(ChatSeed::of_a().with_id(1)).await;
    let b = h.seed_chat(ChatSeed::of_a().with_id(2)).await;
    h.delete(&h.chat_uri(a), &ctx_a()).await;
    h.delete(&h.chat_uri(b), &ctx_a()).await;

    let delivered = h.cleanup.wait_for(2).await;
    let mut chat_ids: Vec<String> = delivered
        .iter()
        .map(|p| p["chat_id"].as_str().unwrap().to_owned())
        .collect();
    chat_ids.sort();
    assert_eq!(chat_ids, ids_of(&[a, b]));
    let request_ids: std::collections::BTreeSet<&str> = delivered
        .iter()
        .map(|p| p["system_request_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        request_ids.len(),
        2,
        "system_request_id is unique per message"
    );
}

#[tokio::test]
async fn delete_marks_every_attachment_of_the_chat_for_cleanup() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x33)).await;
    let other_chat = h.seed_chat(ChatSeed::of_a().with_id(0x34)).await;

    let ready = h
        .seed_attachment(AttachmentSeed::document(chat, "ready.pdf").with_id(0x3301))
        .await;
    let image = h
        .seed_attachment(AttachmentSeed::image(chat, "photo.png").with_id(0x3302))
        .await;
    let uploading = h
        .seed_attachment(
            AttachmentSeed::document(chat, "up.pdf")
                .with_id(0x3303)
                .status("uploaded"),
        )
        .await;
    let pending = h
        .seed_attachment(
            AttachmentSeed::document(chat, "p.pdf")
                .with_id(0x3304)
                .status("pending"),
        )
        .await;
    let failed = h
        .seed_attachment(
            AttachmentSeed::document(chat, "f.pdf")
                .with_id(0x3305)
                .status("failed"),
        )
        .await;
    let foreign = h
        .seed_attachment(AttachmentSeed::document(other_chat, "other.pdf").with_id(0x3401))
        .await;

    assert_eq!(
        h.delete(&h.chat_uri(chat), &ctx_a()).await.status.as_u16(),
        204
    );

    for id in [ready, image, uploading, pending, failed] {
        let row = h.attachment_row(id).await;
        assert_eq!(
            row.cleanup_status.as_deref(),
            Some("pending"),
            "attachment {id} ({}) is marked for cleanup",
            row.status
        );
        assert!(
            row.deleted_at.is_none(),
            "attachment rows stay visible, only the provider resources go (ADR-0009)"
        );
    }
    let untouched = h.attachment_row(foreign).await;
    assert_eq!(
        untouched.cleanup_status, None,
        "other chats are not affected"
    );
}

#[tokio::test]
async fn delete_does_not_reopen_attachments_whose_cleanup_already_finished() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(0x35)).await;
    let done = h
        .seed_attachment(
            AttachmentSeed::document(chat, "done.pdf")
                .with_id(0x3501)
                .cleanup("done")
                .deleted(),
        )
        .await;
    let failed = h
        .seed_attachment(
            AttachmentSeed::document(chat, "failed.pdf")
                .with_id(0x3502)
                .cleanup("failed")
                .deleted(),
        )
        .await;
    let pending = h
        .seed_attachment(
            AttachmentSeed::document(chat, "pending.pdf")
                .with_id(0x3503)
                .cleanup("pending")
                .deleted(),
        )
        .await;

    assert_eq!(
        h.delete(&h.chat_uri(chat), &ctx_a()).await.status.as_u16(),
        204
    );

    // `done` and `failed` are terminal (DESIGN 3.6 attachment cleanup state machine).
    assert_eq!(
        h.attachment_row(done).await.cleanup_status.as_deref(),
        Some("done")
    );
    assert_eq!(
        h.attachment_row(failed).await.cleanup_status.as_deref(),
        Some("failed")
    );
    assert_eq!(
        h.attachment_row(pending).await.cleanup_status.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn delete_is_one_transaction_with_the_cleanup_enqueue() {
    // The chat-cleanup queue is not registered, so the enqueue fails: the
    // soft delete and the attachment marking must roll back with it.
    let h = chat_harness_with(ChatHarnessOptions {
        register_cleanup_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h
        .seed_chat(ChatSeed::of_a().with_id(0x36).titled("Stays"))
        .await;
    let attachment = h
        .seed_attachment(AttachmentSeed::document(chat, "a.pdf").with_id(0x3601))
        .await;

    let resp = h.delete(&h.chat_uri(chat), &ctx_a()).await;

    let problem = assert_problem(&resp, 500, "internal");
    assert!(
        !resp.raw.contains("not registered") && !resp.raw.contains("mini-chat.chat_cleanup"),
        "the wire detail is generic: {problem}"
    );
    let row = h.chat_row(chat).await.unwrap();
    assert!(row.deleted_at.is_none(), "the soft delete was rolled back");
    assert_eq!(h.attachment_row(attachment).await.cleanup_status, None);
    assert_eq!(
        h.get(&h.chat_uri(chat), &ctx_a()).await.status.as_u16(),
        200
    );
}

#[tokio::test]
async fn delete_does_not_cancel_or_modify_other_chats() {
    let h = chat_harness().await;
    let gone = h
        .seed_chat(ChatSeed::of_a().with_id(1).titled("gone"))
        .await;
    let kept = h
        .seed_chat(ChatSeed::of_a().with_id(2).titled("kept").at(5))
        .await;
    h.delete(&h.chat_uri(gone), &ctx_a()).await;
    let row = h.chat_row(kept).await.unwrap();
    assert!(row.deleted_at.is_none());
    assert_eq!(from_db(row.updated_at), ts(5));
}

// ═════════════════════════════════════════════════════════════════════════════
// authorization matrix of the chat endpoints (3.8) and route registration
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn chat_operations_ask_the_pdp_for_their_matrix_action_on_the_chat_resource() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().titled("t")).await;

    // Each call is made right before its PDP request is read back: the
    // recorder keeps the order of calls, so `last()` must follow the call.
    let cases: [(&str, Option<Uuid>); 5] = [
        ("create", None),
        ("list", None),
        ("read", Some(chat)),
        ("update", Some(chat)),
        ("delete", Some(chat)),
    ];
    for (action, id) in cases {
        let resp = match action {
            "create" => h.post_json(&h.chats_uri(), &ctx_a(), &json!({})).await,
            "list" => h.list_chats("", &ctx_a()).await,
            "read" => h.get(&h.chat_uri(chat), &ctx_a()).await,
            "update" => {
                h.patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": "n"}))
                    .await
            }
            _ => h.delete(&h.chat_uri(chat), &ctx_a()).await,
        };
        assert!(resp.status.is_success(), "{action}: {}", resp.raw);
        let req = h.pdp.last();
        assert_eq!(req.action.name, action);
        assert_eq!(req.resource.resource_type, CHAT_PDP_TYPE, "{action}");
        assert_eq!(req.resource.id, id, "{action}: resource.id");
        assert!(req.context.require_constraints, "{action}");
    }
}

#[tokio::test]
async fn create_sends_the_owner_properties_of_the_security_context_to_the_pdp() {
    let h = chat_harness().await;
    h.create_chat(&ctx_a(), &json!({"title": "x", "user_id": Uuid::new_v4()}))
        .await;
    let req = h.pdp.last();
    assert_eq!(req.action.name, "create");
    assert_eq!(
        req.resource.properties["owner_tenant_id"],
        json!(TENANT_A.to_string())
    );
    assert_eq!(
        req.resource.properties["owner_id"],
        json!(USER_A.to_string())
    );
}

#[tokio::test]
async fn chat_operations_are_registered_with_stable_ids_auth_and_the_base_license() {
    let h = chat_harness().await;
    for (key, operation_id) in [
        ("POST:/mini-chat/v1/chats", "mini_chat.create_chat"),
        ("GET:/mini-chat/v1/chats", "mini_chat.list_chats"),
        ("GET:/mini-chat/v1/chats/{id}", "mini_chat.get_chat"),
        ("PATCH:/mini-chat/v1/chats/{id}", "mini_chat.update_chat"),
        ("DELETE:/mini-chat/v1/chats/{id}", "mini_chat.delete_chat"),
    ] {
        let spec = h
            .openapi
            .operation_specs
            .get(key)
            .unwrap_or_else(|| panic!("operation `{key}` is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id), "{key}");
        assert!(spec.authenticated, "{key} requires authentication");
        let license = spec
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{key} must require a license feature"));
        assert_eq!(
            license.license_names,
            vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn chat_operations_declare_their_status_codes() {
    let h = chat_harness().await;
    let statuses = |key: &str| -> Vec<u16> {
        let spec = h.openapi.operation_specs.get(key).expect("registered");
        spec.responses.iter().map(|r| r.status).collect()
    };
    let create = statuses("POST:/mini-chat/v1/chats");
    for status in [201, 400, 403, 422, 500, 503] {
        assert!(
            create.contains(&status),
            "create declares {status}: {create:?}"
        );
    }
    assert!(statuses("GET:/mini-chat/v1/chats").contains(&200));
    assert!(statuses("GET:/mini-chat/v1/chats").contains(&400));
    for (key, ok) in [
        ("GET:/mini-chat/v1/chats/{id}", 200),
        ("PATCH:/mini-chat/v1/chats/{id}", 200),
        ("DELETE:/mini-chat/v1/chats/{id}", 204),
    ] {
        let got = statuses(key);
        assert!(got.contains(&ok), "{key}: {got:?}");
        assert!(got.contains(&404), "{key} declares 404: {got:?}");
        assert!(got.contains(&400), "{key} declares 400: {got:?}");
    }
    assert!(statuses("PATCH:/mini-chat/v1/chats/{id}").contains(&422));
}

#[tokio::test]
async fn create_declares_the_location_header_on_201() {
    let h = chat_harness().await;
    let spec = h
        .openapi
        .operation_specs
        .get("POST:/mini-chat/v1/chats")
        .expect("registered");
    let created = spec
        .responses
        .iter()
        .find(|r| r.status == 201)
        .expect("201 is declared");
    assert!(
        created
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("location")),
        "the 201 response declares the Location header"
    );
}

#[tokio::test]
async fn create_is_bound_to_the_chat_throttling_zones() {
    let h = chat_harness().await;
    let spec = h
        .openapi
        .operation_specs
        .get("POST:/mini-chat/v1/chats")
        .expect("registered");
    let throttling = spec
        .throttling
        .as_ref()
        .expect("create_chat carries a throttling spec");
    assert_eq!(
        throttling.rate_limit_zone.as_deref(),
        Some("rl_mini_chat_chat")
    );
    assert_eq!(
        throttling.in_flight_limit_zone.as_deref(),
        Some("ifl_mini_chat_chat")
    );
}

#[tokio::test]
async fn list_chats_publishes_the_odata_filter_and_orderby_fields() {
    let h = chat_harness().await;
    let spec = h
        .openapi
        .operation_specs
        .get("GET:/mini-chat/v1/chats")
        .expect("registered");
    let filter = spec
        .vendor_extensions
        .x_odata_filter
        .as_ref()
        .expect("x-odata-filter");
    assert_eq!(
        filter.allowed_fields.keys().cloned().collect::<Vec<_>>(),
        vec!["id".to_owned(), "title".to_owned(), "updated_at".to_owned()]
    );
    let orderby = spec
        .vendor_extensions
        .x_odata_orderby
        .as_ref()
        .expect("x-odata-orderby");
    let mut got = orderby.allowed_fields.clone();
    got.sort();
    let mut want: Vec<String> = [
        "id asc",
        "id desc",
        "title asc",
        "title desc",
        "updated_at asc",
        "updated_at desc",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    want.sort();
    assert_eq!(got, want);
}

#[tokio::test]
async fn routes_exist_only_under_the_url_prefix_and_need_a_security_context() {
    let h = chat_harness().await;
    assert_eq!(
        send(&h.router, "GET", "/v1/chats", Some(&ctx_a()))
            .await
            .status
            .as_u16(),
        404
    );

    // The api-gateway answers 401 before the handler; a handler reached
    // without an authenticated context must never serve or create anything.
    let resp = send(&h.router, "GET", &h.chats_uri(), None).await;
    assert!(!resp.status.is_success(), "status {}", resp.status);
    let resp = send_body(
        &h.router,
        "POST",
        &h.chats_uri(),
        None,
        Some("application/json"),
        r#"{"title":"x"}"#,
    )
    .await;
    assert!(!resp.status.is_success(), "status {}", resp.status);
    assert!(h.all_chats().await.is_empty());
}

// ── $orderby=title over untitled chats (NULL-free sort key) ─────────────────

/// Every page of a `limit=1` walk in `direction` (`next_cursor` forward from
/// `query`, or `prev_cursor` backward from the page holding `start`).
async fn walk_pages(
    h: &chat_support::ChatHarness,
    first_query: &str,
    cursor_name: &str,
) -> Vec<(Vec<String>, serde_json::Value)> {
    let mut pages = Vec::new();
    let mut query = first_query.to_owned();
    for _ in 0..50 {
        let page = h.list_chats(&query, &ctx_a()).await;
        assert_eq!(page.status.as_u16(), 200, "{query}: {}", page.raw);
        let page = page.json();
        let next = cursor_of(&page, cursor_name);
        pages.push((page_ids(&page), page));
        match next {
            Some(cursor) => query = format!("limit=1&cursor={cursor}"),
            None => return pages,
        }
    }
    panic!("pagination did not terminate");
}

async fn mixed_title_chats(h: &chat_support::ChatHarness) -> ([Uuid; 3], Uuid, Uuid) {
    let untitled = [
        h.seed_chat(ChatSeed::of_a().with_id(1).at(100)).await,
        h.seed_chat(ChatSeed::of_a().with_id(2).at(200)).await,
        h.seed_chat(ChatSeed::of_a().with_id(3).at(300)).await,
    ];
    let alpha = h
        .seed_chat(ChatSeed::of_a().with_id(4).titled("Alpha").at(400))
        .await;
    let beta = h
        .seed_chat(ChatSeed::of_a().with_id(5).titled("Beta").at(500))
        .await;
    (untitled, alpha, beta)
}

#[tokio::test]
async fn orderby_title_walk_visits_untitled_and_titled_chats_exactly_once() {
    let h = chat_harness().await;
    let ([u1, u2, u3], alpha, beta) = mixed_title_chats(&h).await;

    // Untitled chats sort as the empty title; ties break by id descending.
    let cases = [
        ("title%20asc", ids_of(&[u3, u2, u1, alpha, beta])),
        ("title%20desc", ids_of(&[beta, alpha, u3, u2, u1])),
    ];
    for (orderby, expected) in cases {
        let full = h
            .list_chats(&format!("limit=100&$orderby={orderby}"), &ctx_a())
            .await
            .json();
        assert_eq!(page_ids(&full), expected, "$orderby={orderby} single page");

        // Forward over next cursors.
        let forward = walk_pages(&h, &format!("limit=1&$orderby={orderby}"), "next_cursor").await;
        let walked: Vec<String> = forward.iter().flat_map(|(ids, _)| ids.clone()).collect();
        assert_eq!(walked, expected, "$orderby={orderby} forward walk");

        // Backward over prev cursors, from the last page.
        let last = forward.last().expect("a page").1.clone();
        let mut backward = page_ids(&last);
        let mut prev = cursor_of(&last, "prev_cursor");
        let mut guard = 0;
        while let Some(cursor) = prev {
            let page = h
                .list_chats(&format!("limit=1&cursor={cursor}"), &ctx_a())
                .await
                .json();
            backward.splice(0..0, page_ids(&page));
            prev = cursor_of(&page, "prev_cursor");
            guard += 1;
            assert!(guard < 50, "backward walk did not terminate");
        }
        assert_eq!(backward, expected, "$orderby={orderby} backward walk");
    }
}

#[tokio::test]
async fn orderby_title_walk_with_pages_of_two_has_no_gaps_or_duplicates() {
    let h = chat_harness().await;
    let ([u1, u2, u3], alpha, beta) = mixed_title_chats(&h).await;
    let mut walked = Vec::new();
    let mut query = "limit=2&$orderby=title%20asc".to_owned();
    for _ in 0..10 {
        let page = h.list_chats(&query, &ctx_a()).await.json();
        walked.extend(page_ids(&page));
        match cursor_of(&page, "next_cursor") {
            Some(cursor) => query = format!("limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(walked, ids_of(&[u3, u2, u1, alpha, beta]));
}

#[tokio::test]
async fn renaming_an_untitled_chat_moves_it_between_the_title_groups() {
    let h = chat_harness().await;
    let ([u1, u2, u3], alpha, beta) = mixed_title_chats(&h).await;
    let resp = h
        .patch_json(&h.chat_uri(u2), &ctx_a(), &json!({"title": "Aardvark"}))
        .await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    let walked: Vec<String> = walk_pages(&h, "limit=1&$orderby=title%20asc", "next_cursor")
        .await
        .into_iter()
        .flat_map(|(ids, _)| ids)
        .collect();
    assert_eq!(walked, ids_of(&[u3, u1, u2, alpha, beta]));
}

// ── timestamps: exact $filter and chronological order ───────────────────────

/// Five chats whose `updated_at` instants share one second but have different
/// fraction widths; chat `n` (1..=5) is the n-th instant in ascending order.
/// Returns the ids and the `updated_at` strings exactly as the API renders them.
async fn mixed_width_chats(h: &chat_support::ChatHarness) -> (Vec<Uuid>, Vec<String>) {
    let instants = mixed_width_instants();
    // Insert out of chronological order so insertion order cannot hide a bad sort.
    let mut ids = vec![Uuid::nil(); 5];
    for index in [3usize, 0, 4, 2, 1] {
        ids[index] = h
            .seed_chat(
                ChatSeed::of_a()
                    .with_id(u128::try_from(index + 1).unwrap())
                    .at_time(instants[index]),
            )
            .await;
    }
    let page = h.list_chats("limit=100", &ctx_a()).await.json();
    let mut stamps = vec![String::new(); 5];
    for item in page_items(&page) {
        let id = Uuid::parse_str(item["id"].as_str().unwrap()).unwrap();
        let index = ids.iter().position(|i| *i == id).unwrap();
        item["updated_at"]
            .as_str()
            .unwrap()
            .clone_into(&mut stamps[index]);
        assert_eq!(parse_ts(&item["updated_at"]), instants[index]);
    }
    (ids, stamps)
}

fn desc_ids(ids: &[Uuid], indexes: impl IntoIterator<Item = usize>) -> Vec<String> {
    let mut picked: Vec<usize> = indexes.into_iter().collect();
    picked.sort_unstable_by(|a, b| b.cmp(a));
    picked.into_iter().map(|i| ids[i].to_string()).collect()
}

#[tokio::test]
async fn updated_at_filters_are_exact_on_the_timestamps_the_api_returns() {
    let h = chat_harness().await;
    let (ids, stamps) = mixed_width_chats(&h).await;
    for (i, stamp) in stamps.iter().enumerate() {
        let all = 0..5usize;
        // The default order is updated_at descending.
        assert_eq!(
            filtered(&h, &format!("updated_at eq {stamp}")).await,
            desc_ids(&ids, [i]),
            "eq {stamp}"
        );
        assert_eq!(
            filtered(&h, &format!("updated_at ne {stamp}")).await,
            desc_ids(&ids, all.clone().filter(|j| *j != i)),
            "ne {stamp}"
        );
        assert_eq!(
            filtered(&h, &format!("updated_at gt {stamp}")).await,
            desc_ids(&ids, all.clone().filter(|j| *j > i)),
            "gt {stamp}"
        );
        assert_eq!(
            filtered(&h, &format!("updated_at ge {stamp}")).await,
            desc_ids(&ids, all.clone().filter(|j| *j >= i)),
            "ge {stamp}"
        );
        assert_eq!(
            filtered(&h, &format!("updated_at lt {stamp}")).await,
            desc_ids(&ids, all.clone().filter(|j| *j < i)),
            "lt {stamp}"
        );
        assert_eq!(
            filtered(&h, &format!("updated_at le {stamp}")).await,
            desc_ids(&ids, all.filter(|j| *j <= i)),
            "le {stamp}"
        );
    }
    assert_eq!(
        filtered(
            &h,
            &format!("updated_at in ({},{},{})", stamps[0], stamps[2], stamps[4])
        )
        .await,
        desc_ids(&ids, [0, 2, 4])
    );
}

#[tokio::test]
async fn updated_at_filters_agree_with_other_instant_spellings() {
    let h = chat_harness().await;
    let (ids, _) = mixed_width_chats(&h).await;
    // The same instants written with other fraction widths / an offset.
    assert_eq!(
        filtered(&h, "updated_at eq 2025-01-01T00:00:10.500000Z").await,
        desc_ids(&ids, [2])
    );
    assert_eq!(
        filtered(&h, "updated_at eq 2025-01-01T01:00:10.5%2B01:00").await,
        desc_ids(&ids, [2])
    );
    assert_eq!(
        filtered(&h, "updated_at gt 2025-01-01T00:00:10.5Z").await,
        desc_ids(&ids, [3, 4])
    );
    assert_eq!(
        filtered(&h, "updated_at le 2025-01-01T00:00:10Z").await,
        desc_ids(&ids, [0])
    );
}

#[tokio::test]
async fn same_second_timestamps_of_different_fraction_widths_are_ordered_chronologically() {
    let h = chat_harness().await;
    let (ids, _) = mixed_width_chats(&h).await;
    let ascending: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let descending: Vec<String> = ascending.iter().rev().cloned().collect();

    let asc = h
        .list_chats("$orderby=updated_at%20asc", &ctx_a())
        .await
        .json();
    assert_eq!(page_ids(&asc), ascending);
    let default = h.list_chats("", &ctx_a()).await.json();
    assert_eq!(page_ids(&default), descending, "default is newest first");

    for (orderby, expected) in [("asc", &ascending), ("desc", &descending)] {
        let walked: Vec<String> = walk_pages(
            &h,
            &format!("limit=1&$orderby=updated_at%20{orderby}"),
            "next_cursor",
        )
        .await
        .into_iter()
        .flat_map(|(page, _)| page)
        .collect();
        assert_eq!(&walked, expected, "updated_at {orderby} keyset walk");
    }
}

#[tokio::test]
async fn a_rename_keeps_the_updated_at_filters_exact() {
    let h = chat_harness().await;
    let chat = h.seed_chat(ChatSeed::of_a().with_id(1).at(10)).await;
    let resp = h
        .patch_json(&h.chat_uri(chat), &ctx_a(), &json!({"title": "Renamed"}))
        .await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    let stamp = resp.json()["updated_at"].as_str().unwrap().to_owned();
    assert_eq!(
        filtered(&h, &format!("updated_at eq {stamp}")).await,
        ids_of(&[chat])
    );
    assert_eq!(
        filtered(&h, &format!("updated_at ge {stamp}")).await,
        ids_of(&[chat])
    );
    assert!(
        filtered(&h, &format!("updated_at gt {stamp}"))
            .await
            .is_empty()
    );
    assert!(
        filtered(&h, &format!("updated_at lt {stamp}"))
            .await
            .is_empty()
    );
}
