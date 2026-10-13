#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slices 2 to 4 of `cpt-cf-mini-chat-feature-chat-crud` through the REST layer:
//! chat create, get and rename; shared pagination and the chat list; chat
//! delete and the chat-cleanup enqueue. Every test runs on a migrated `SQLite`
//! database with a fake PDP and a fake model-policy plugin.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use mini_chat_sdk::PolicyPluginError;
use sea_orm::ConnectionTrait;
use serde_json::{Value, json};
use tracing::Level;
use uuid::Uuid;

use crate::chat_test_env::{
    CHAT_CLEANUP_QUEUE, CHAT_PEP_TYPE, CHAT_TYPE, ChatEnv, MESSAGE_TYPE, Options, OutboxMode,
    att_id, blob, catalog_entry, chat_id, chat_uri, enc, item_ids, msg_id, next_cursor, parse_ts,
    rfc3339, ts,
};
use crate::test_doubles::{
    Behavior, LogCapture, OTHER_TENANT, OTHER_USER, POLICY_VERSION, PdpReply, USER, USER_TENANT,
    other_tenant_ctx, other_user_ctx, user_ctx,
};

const CHAT_KEYS: [&str; 7] = [
    "id",
    "model",
    "title",
    "is_temporary",
    "message_count",
    "created_at",
    "updated_at",
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

fn qblob(id: Uuid) -> String {
    blob(id).to_uppercase()
}

async fn chat_column(env: &ChatEnv, id: Uuid, column: &str) -> String {
    env.text(&format!(
        "SELECT quote({column}) FROM chats WHERE id = {}",
        blob(id)
    ))
    .await
}

/// Every column of the chat row except `title` and `updated_at`.
async fn chat_stable_columns(env: &ChatEnv, id: Uuid) -> String {
    env.text(&format!(
        "SELECT quote(id)||quote(tenant_id)||quote(user_id)||quote(model)\
         ||quote(is_temporary)||quote(created_at)||quote(deleted_at) \
         FROM chats WHERE id = {}",
        blob(id)
    ))
    .await
}

async fn chat_count(env: &ChatEnv) -> i64 {
    env.db.count("chats").await
}

fn uuid_of(value: &Value) -> Uuid {
    Uuid::parse_str(value.as_str().unwrap()).unwrap()
}

async fn create(env: &ChatEnv, body: &Value) -> Uuid {
    let reply = env.post_chat(body).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    uuid_of(&reply.json["id"])
}

fn assert_no_leak(reply_text: &str) {
    for forbidden in ["user_id", "tenant_id", "deleted_at"] {
        assert!(
            !reply_text.contains(forbidden),
            "`{forbidden}` leaked: {reply_text}"
        );
    }
}

// ================================================================================================
// Slice 2: create
// ================================================================================================

#[tokio::test]
async fn create_returns_201_with_exactly_the_chat_detail_keys_and_a_location_header() {
    let env = ChatEnv::new().await;
    let before = chrono::Utc::now();

    let reply = env
        .post_chat(&json!({ "title": "  Q3 report  ", "model": "model-b" }))
        .await;

    let after = chrono::Utc::now();
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(keys(&reply.json), key_set(&CHAT_KEYS), "{}", reply.text);
    let id = uuid_of(&reply.json["id"]);
    assert_eq!(reply.json["title"], "Q3 report");
    assert_eq!(reply.json["model"], "model-b");
    assert_eq!(reply.json["is_temporary"], false);
    assert_eq!(reply.json["message_count"], 0);
    assert_eq!(reply.json["created_at"], reply.json["updated_at"]);
    let created = parse_ts(reply.json["created_at"].as_str().unwrap());
    assert!(
        created >= before - chrono::Duration::seconds(2),
        "{created}"
    );
    assert!(created <= after + chrono::Duration::seconds(2), "{created}");
    assert_eq!(
        reply.header("location"),
        Some(format!("/mini-chat/v1/chats/{id}").as_str())
    );
    assert_no_leak(&reply.text);
}

#[tokio::test]
async fn create_stores_the_chat_for_the_caller_with_the_trimmed_title() {
    let env = ChatEnv::new().await;

    let id = create(&env, &json!({ "title": "  Q3  ", "model": "model-b" })).await;

    assert_eq!(chat_count(&env).await, 1);
    assert_eq!(chat_column(&env, id, "tenant_id").await, qblob(USER_TENANT));
    assert_eq!(chat_column(&env, id, "user_id").await, qblob(USER));
    assert_eq!(chat_column(&env, id, "title").await, "'Q3'");
    assert_eq!(chat_column(&env, id, "model").await, "'model-b'");
    assert_eq!(chat_column(&env, id, "is_temporary").await, "0");
    assert_eq!(chat_column(&env, id, "deleted_at").await, "NULL");
    assert_eq!(
        chat_column(&env, id, "created_at").await,
        chat_column(&env, id, "updated_at").await
    );
}

#[tokio::test]
async fn create_without_title_and_model_uses_the_default_model_and_omits_the_title_key() {
    let env = ChatEnv::new().await;

    let reply = env.post_chat(&json!({})).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(
        keys(&reply.json),
        key_set(&[
            "id",
            "model",
            "is_temporary",
            "message_count",
            "created_at",
            "updated_at"
        ]),
        "title is omitted, never null: {}",
        reply.text
    );
    assert_eq!(reply.json["model"], "model-a");
    let id = uuid_of(&reply.json["id"]);
    assert_eq!(chat_column(&env, id, "title").await, "NULL");
}

#[tokio::test]
async fn create_treats_null_title_and_null_model_like_absent_ones() {
    let env = ChatEnv::new().await;

    let reply = env
        .post_chat(&json!({ "title": null, "model": null }))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert!(reply.json.get("title").is_none(), "{}", reply.text);
    assert_eq!(reply.json["model"], "model-a");
}

#[tokio::test]
async fn create_ignores_unknown_body_fields_and_always_creates_a_non_temporary_chat() {
    let env = ChatEnv::new().await;

    let reply = env
        .post_chat(&json!({
            "title": "t",
            "is_temporary": true,
            "user_id": "00000000-0000-0000-0000-000000000001",
            "tenant_id": "00000000-0000-0000-0000-000000000002",
            "id": "00000000-0000-0000-0000-000000000003",
            "message_count": 99,
            "whatever": { "nested": 1 }
        }))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["is_temporary"], false);
    assert_eq!(reply.json["message_count"], 0);
    let id = uuid_of(&reply.json["id"]);
    assert_ne!(id, Uuid::from_u128(3), "the server generates the id");
    assert_eq!(chat_column(&env, id, "is_temporary").await, "0");
    assert_eq!(chat_column(&env, id, "tenant_id").await, qblob(USER_TENANT));
    assert_eq!(chat_column(&env, id, "user_id").await, qblob(USER));
}

#[tokio::test]
async fn create_accepts_a_255_character_title_and_a_one_character_title() {
    let env = ChatEnv::new().await;
    let long = "x".repeat(255);

    let long_reply = env.post_chat(&json!({ "title": long })).await;
    let short_reply = env.post_chat(&json!({ "title": " y " })).await;

    assert_eq!(
        long_reply.status,
        StatusCode::CREATED,
        "{}",
        long_reply.text
    );
    assert_eq!(long_reply.json["title"], long.as_str());
    assert_eq!(
        short_reply.status,
        StatusCode::CREATED,
        "{}",
        short_reply.text
    );
    assert_eq!(short_reply.json["title"], "y");
}

#[tokio::test]
async fn create_counts_title_characters_not_bytes() {
    let env = ChatEnv::new().await;

    let ok = env
        .post_chat(&json!({ "title": "\u{e9}".repeat(255) }))
        .await;
    let too_long = env
        .post_chat(&json!({ "title": "\u{e9}".repeat(256) }))
        .await;

    assert_eq!(ok.status, StatusCode::CREATED, "{}", ok.text);
    too_long.assert_invalid("title", "INVALID_TITLE");
}

#[tokio::test]
async fn create_rejects_invalid_titles_before_the_pdp_and_the_catalog() {
    for title in [
        String::new(),
        "   ".to_owned(),
        "\u{3000}\t".to_owned(),
        "x".repeat(256),
        format!("  {}  ", "x".repeat(256)),
    ] {
        // A denying PDP proves the title check comes first: 400, not 403.
        let env = ChatEnv::with(Options::with_pdp(PdpReply::Deny)).await;

        let reply = env.post_chat(&json!({ "title": title })).await;

        reply.assert_invalid("title", "INVALID_TITLE");
        assert!(env.pdp.requests().is_empty(), "no PDP call for {title:?}");
        assert!(
            env.plugin.snapshot_requests.lock().unwrap().is_empty(),
            "no catalog read for {title:?}"
        );
        assert_eq!(chat_count(&env).await, 0);
    }
}

#[tokio::test]
async fn create_rejects_an_unknown_disabled_or_misspelled_model() {
    for model in [
        "no-such-model",
        "model-off",
        "",
        "Model-A",
        "model-a ",
        " model-a",
    ] {
        let env = ChatEnv::new().await;

        let reply = env.post_chat(&json!({ "model": model })).await;

        reply.assert_invalid("model", "INVALID_MODEL");
        assert_eq!(chat_count(&env).await, 0, "{model:?}");
    }
}

#[tokio::test]
async fn create_picks_the_first_enabled_default_entry_in_catalog_order() {
    let env = ChatEnv::with(Options {
        catalog: vec![
            catalog_entry("a-off-default", false, Some(true)),
            catalog_entry("b-plain", true, None),
            catalog_entry("c-default", true, Some(true)),
            catalog_entry("d-default", true, Some(true)),
        ],
        ..Options::default()
    })
    .await;

    let reply = env.post_chat(&json!({})).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["model"], "c-default");
}

#[tokio::test]
async fn create_falls_back_to_the_first_enabled_entry_without_an_enabled_default() {
    let env = ChatEnv::with(Options {
        catalog: vec![
            catalog_entry("a-off", false, Some(true)),
            catalog_entry("b-first", true, Some(false)),
            catalog_entry("c-second", true, None),
        ],
        ..Options::default()
    })
    .await;

    let reply = env.post_chat(&json!({ "title": "t" })).await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    assert_eq!(reply.json["model"], "b-first");
}

#[tokio::test]
async fn create_without_any_enabled_model_is_invalid_model() {
    for catalog in [Vec::new(), vec![catalog_entry("a", false, Some(true))]] {
        let env = ChatEnv::with(Options {
            catalog,
            ..Options::default()
        })
        .await;

        let reply = env.post_chat(&json!({})).await;

        reply.assert_invalid("model", "INVALID_MODEL");
        assert_eq!(chat_count(&env).await, 0);
    }
}

#[tokio::test]
async fn create_reads_the_current_policy_snapshot_on_every_call_without_a_cache() {
    let env = ChatEnv::new().await;

    create(&env, &json!({})).await;
    create(&env, &json!({ "model": "model-b" })).await;

    let requests = env.plugin.snapshot_requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec![(USER, POLICY_VERSION), (USER, POLICY_VERSION)]
    );
}

#[tokio::test]
async fn create_with_a_failing_policy_plugin_is_500_internal_and_creates_nothing() {
    for failing in ["version", "snapshot"] {
        let env = ChatEnv::new().await;
        let behavior = Behavior::Fail(PolicyPluginError::Internal(
            "database password=hunter2".to_owned(),
        ));
        if failing == "version" {
            env.plugin.set_version(behavior);
        } else {
            env.plugin.set_snapshot(behavior);
        }

        let reply = env.post_chat(&json!({ "title": "t" })).await;

        assert_eq!(
            reply.status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{}",
            reply.text
        );
        assert!(reply.category().contains("internal"), "{}", reply.text);
        assert!(!reply.text.contains("hunter2"), "{}", reply.text);
        assert_eq!(chat_count(&env).await, 0);
    }
}

#[tokio::test]
async fn create_with_a_policy_plugin_that_never_answers_is_500_internal() {
    let env = ChatEnv::new().await;
    env.plugin.set_snapshot(Behavior::Hang);

    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        env.post_chat(&json!({ "title": "t" })),
    )
    .await
    .expect("the plugin call is bounded");

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(chat_count(&env).await, 0);
}

#[tokio::test]
async fn create_with_an_unregistered_policy_plugin_is_500_internal() {
    let env = ChatEnv::with(Options {
        register_plugin: false,
        ..Options::default()
    })
    .await;

    let reply = env.post_chat(&json!({ "title": "t" })).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(chat_count(&env).await, 0);
}

#[tokio::test]
async fn create_asks_the_pdp_for_the_create_action_with_the_owner_properties() {
    let env = ChatEnv::new().await;

    create(&env, &json!({ "title": "t" })).await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "create");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, None);
    assert_eq!(
        request.resource.properties.get("owner_tenant_id"),
        Some(&json!(USER_TENANT.to_string()))
    );
    assert_eq!(
        request.resource.properties.get("owner_id"),
        Some(&json!(USER.to_string()))
    );
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn create_stores_the_chat_in_the_tenant_and_for_the_user_of_the_security_context() {
    let env = ChatEnv::new().await;

    let reply = env
        .send_json(
            "POST",
            "/mini-chat/v1/chats",
            &other_tenant_ctx(),
            &json!({ "title": "t" }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let id = uuid_of(&reply.json["id"]);
    assert_eq!(
        chat_column(&env, id, "tenant_id").await,
        qblob(OTHER_TENANT)
    );
    assert_eq!(chat_column(&env, id, "user_id").await, qblob(OTHER_USER));
    let own = env.list_chats("").await;
    assert_eq!(
        own.json["items"],
        json!([]),
        "the default user sees nothing"
    );
}

#[tokio::test]
async fn create_with_a_pdp_returning_two_tenants_still_creates_in_the_request_tenant() {
    let env = ChatEnv::with(Options::with_pdp(PdpReply::AllowTwoTenants)).await;

    let id = create(&env, &json!({ "title": "t" })).await;

    assert_eq!(chat_column(&env, id, "tenant_id").await, qblob(USER_TENANT));
    assert_eq!(chat_column(&env, id, "user_id").await, qblob(USER));
}

#[tokio::test]
async fn create_writes_nothing_but_the_chat_row() {
    let env = ChatEnv::running_outbox().await;

    create(&env, &json!({ "title": "t" })).await;

    for table in [
        "messages",
        "chat_turns",
        "attachments",
        "message_attachments",
        "message_reactions",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
    ] {
        assert_eq!(env.db.count(table).await, 0, "{table}");
    }
    assert_eq!(env.chat_cleanup_rows().await.len(), 0);
}

#[tokio::test]
async fn create_generates_a_distinct_id_for_every_chat() {
    let env = ChatEnv::new().await;

    let first = create(&env, &json!({ "title": "same" })).await;
    let second = create(&env, &json!({ "title": "same" })).await;

    assert_ne!(first, second);
    assert_eq!(chat_count(&env).await, 2);
}

#[tokio::test]
async fn the_location_header_follows_the_configured_url_prefix() {
    let env = ChatEnv::with(Options {
        prefix: "/chat-api",
        ..Options::default()
    })
    .await;

    let reply = env
        .send_json("POST", "/chat-api/v1/chats", &user_ctx(), &json!({}))
        .await;

    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let id = uuid_of(&reply.json["id"]);
    assert_eq!(
        reply.header("location"),
        Some(format!("/chat-api/v1/chats/{id}").as_str())
    );
    let elsewhere = env.post_chat(&json!({})).await;
    assert_eq!(elsewhere.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_rejects_a_body_that_does_not_match_the_schema_with_422() {
    for body in [
        json!({ "title": 5 }),
        json!({ "title": ["a"] }),
        json!({ "model": 7 }),
        json!({ "title": true }),
        json!("text"),
        json!(42),
    ] {
        let env = ChatEnv::new().await;

        let reply = env.post_chat(&body).await;

        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            reply.text
        );
        assert_eq!(
            reply.first_violation().1,
            "invalid_json_body",
            "{}",
            reply.text
        );
        assert_eq!(chat_count(&env).await, 0);
    }
}

#[tokio::test]
async fn create_rejects_malformed_json_with_400_and_a_wrong_content_type_with_415() {
    let env = ChatEnv::new().await;

    let malformed = env
        .send_raw(
            "POST",
            "/mini-chat/v1/chats",
            &user_ctx(),
            Some("application/json"),
            Some("{not json".to_owned()),
        )
        .await;
    let wrong_type = env
        .send_raw(
            "POST",
            "/mini-chat/v1/chats",
            &user_ctx(),
            Some("text/plain"),
            Some(r#"{"title":"t"}"#.to_owned()),
        )
        .await;
    let no_type = env
        .send_raw(
            "POST",
            "/mini-chat/v1/chats",
            &user_ctx(),
            None,
            Some(r#"{"title":"t"}"#.to_owned()),
        )
        .await;

    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "{}",
        malformed.text
    );
    assert_eq!(malformed.first_violation().1, "json_syntax_error");
    assert_eq!(
        wrong_type.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        wrong_type.text
    );
    assert_eq!(wrong_type.first_violation().1, "missing_json_content_type");
    assert_eq!(no_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(chat_count(&env).await, 0);
    assert!(env.pdp.requests().is_empty());
}

// ================================================================================================
// Slice 2: model immutability (acceptance: "a chat's model is immutable once set")
// ================================================================================================

#[tokio::test]
async fn the_model_of_a_chat_never_changes_after_create() {
    let env = ChatEnv::new().await;
    let id = create(&env, &json!({ "title": "t", "model": "model-a" })).await;

    let renamed = env
        .patch_chat(id, &json!({ "title": "x", "model": "model-b" }))
        .await;
    // Disabling and even removing the model from the catalog changes nothing.
    env.plugin.catalog.lock().unwrap().clear();
    let got = env.get(&chat_uri(id)).await;
    let listed = env.list_chats("").await;

    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    assert_eq!(renamed.json["model"], "model-a");
    assert_eq!(got.json["model"], "model-a");
    assert_eq!(listed.json["items"][0]["model"], "model-a");
    assert_eq!(chat_column(&env, id, "model").await, "'model-a'");
}

#[tokio::test]
async fn no_route_accepts_a_model_change_or_a_temporary_toggle() {
    let env = ChatEnv::new().await;
    let id = create(&env, &json!({ "title": "t" })).await;

    let put = env
        .send_json(
            "PUT",
            &chat_uri(id),
            &user_ctx(),
            &json!({ "model": "model-b" }),
        )
        .await;
    let post = env
        .send_json(
            "POST",
            &chat_uri(id),
            &user_ctx(),
            &json!({ "model": "model-b" }),
        )
        .await;
    let temporary = env
        .send_json(
            "POST",
            &format!("/mini-chat/v1/chats/{id}:temporary"),
            &user_ctx(),
            &json!({}),
        )
        .await;

    assert_eq!(put.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(
        temporary.status == StatusCode::NOT_FOUND
            || temporary.status == StatusCode::METHOD_NOT_ALLOWED,
        "{}",
        temporary.status
    );
    assert_eq!(chat_column(&env, id, "model").await, "'model-a'");
    assert_eq!(chat_column(&env, id, "is_temporary").await, "0");
}

// ================================================================================================
// Slice 2: get
// ================================================================================================

#[tokio::test]
async fn get_returns_the_chat_detail_with_the_message_count_and_no_messages() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Plan"), 30).await;
    env.seed_message(id, 1, "user", 1).await;
    env.seed_message(id, 2, "assistant", 2).await;

    let reply = env.get(&chat_uri(id)).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json), key_set(&CHAT_KEYS), "{}", reply.text);
    assert_eq!(reply.json["id"], id.to_string());
    assert_eq!(reply.json["title"], "Plan");
    assert_eq!(reply.json["model"], "gpt-5.2");
    assert_eq!(reply.json["is_temporary"], false);
    assert_eq!(reply.json["message_count"], 2);
    assert_eq!(
        parse_ts(reply.json["created_at"].as_str().unwrap()),
        parse_ts(&ts(0))
    );
    assert_eq!(
        parse_ts(reply.json["updated_at"].as_str().unwrap()),
        parse_ts(&ts(30))
    );
    assert!(reply.json.get("messages").is_none());
    assert_no_leak(&reply.text);
}

#[tokio::test]
async fn message_count_counts_every_role_and_skips_soft_deleted_messages() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Plan"), 30).await;
    let other = env.seed_chat(2, Some("Other"), 30).await;
    env.seed_message(id, 1, "user", 1).await;
    env.seed_message(id, 2, "assistant", 2).await;
    env.seed_message(id, 3, "system", 3).await;
    let deleted = env.seed_message(id, 4, "assistant", 4).await;
    env.soft_delete_message(deleted).await;
    env.seed_message(other, 5, "user", 5).await;

    let reply = env.get(&chat_uri(id)).await;

    assert_eq!(reply.json["message_count"], 3, "{}", reply.text);
}

#[tokio::test]
async fn a_chat_without_messages_has_message_count_zero() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Empty"), 30).await;

    let reply = env.get(&chat_uri(id)).await;

    assert_eq!(reply.json["message_count"], 0);
}

#[tokio::test]
async fn get_omits_the_title_key_of_an_untitled_chat() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, None, 30).await;

    let reply = env.get(&chat_uri(id)).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(reply.json.get("title").is_none(), "{}", reply.text);
    assert_eq!(keys(&reply.json).len(), 6);
}

#[tokio::test]
async fn get_does_not_change_the_chat() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Plan"), 30).await;
    let before = env.chat_snapshot(id).await;

    let reply = env.get(&chat_uri(id)).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(env.chat_snapshot(id).await, before);
}

#[tokio::test]
async fn get_of_an_unknown_deleted_or_foreign_chat_is_404_with_the_chat_resource_type() {
    let env = ChatEnv::new().await;
    let deleted = env.seed_chat(1, Some("gone"), 30).await;
    env.db.soft_delete("chats", deleted).await;
    let foreign_user = env
        .seed_chat_of(2, USER_TENANT, OTHER_USER, Some("not yours"), 30)
        .await;
    let foreign_tenant = env
        .seed_chat_of(3, OTHER_TENANT, USER, Some("other tenant"), 30)
        .await;

    let replies = [
        env.get(&chat_uri(chat_id(99))).await,
        env.get(&chat_uri(deleted)).await,
        env.get(&chat_uri(foreign_user)).await,
        env.get(&chat_uri(foreign_tenant)).await,
    ];

    for reply in &replies {
        reply.assert_not_found(CHAT_TYPE);
        assert!(reply.json.get("code").is_none(), "{}", reply.text);
    }
    // Indistinguishable from each other.
    for reply in &replies[1..] {
        assert_eq!(reply.json["context"], replies[0].json["context"]);
        assert_eq!(reply.json["detail"], replies[0].json["detail"]);
        assert_eq!(reply.json["title"], replies[0].json["title"]);
    }
    assert!(!replies[2].text.contains("not yours"));
}

#[tokio::test]
async fn get_with_an_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = ChatEnv::new().await;

    let reply = env.get("/mini-chat/v1/chats/not-a-uuid").await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(
        reply.first_violation(),
        ("path".to_owned(), "invalid_path_params".to_owned())
    );
    assert!(env.pdp.requests().is_empty());
}

#[tokio::test]
async fn get_asks_the_pdp_for_read_on_the_chat() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Plan"), 30).await;

    env.get(&chat_uri(id)).await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, Some(id));
    assert!(request.context.require_constraints);
}

// ================================================================================================
// Slice 2: rename
// ================================================================================================

#[tokio::test]
async fn rename_returns_200_with_the_new_trimmed_title_and_a_later_updated_at() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;
    env.seed_message(id, 1, "user", 1).await;
    env.seed_message(id, 2, "assistant", 2).await;
    let stable_before = chat_stable_columns(&env, id).await;

    let reply = env
        .patch_chat(id, &json!({ "title": "  New name  " }))
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json), key_set(&CHAT_KEYS), "{}", reply.text);
    assert_eq!(reply.json["title"], "New name");
    assert_eq!(reply.json["id"], id.to_string());
    assert_eq!(reply.json["model"], "gpt-5.2");
    assert_eq!(reply.json["is_temporary"], false);
    assert_eq!(reply.json["message_count"], 2);
    let created = parse_ts(reply.json["created_at"].as_str().unwrap());
    let updated = parse_ts(reply.json["updated_at"].as_str().unwrap());
    assert_eq!(created, parse_ts(&ts(0)));
    assert!(updated > parse_ts(&ts(30)), "{updated}");
    assert!(updated >= chrono::Utc::now() - chrono::Duration::seconds(30));
    assert_eq!(chat_column(&env, id, "title").await, "'New name'");
    assert_eq!(chat_stable_columns(&env, id).await, stable_before);
    assert_eq!(
        parse_ts(chat_column(&env, id, "updated_at").await.as_str()),
        updated
    );
}

#[tokio::test]
async fn rename_gives_an_untitled_chat_a_title() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, None, 30).await;

    let reply = env.patch_chat(id, &json!({ "title": "Now titled" })).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["title"], "Now titled");
}

#[tokio::test]
async fn rename_ignores_unknown_fields_and_leaves_model_and_flags_unchanged() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;
    let stable_before = chat_stable_columns(&env, id).await;

    let reply = env
        .patch_chat(
            id,
            &json!({
                "title": "Renamed",
                "model": "model-b",
                "is_temporary": true,
                "id": "00000000-0000-0000-0000-000000000009",
                "user_id": "00000000-0000-0000-0000-000000000001",
                "tenant_id": "00000000-0000-0000-0000-000000000002",
                "message_count": 42,
                "created_at": "2000-01-01T00:00:00Z",
                "updated_at": "2000-01-01T00:00:00Z",
                "deleted_at": "2000-01-01T00:00:00Z"
            }),
        )
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["title"], "Renamed");
    assert_eq!(reply.json["model"], "gpt-5.2");
    assert_eq!(reply.json["is_temporary"], false);
    assert_eq!(reply.json["id"], id.to_string());
    assert_eq!(reply.json["message_count"], 0);
    assert_eq!(chat_stable_columns(&env, id).await, stable_before);
}

#[tokio::test]
async fn rename_writes_only_the_chat_row_never_messages_or_attachments() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;
    let message = env.seed_message(id, 1, "assistant", 1).await;
    let attachment = env.seed_attachment(id, 1, "image", "ready", None).await;
    env.link_attachment(id, message, attachment, 1).await;
    env.seed_reaction(1, message, USER, "like").await;
    let mut before = Vec::new();
    for table in [
        "messages",
        "attachments",
        "message_attachments",
        "message_reactions",
    ] {
        before.push(env.dump(table).await);
    }

    let reply = env.patch_chat(id, &json!({ "title": "Renamed" })).await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let mut after = Vec::new();
    for table in [
        "messages",
        "attachments",
        "message_attachments",
        "message_reactions",
    ] {
        after.push(env.dump(table).await);
    }
    assert_eq!(before, after);
}

#[tokio::test]
async fn rename_accepts_255_characters_and_rejects_256() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;

    let ok = env
        .patch_chat(id, &json!({ "title": "t".repeat(255) }))
        .await;
    let bad = env
        .patch_chat(id, &json!({ "title": "t".repeat(256) }))
        .await;

    assert_eq!(ok.status, StatusCode::OK, "{}", ok.text);
    bad.assert_invalid("title", "INVALID_TITLE");
    assert_eq!(
        chat_column(&env, id, "title").await,
        format!("'{}'", "t".repeat(255))
    );
}

#[tokio::test]
async fn rename_rejects_invalid_titles_before_the_pdp_and_the_chat_lookup() {
    let too_long = "t".repeat(256);
    for title in ["", "   ", "\n\t", too_long.as_str()] {
        let env = ChatEnv::with(Options::with_pdp(PdpReply::Deny)).await;
        let id = env.seed_chat(1, Some("Old"), 30).await;
        let before = env.chat_snapshot(id).await;

        let on_existing = env.patch_chat(id, &json!({ "title": title })).await;
        // An invalid title on an unknown chat is 400, not 404.
        let on_unknown = env
            .patch_chat(chat_id(99), &json!({ "title": title }))
            .await;

        on_existing.assert_invalid("title", "INVALID_TITLE");
        on_unknown.assert_invalid("title", "INVALID_TITLE");
        assert!(env.pdp.requests().is_empty());
        assert_eq!(env.chat_snapshot(id).await, before);
    }
}

#[tokio::test]
async fn rename_rejects_a_body_without_a_valid_title_with_422() {
    for body in [
        json!({}),
        json!({ "title": null }),
        json!({ "title": 5 }),
        json!({ "title": false }),
        json!({ "model": "model-b" }),
        json!([]),
    ] {
        let env = ChatEnv::new().await;
        let id = env.seed_chat(1, Some("Old"), 30).await;
        let before = env.chat_snapshot(id).await;

        let reply = env.patch_chat(id, &body).await;

        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            reply.text
        );
        assert_eq!(reply.first_violation().1, "invalid_json_body");
        assert_eq!(env.chat_snapshot(id).await, before);
    }
}

#[tokio::test]
async fn rename_rejects_malformed_json_with_400_and_a_wrong_content_type_with_415() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;

    let malformed = env
        .send_raw(
            "PATCH",
            &chat_uri(id),
            &user_ctx(),
            Some("application/json"),
            Some("{\"title\":".to_owned()),
        )
        .await;
    let wrong = env
        .send_raw(
            "PATCH",
            &chat_uri(id),
            &user_ctx(),
            Some("text/plain"),
            Some(r#"{"title":"x"}"#.to_owned()),
        )
        .await;

    assert_eq!(
        malformed.status,
        StatusCode::BAD_REQUEST,
        "{}",
        malformed.text
    );
    assert_eq!(
        wrong.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        wrong.text
    );
    assert_eq!(chat_column(&env, id, "title").await, "'Old'");
}

#[tokio::test]
async fn rename_of_an_unknown_deleted_or_foreign_chat_is_404_and_changes_nothing() {
    let env = ChatEnv::new().await;
    let deleted = env.seed_chat(1, Some("gone"), 30).await;
    env.db.soft_delete("chats", deleted).await;
    let foreign_user = env
        .seed_chat_of(2, USER_TENANT, OTHER_USER, Some("theirs"), 30)
        .await;
    let foreign_tenant = env
        .seed_chat_of(3, OTHER_TENANT, USER, Some("other tenant"), 30)
        .await;
    let snapshots = [
        env.chat_snapshot(deleted).await,
        env.chat_snapshot(foreign_user).await,
        env.chat_snapshot(foreign_tenant).await,
    ];

    for id in [chat_id(99), deleted, foreign_user, foreign_tenant] {
        let reply = env.patch_chat(id, &json!({ "title": "hijacked" })).await;

        reply.assert_not_found(CHAT_TYPE);
    }

    assert_eq!(env.chat_snapshot(deleted).await, snapshots[0]);
    assert_eq!(env.chat_snapshot(foreign_user).await, snapshots[1]);
    assert_eq!(env.chat_snapshot(foreign_tenant).await, snapshots[2]);
}

#[tokio::test]
async fn rename_asks_the_pdp_for_update_on_the_chat() {
    let env = ChatEnv::new().await;
    let id = env.seed_chat(1, Some("Old"), 30).await;

    env.patch_chat(id, &json!({ "title": "New" })).await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "update");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, Some(id));
    assert!(request.context.require_constraints);
}

// ================================================================================================
// Slice 3: list, pagination, OData
// ================================================================================================

#[tokio::test]
async fn list_returns_items_and_page_info_in_the_documented_shape() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("one"), 10).await;
    env.seed_chat(2, None, 20).await;

    let reply = env.list_chats("").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json), key_set(&["items", "page_info"]));
    assert_eq!(
        keys(&reply.json["page_info"]),
        key_set(&["limit", "next_cursor", "prev_cursor"])
    );
    assert_eq!(reply.json["page_info"]["limit"], 20);
    assert_eq!(reply.json["page_info"]["next_cursor"], Value::Null);
    assert_eq!(reply.json["page_info"]["prev_cursor"], Value::Null);
    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(keys(&items[1]), key_set(&CHAT_KEYS));
    assert_eq!(
        keys(&items[0]).len(),
        6,
        "the untitled chat has no title key: {}",
        reply.text
    );
    assert_no_leak(&reply.text);
}

#[tokio::test]
async fn an_empty_list_is_200_with_no_items_and_a_null_next_cursor() {
    let env = ChatEnv::new().await;

    let reply = env.list_chats("").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(
        reply.json,
        json!({
            "items": [],
            "page_info": { "limit": 20, "next_cursor": null, "prev_cursor": null }
        })
    );
}

#[tokio::test]
async fn the_default_order_is_updated_at_desc_then_id_desc() {
    let env = ChatEnv::new().await;
    let oldest = env.seed_chat(1, Some("a"), 10).await;
    let newest = env.seed_chat(2, Some("b"), 30).await;
    let middle = env.seed_chat(3, Some("c"), 20).await;

    let reply = env.list_chats("").await;

    assert_eq!(
        item_ids(&reply),
        vec![newest.to_string(), middle.to_string(), oldest.to_string()]
    );
}

#[tokio::test]
async fn chats_with_equal_updated_at_are_ordered_by_id_desc_and_stay_stable_across_pages() {
    let env = ChatEnv::new().await;
    for n in 1..=5 {
        env.seed_chat(n, Some("same"), 50).await;
    }
    let expected: Vec<String> = (1..=5).rev().map(|n| chat_id(n).to_string()).collect();

    let single = env.list_chats("").await;
    let mut walked = Vec::new();
    let mut query = "?limit=2".to_owned();
    loop {
        let page = env.list_chats(&query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text);
        walked.extend(item_ids(&page));
        match next_cursor(&page) {
            Some(cursor) => query = format!("?limit=2&cursor={}", enc(&cursor)),
            None => break,
        }
    }

    assert_eq!(item_ids(&single), expected);
    assert_eq!(walked, expected);
}

#[tokio::test]
async fn walking_next_cursor_returns_every_chat_exactly_once() {
    let env = ChatEnv::new().await;
    for n in 1..=7_u128 {
        env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap() * 10)
            .await;
    }
    let expected: Vec<String> = (1..=7).rev().map(|n| chat_id(n).to_string()).collect();

    let mut walked = Vec::new();
    let mut sizes = Vec::new();
    let mut query = "?limit=3".to_owned();
    loop {
        let page = env.list_chats(&query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text);
        assert_eq!(page.json["page_info"]["limit"], 3);
        sizes.push(page.json["items"].as_array().unwrap().len());
        walked.extend(item_ids(&page));
        match next_cursor(&page) {
            Some(cursor) => query = format!("?limit=3&cursor={}", enc(&cursor)),
            None => break,
        }
    }

    assert_eq!(sizes, vec![3, 3, 1]);
    assert_eq!(walked, expected);
    let unique: BTreeSet<&String> = walked.iter().collect();
    assert_eq!(unique.len(), 7, "no duplicates");
}

#[tokio::test]
async fn the_prev_cursor_of_a_later_page_leads_back_to_the_previous_page() {
    let env = ChatEnv::new().await;
    for n in 1..=6_u128 {
        env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap() * 10)
            .await;
    }
    let first = env.list_chats("?limit=2").await;
    let cursor = next_cursor(&first).expect("a second page exists");
    let second = env
        .list_chats(&format!("?limit=2&cursor={}", enc(&cursor)))
        .await;
    let prev = second.json["page_info"]["prev_cursor"]
        .as_str()
        .expect("the second page has a prev_cursor")
        .to_owned();

    let back = env
        .list_chats(&format!("?limit=2&cursor={}", enc(&prev)))
        .await;

    assert_eq!(back.status, StatusCode::OK, "{}", back.text);
    assert_eq!(item_ids(&back), item_ids(&first));
}

#[tokio::test]
async fn the_default_limit_is_20() {
    let env = ChatEnv::new().await;
    for n in 1..=25_u128 {
        env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap())
            .await;
    }

    let first = env.list_chats("").await;
    let cursor = next_cursor(&first).expect("more chats exist");
    let second = env.list_chats(&format!("?cursor={}", enc(&cursor))).await;

    assert_eq!(first.json["items"].as_array().unwrap().len(), 20);
    assert_eq!(first.json["page_info"]["limit"], 20);
    assert_eq!(second.json["items"].as_array().unwrap().len(), 5);
    assert_eq!(next_cursor(&second), None);
}

#[tokio::test]
async fn a_limit_above_100_is_clamped_to_100_and_not_rejected() {
    let env = ChatEnv::new().await;
    for n in 1..=101_u128 {
        env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap())
            .await;
    }

    let reply = env.list_chats("?limit=500").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.json["page_info"]["limit"], 100);
    assert_eq!(reply.json["items"].as_array().unwrap().len(), 100);
    assert!(next_cursor(&reply).is_some());
}

#[tokio::test]
async fn a_limit_of_exactly_100_is_accepted_and_a_small_limit_is_honoured() {
    let env = ChatEnv::new().await;
    for n in 1..=3_u128 {
        env.seed_chat(n, Some("chat"), i64::try_from(n).unwrap())
            .await;
    }

    let hundred = env.list_chats("?limit=100").await;
    let one = env.list_chats("?limit=1").await;

    assert_eq!(hundred.json["page_info"]["limit"], 100);
    assert_eq!(one.json["page_info"]["limit"], 1);
    assert_eq!(one.json["items"].as_array().unwrap().len(), 1);
    assert!(next_cursor(&one).is_some());
}

#[tokio::test]
async fn limit_zero_is_400_invalid_limit() {
    let env = ChatEnv::new().await;

    let reply = env.list_chats("?limit=0").await;

    reply.assert_odata_error("INVALID_LIMIT");
}

#[tokio::test]
async fn list_filters_over_title_updated_at_and_id() {
    let env = ChatEnv::new().await;
    let q3 = env.seed_chat(1, Some("Q3 report"), 10).await;
    let q4 = env.seed_chat(2, Some("Q4 report"), 20).await;
    let notes = env.seed_chat(3, Some("Notes"), 30).await;
    let untitled = env.seed_chat(4, None, 40).await;
    let ids = |reply: &crate::chat_test_env::Reply| -> BTreeSet<String> {
        item_ids(reply).into_iter().collect()
    };
    let set =
        |chats: &[Uuid]| -> BTreeSet<String> { chats.iter().map(ToString::to_string).collect() };

    let contains = env
        .list_chats(&format!("?$filter={}", enc("contains(title,'report')")))
        .await;
    let eq = env
        .list_chats(&format!("?$filter={}", enc("title eq 'Notes'")))
        .await;
    let starts = env
        .list_chats(&format!("?$filter={}", enc("startswith(title,'Q')")))
        .await;
    let by_id = env
        .list_chats(&format!("?$filter={}", enc(&format!("id eq {untitled}"))))
        .await;
    let newer = env
        .list_chats(&format!(
            "?$filter={}",
            enc(&format!("updated_at gt {}", rfc3339(25)))
        ))
        .await;
    let combined = env
        .list_chats(&format!(
            "?$filter={}",
            enc(&format!(
                "contains(title,'report') and updated_at lt {}",
                rfc3339(15)
            ))
        ))
        .await;

    for reply in [&contains, &eq, &starts, &by_id, &newer, &combined] {
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    }
    assert_eq!(ids(&contains), set(&[q3, q4]));
    assert_eq!(ids(&eq), set(&[notes]));
    assert_eq!(ids(&starts), set(&[q3, q4]));
    assert_eq!(ids(&by_id), set(&[untitled]));
    assert_eq!(ids(&newer), set(&[notes, untitled]));
    assert_eq!(ids(&combined), set(&[q3]));
}

#[tokio::test]
async fn list_orders_by_the_allowed_fields_with_the_id_tiebreaker_in_the_direction_of_the_first_key()
 {
    let env = ChatEnv::new().await;
    let a1 = env.seed_chat(1, Some("alpha"), 30).await;
    let a2 = env.seed_chat(2, Some("alpha"), 20).await;
    let b3 = env.seed_chat(3, Some("beta"), 10).await;
    let s = |id: Uuid| id.to_string();

    let title_asc = env
        .list_chats(&format!("?$orderby={}", enc("title asc")))
        .await;
    let title_desc = env
        .list_chats(&format!("?$orderby={}", enc("title desc")))
        .await;
    let updated_asc = env
        .list_chats(&format!("?$orderby={}", enc("updated_at asc")))
        .await;
    let id_desc = env
        .list_chats(&format!("?$orderby={}", enc("id desc")))
        .await;
    let id_asc = env
        .list_chats(&format!("?$orderby={}", enc("id asc")))
        .await;
    let two_keys = env
        .list_chats(&format!("?$orderby={}", enc("title asc,updated_at desc")))
        .await;

    // Equal titles: id follows the direction of the first key.
    assert_eq!(item_ids(&title_asc), vec![s(a1), s(a2), s(b3)]);
    assert_eq!(item_ids(&title_desc), vec![s(b3), s(a2), s(a1)]);
    assert_eq!(item_ids(&updated_asc), vec![s(b3), s(a2), s(a1)]);
    assert_eq!(item_ids(&id_desc), vec![s(b3), s(a2), s(a1)]);
    assert_eq!(item_ids(&id_asc), vec![s(a1), s(a2), s(b3)]);
    assert_eq!(item_ids(&two_keys), vec![s(a1), s(a2), s(b3)]);
}

#[tokio::test]
async fn a_cursor_walk_over_a_custom_order_and_filter_has_no_gaps_or_duplicates() {
    let env = ChatEnv::new().await;
    for n in 1..=6_u128 {
        let title = if n % 2 == 0 { "even report" } else { "odd" };
        env.seed_chat(n, Some(title), i64::try_from(n).unwrap())
            .await;
    }
    let filter = enc("contains(title,'report')");
    let orderby = enc("title asc");

    let mut walked = Vec::new();
    let mut query = format!("?limit=2&$filter={filter}&$orderby={orderby}");
    loop {
        let page = env.list_chats(&query).await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.text);
        walked.extend(item_ids(&page));
        match next_cursor(&page) {
            Some(cursor) => query = format!("?limit=2&$filter={filter}&cursor={}", enc(&cursor)),
            None => break,
        }
    }

    // All three titles are equal, so the id ascending tiebreaker decides.
    assert_eq!(
        walked,
        vec![
            chat_id(2).to_string(),
            chat_id(4).to_string(),
            chat_id(6).to_string()
        ]
    );
}

#[tokio::test]
async fn renaming_an_older_chat_moves_it_to_the_first_position() {
    let env = ChatEnv::new().await;
    let oldest = env.seed_chat(1, Some("oldest"), 10).await;
    env.seed_chat(2, Some("middle"), 20).await;
    env.seed_chat(3, Some("newest"), 30).await;
    let before = env.list_chats("").await;

    let renamed = env.patch_chat(oldest, &json!({ "title": "bumped" })).await;
    let after = env.list_chats("").await;

    assert_eq!(renamed.status, StatusCode::OK, "{}", renamed.text);
    assert_eq!(item_ids(&before).last().unwrap(), &oldest.to_string());
    assert_eq!(item_ids(&after)[0], oldest.to_string());
}

#[tokio::test]
async fn a_chat_created_now_is_listed_before_older_chats() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("seeded"), 30).await;

    let created = create(&env, &json!({ "title": "fresh" })).await;

    let reply = env.list_chats("").await;
    assert_eq!(item_ids(&reply)[0], created.to_string());
}

#[tokio::test]
async fn list_items_carry_their_own_message_count_without_deleted_messages() {
    let env = ChatEnv::new().await;
    let busy = env.seed_chat(1, Some("busy"), 20).await;
    let quiet = env.seed_chat(2, Some("quiet"), 10).await;
    env.seed_message(busy, 1, "user", 1).await;
    env.seed_message(busy, 2, "assistant", 2).await;
    let gone = env.seed_message(busy, 3, "user", 3).await;
    env.soft_delete_message(gone).await;

    let reply = env.list_chats("").await;

    let items = reply.json["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], busy.to_string());
    assert_eq!(items[0]["message_count"], 2);
    assert_eq!(items[1]["id"], quiet.to_string());
    assert_eq!(items[1]["message_count"], 0);
}

#[tokio::test]
async fn list_returns_only_the_chats_of_the_caller_and_never_deleted_ones() {
    let env = ChatEnv::new().await;
    let mine = env.seed_chat(1, Some("mine"), 10).await;
    let deleted = env.seed_chat(2, Some("deleted"), 20).await;
    env.db.soft_delete("chats", deleted).await;
    let theirs = env
        .seed_chat_of(3, USER_TENANT, OTHER_USER, Some("theirs"), 30)
        .await;
    let other_tenant = env
        .seed_chat_of(4, OTHER_TENANT, OTHER_USER, Some("other tenant"), 40)
        .await;
    let other_tenant_same_user = env
        .seed_chat_of(5, OTHER_TENANT, USER, Some("same user, other tenant"), 50)
        .await;

    let mine_reply = env.list_chats("").await;
    let theirs_reply = env.get_as(&other_user_ctx(), "/mini-chat/v1/chats").await;
    let tenant_reply = env.get_as(&other_tenant_ctx(), "/mini-chat/v1/chats").await;
    let filtered_for_foreign = env
        .list_chats(&format!("?$filter={}", enc(&format!("id eq {theirs}"))))
        .await;

    assert_eq!(item_ids(&mine_reply), vec![mine.to_string()]);
    assert_eq!(item_ids(&theirs_reply), vec![theirs.to_string()]);
    assert_eq!(item_ids(&tenant_reply), vec![other_tenant.to_string()]);
    let same_user_other_tenant = env.get(&chat_uri(other_tenant_same_user)).await;
    same_user_other_tenant.assert_not_found(CHAT_TYPE);
    assert_eq!(filtered_for_foreign.json["items"], json!([]));
}

#[tokio::test]
async fn list_with_a_pdp_returning_two_tenants_is_still_limited_to_the_request_tenant_and_user() {
    let env = ChatEnv::with(Options::with_pdp(PdpReply::AllowTwoTenants)).await;
    let mine = env.seed_chat(1, Some("mine"), 10).await;
    env.seed_chat_of(2, OTHER_TENANT, USER, Some("other tenant"), 20)
        .await;
    env.seed_chat_of(3, USER_TENANT, OTHER_USER, Some("other user"), 30)
        .await;

    let reply = env.list_chats("").await;

    assert_eq!(item_ids(&reply), vec![mine.to_string()]);
}

#[tokio::test]
async fn list_accepts_select_and_still_returns_the_full_item() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("t"), 10).await;

    let reply = env.list_chats("?$select=title").await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(keys(&reply.json["items"][0]), key_set(&CHAT_KEYS));
}

#[tokio::test]
async fn list_rejects_fields_outside_updated_at_id_and_title_in_filter_and_orderby() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("t"), 10).await;

    for field_filter in [
        "model eq 'gpt-5.2'",
        "message_count gt 1",
        "is_temporary eq false",
        "created_at gt 2020-01-01T00:00:00Z",
        "user_id eq 00000000-0000-0000-0000-000000000055",
        "tenant_id eq 00000000-0000-0000-0000-00000000007e",
        "deleted_at eq null",
    ] {
        let reply = env
            .list_chats(&format!("?$filter={}", enc(field_filter)))
            .await;

        reply.assert_odata_error("INVALID_FILTER");
    }
    for field in [
        "model",
        "message_count",
        "is_temporary",
        "created_at",
        "user_id",
        "tenant_id",
        "deleted_at",
    ] {
        let reply = env
            .list_chats(&format!("?$orderby={}", enc(&format!("{field} asc"))))
            .await;

        reply.assert_odata_error("INVALID_ORDERBY_FIELD");
    }
}

#[tokio::test]
async fn list_rejects_malformed_filter_and_orderby_expressions() {
    let env = ChatEnv::new().await;

    for filter in [
        "title eq",
        "((title eq 'a')",
        "title lol 'a'",
        "contains(title)",
    ] {
        let reply = env.list_chats(&format!("?$filter={}", enc(filter))).await;

        reply.assert_odata_error("INVALID_FILTER");
    }
    for orderby in ["title sideways", "title asc desc", "title asc extra"] {
        let reply = env.list_chats(&format!("?$orderby={}", enc(orderby))).await;

        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{orderby}: {}",
            reply.text
        );
        assert_eq!(reply.resource_type(), crate::chat_test_env::ODATA_TYPE);
    }
}

#[tokio::test]
async fn list_rejects_a_filter_that_is_too_long() {
    let env = ChatEnv::new().await;
    let filter = format!("title eq '{}'", "x".repeat(9000));

    let reply = env.list_chats(&format!("?$filter={}", enc(&filter))).await;

    reply.assert_odata_error("FILTER_TOO_LONG");
}

#[tokio::test]
async fn list_rejects_a_malformed_cursor() {
    let env = ChatEnv::new().await;
    env.seed_chat(1, Some("t"), 10).await;

    for cursor in ["not-a-cursor", "!!!", "e30", "bm90IGpzb24"] {
        let reply = env.list_chats(&format!("?cursor={}", enc(cursor))).await;

        reply.assert_odata_error("INVALID_CURSOR");
    }
}

#[tokio::test]
async fn list_rejects_a_cursor_combined_with_orderby() {
    let env = ChatEnv::new().await;
    for n in 1..=3_u128 {
        env.seed_chat(n, Some("t"), i64::try_from(n).unwrap()).await;
    }
    let first = env.list_chats("?limit=1").await;
    let cursor = next_cursor(&first).unwrap();

    let reply = env
        .list_chats(&format!(
            "?limit=1&cursor={}&$orderby={}",
            enc(&cursor),
            enc("title asc")
        ))
        .await;

    reply.assert_odata_error("ORDER_WITH_CURSOR");
}

#[tokio::test]
async fn list_rejects_a_cursor_issued_for_another_filter() {
    let env = ChatEnv::new().await;
    for n in 1..=4_u128 {
        env.seed_chat(n, Some("report"), i64::try_from(n).unwrap())
            .await;
    }
    let filtered = env
        .list_chats(&format!(
            "?limit=1&$filter={}",
            enc("contains(title,'report')")
        ))
        .await;
    let filter_cursor = next_cursor(&filtered).unwrap();

    let other_filter = env
        .list_chats(&format!(
            "?limit=1&$filter={}&cursor={}",
            enc("contains(title,'rep')"),
            enc(&filter_cursor)
        ))
        .await;
    let no_filter = env
        .list_chats(&format!("?limit=1&cursor={}", enc(&filter_cursor)))
        .await;
    let same_filter = env
        .list_chats(&format!(
            "?limit=1&$filter={}&cursor={}",
            enc("contains(title,'report')"),
            enc(&filter_cursor)
        ))
        .await;

    other_filter.assert_odata_error("FILTER_MISMATCH");
    no_filter.assert_odata_error("FILTER_MISMATCH");
    assert_eq!(same_filter.status, StatusCode::OK, "{}", same_filter.text);
}

#[tokio::test]
async fn a_cursor_of_the_message_list_is_not_accepted_by_the_chat_list() {
    let env = ChatEnv::new().await;
    let chat = env.seed_chat(1, Some("chat"), 10).await;
    for n in 1..=3_u128 {
        env.seed_message(chat, n, "user", i64::try_from(n).unwrap())
            .await;
    }
    let messages = env.list_messages(chat, "?limit=1").await;
    let message_cursor = next_cursor(&messages).expect("a message cursor");

    let reply = env
        .list_chats(&format!("?cursor={}", enc(&message_cursor)))
        .await;

    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.resource_type(), crate::chat_test_env::ODATA_TYPE);
}

#[tokio::test]
async fn list_rejects_unsupported_dollar_options_and_a_malformed_select() {
    let env = ChatEnv::new().await;

    for query in [
        "?$skip=1",
        "?$count=true",
        "?$expand=messages",
        "?$search=a",
        "?$bogus=1",
    ] {
        let reply = env.list_chats(query).await;

        reply.assert_odata_error("UNSUPPORTED_QUERY_PARAM");
    }
    let select = env.list_chats("?$select=").await;
    select.assert_odata_error("INVALID_SELECT");
}

#[tokio::test]
async fn list_asks_the_pdp_for_list_without_a_resource_id() {
    let env = ChatEnv::new().await;

    env.list_chats("").await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "list");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, None);
    assert!(request.context.require_constraints);
}

// ================================================================================================
// Slice 4: delete
// ================================================================================================

/// A chat with messages, a turn, a reaction, a vector store and attachments in
/// every cleanup state, of the default user.
struct Populated {
    chat: Uuid,
    other_chat: Uuid,
    null_document: Uuid,
    null_image_failed: Uuid,
    null_soft_deleted: Uuid,
    pending: Uuid,
    done: Uuid,
    failed: Uuid,
    other_chat_attachment: Uuid,
}

async fn populate(env: &ChatEnv) -> Populated {
    let chat = env.seed_chat(1, Some("to delete"), 10).await;
    let other_chat = env.seed_chat(2, Some("other"), 20).await;
    let user_message = env.seed_message(chat, 1, "user", 1).await;
    let assistant = env.seed_message(chat, 2, "assistant", 2).await;
    env.seed_reaction(1, assistant, USER, "like").await;
    env.db
        .insert(
            "chat_turns",
            crate::infra::db::test_support::turn_row(
                Uuid::from_u128(0x7001),
                USER_TENANT,
                chat,
                crate::chat_test_env::request_id(1),
                "running",
            ),
        )
        .await
        .unwrap();
    env.db
        .insert(
            "chat_vector_stores",
            crate::infra::db::test_support::Row::default()
                .set("id", Uuid::from_u128(0x7101))
                .set("tenant_id", USER_TENANT)
                .set("chat_id", chat)
                .set("vector_store_id", "vs-1")
                .set("provider", "openai")
                .set("created_at", ts(0)),
        )
        .await
        .unwrap();
    let null_document = env
        .seed_attachment(chat, 1, "document", "ready", None)
        .await;
    let null_image_failed = env.seed_attachment(chat, 2, "image", "failed", None).await;
    let null_soft_deleted = env
        .seed_attachment(chat, 3, "document", "uploaded", None)
        .await;
    env.db.soft_delete("attachments", null_soft_deleted).await;
    let pending = env
        .seed_attachment(chat, 4, "document", "ready", Some("pending"))
        .await;
    let done = env
        .seed_attachment(chat, 5, "document", "ready", Some("done"))
        .await;
    let failed = env
        .seed_attachment(chat, 6, "image", "ready", Some("failed"))
        .await;
    for (n, attachment) in [null_document, null_image_failed, pending]
        .into_iter()
        .enumerate()
    {
        env.link_attachment(chat, user_message, attachment, i64::try_from(n).unwrap())
            .await;
    }
    // Pending, done and failed rows carry their own bookkeeping, which must stay.
    env.db
        .raw
        .execute_unprepared(&format!(
            "UPDATE attachments SET cleanup_attempts = 2, last_cleanup_error = 'boom', \
             cleanup_updated_at = '{}' WHERE id IN ({}, {}, {})",
            ts(7),
            blob(pending),
            blob(done),
            blob(failed)
        ))
        .await
        .unwrap();
    let other_chat_attachment = env
        .seed_attachment(other_chat, 7, "document", "ready", None)
        .await;
    Populated {
        chat,
        other_chat,
        null_document,
        null_image_failed,
        null_soft_deleted,
        pending,
        done,
        failed,
        other_chat_attachment,
    }
}

async fn attachment_state(env: &ChatEnv, id: Uuid) -> String {
    env.text(&format!(
        "SELECT quote(cleanup_status)||'|'||quote(cleanup_updated_at)||'|'||cleanup_attempts\
         ||'|'||quote(last_cleanup_error)||'|'||quote(deleted_at) \
         FROM attachments WHERE id = {}",
        blob(id)
    ))
    .await
}

#[tokio::test]
async fn delete_returns_204_with_an_empty_body_and_the_chat_becomes_unreachable() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;
    let message = msg_id(2);

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert!(reply.text.is_empty(), "no body: {}", reply.text);
    let after = [
        env.get(&chat_uri(p.chat)).await,
        env.patch_chat(p.chat, &json!({ "title": "x" })).await,
        env.delete(&chat_uri(p.chat)).await,
        env.list_messages(p.chat, "").await,
        env.put_reaction(p.chat, message, "like").await,
        env.delete_reaction(p.chat, message).await,
    ];
    for reply in &after {
        reply.assert_not_found(CHAT_TYPE);
    }
    let list = env.list_chats("").await;
    assert_eq!(item_ids(&list), vec![p.other_chat.to_string()]);
}

#[tokio::test]
async fn delete_sets_deleted_at_and_updated_at_to_the_same_deletion_time() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;
    let before = chrono::Utc::now();
    let stable_before = chat_stable_columns(&env, p.chat).await;

    let reply = env.delete(&chat_uri(p.chat)).await;

    let after = chrono::Utc::now();
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    let deleted_at = env
        .chat_deleted_at(p.chat)
        .await
        .expect("deleted_at is set");
    let deleted = parse_ts(&deleted_at);
    assert!(deleted >= before - chrono::Duration::seconds(2));
    assert!(deleted <= after + chrono::Duration::seconds(2));
    assert_eq!(parse_ts(&env.chat_updated_at(p.chat).await), deleted);
    // Nothing else of the chat row changed.
    let stable_after = chat_stable_columns(&env, p.chat).await;
    assert_eq!(
        stable_after.replace(&format!("'{deleted_at}'"), "NULL"),
        stable_before
    );
}

#[tokio::test]
async fn a_second_delete_of_the_same_chat_is_404_and_enqueues_nothing_more() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;
    assert_eq!(
        env.delete(&chat_uri(p.chat)).await.status,
        StatusCode::NO_CONTENT
    );
    let deleted_at = env.chat_deleted_at(p.chat).await;

    let second = env.delete(&chat_uri(p.chat)).await;

    second.assert_not_found(CHAT_TYPE);
    assert_eq!(env.chat_cleanup_rows().await.len(), 1);
    assert_eq!(env.chat_deleted_at(p.chat).await, deleted_at);
}

#[tokio::test]
async fn delete_marks_attachments_without_a_cleanup_status_as_pending() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;
    let untouched: Vec<(Uuid, String)> = {
        let mut rows = Vec::new();
        for id in [p.pending, p.done, p.failed, p.other_chat_attachment] {
            rows.push((id, attachment_state(&env, id).await));
        }
        rows
    };

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    let deleted_at = parse_ts(&env.chat_deleted_at(p.chat).await.unwrap());
    // NULL becomes pending for every kind and status, with the deletion time.
    for id in [p.null_document, p.null_image_failed, p.null_soft_deleted] {
        let status = env
            .text(&format!(
                "SELECT quote(cleanup_status) FROM attachments WHERE id = {}",
                blob(id)
            ))
            .await;
        assert_eq!(status, "'pending'", "{id}");
        let at = env
            .text(&format!(
                "SELECT cleanup_updated_at FROM attachments WHERE id = {}",
                blob(id)
            ))
            .await;
        assert_eq!(parse_ts(&at), deleted_at, "{id}");
    }
    // pending, done, failed and the attachments of other chats are untouched.
    for (id, state) in untouched {
        assert_eq!(attachment_state(&env, id).await, state, "{id}");
    }
    // `attachments.deleted_at` is never written by the chat delete.
    for id in [p.null_document, p.null_image_failed, p.pending] {
        let deleted = env
            .text(&format!(
                "SELECT quote(deleted_at) FROM attachments WHERE id = {}",
                blob(id)
            ))
            .await;
        assert_eq!(deleted, "NULL", "{id}");
    }
}

#[tokio::test]
async fn delete_does_not_touch_messages_turns_reactions_links_summaries_or_vector_stores() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;
    let tables = [
        "messages",
        "chat_turns",
        "message_reactions",
        "message_attachments",
        "thread_summaries",
        "chat_vector_stores",
        "quota_usage",
    ];
    let mut before = Vec::new();
    for table in tables {
        before.push(env.dump(table).await);
    }
    let other_chat_before = env.chat_snapshot(p.other_chat).await;

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    for (index, table) in tables.iter().enumerate() {
        assert_eq!(env.dump(table).await, before[index], "{table}");
    }
    assert_eq!(env.chat_snapshot(p.other_chat).await, other_chat_before);
    let running = env
        .db
        .query_i64("SELECT COUNT(*) FROM chat_turns WHERE state = 'running' AND deleted_at IS NULL")
        .await;
    assert_eq!(running, 1, "a running turn is not cancelled");
}

#[tokio::test]
async fn delete_enqueues_exactly_one_chat_cleanup_message_with_the_documented_payload() {
    let env = ChatEnv::running_outbox().await;
    let p = populate(&env).await;

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    let rows = env.chat_cleanup_rows().await;
    assert_eq!(
        rows.len(),
        1,
        "exactly one message on the chat-cleanup queue"
    );
    assert_eq!(rows[0].queue, CHAT_CLEANUP_QUEUE);
    let payload = rows[0].json();
    assert_eq!(
        keys(&payload),
        key_set(&[
            "tenant_id",
            "chat_id",
            "system_request_id",
            "reason",
            "chat_deleted_at"
        ]),
        "{payload}"
    );
    assert_eq!(payload["tenant_id"], USER_TENANT.to_string());
    assert_eq!(payload["chat_id"], p.chat.to_string());
    assert_eq!(payload["reason"], "chat_soft_delete");
    let system_request_id = uuid_of(&payload["system_request_id"]);
    assert_eq!(system_request_id.get_version_num(), 4);
    let deleted_at = parse_ts(&env.chat_deleted_at(p.chat).await.unwrap());
    let in_payload = parse_ts(payload["chat_deleted_at"].as_str().unwrap());
    assert_eq!(
        in_payload.timestamp_micros(),
        deleted_at.timestamp_micros(),
        "chat_deleted_at equals chats.deleted_at"
    );
    assert_eq!(
        rows[0].partition,
        i64::from(env.enqueuer.partition(
            crate::infra::outbox::queues::QueueKind::ChatCleanup,
            USER_TENANT,
            p.chat
        )),
        "partitioned by chat_id"
    );
    for other in [
        "mini-chat.usage_snapshot",
        "mini-chat.attachment_cleanup",
        "mini-chat.thread_summary",
        "mini-chat.audit",
    ] {
        assert!(env.outbox_rows(other).await.is_empty(), "{other}");
    }
}

#[tokio::test]
async fn every_delete_gets_its_own_system_request_id() {
    let env = ChatEnv::running_outbox().await;
    let first = env.seed_chat(1, Some("a"), 10).await;
    let second = env.seed_chat(2, Some("b"), 20).await;

    env.delete(&chat_uri(first)).await;
    env.delete(&chat_uri(second)).await;

    let rows = env.chat_cleanup_rows().await;
    assert_eq!(rows.len(), 2);
    let ids: BTreeSet<String> = rows
        .iter()
        .map(|r| r.json()["system_request_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn the_cleanup_message_goes_to_the_configured_chat_cleanup_queue() {
    let env = ChatEnv::with(Options {
        outbox: OutboxMode::Running,
        chat_cleanup_queue: Some("custom.chat_cleanup"),
        ..Options::default()
    })
    .await;
    let id = env.seed_chat(1, Some("a"), 10).await;

    let reply = env.delete(&chat_uri(id)).await;

    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text);
    assert_eq!(env.outbox_rows("custom.chat_cleanup").await.len(), 1);
    assert_eq!(env.outbox_rows(CHAT_CLEANUP_QUEUE).await.len(), 0);
}

#[tokio::test]
async fn an_outbox_failure_rolls_the_whole_delete_back_and_is_500_internal() {
    let logs = LogCapture::start();
    let env = ChatEnv::with(Options {
        outbox: OutboxMode::QueueMismatch,
        ..Options::default()
    })
    .await;
    let p = populate(&env).await;
    let chat_before = env.chat_snapshot(p.chat).await;
    let attachments_before = env.dump("attachments").await;

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(reply.category().contains("internal"), "{}", reply.text);
    assert!(
        !reply.text.contains("not_registered"),
        "no driver text: {}",
        reply.text
    );
    assert!(
        !reply.text.contains("to delete"),
        "no title: {}",
        reply.text
    );
    // Nothing is deleted: the chat is active, attachments still unmarked, no message.
    assert_eq!(env.chat_snapshot(p.chat).await, chat_before);
    assert_eq!(env.dump("attachments").await, attachments_before);
    assert_eq!(env.chat_cleanup_rows().await.len(), 0);
    assert_eq!(env.outbox_message_count().await, 0);
    let still = env.get(&chat_uri(p.chat)).await;
    assert_eq!(still.status, StatusCode::OK, "{}", still.text);
    // The cause is logged with the chat id, never with the title.
    let errors = logs.at_containing(Level::ERROR, &p.chat.to_string());
    assert!(!errors.is_empty(), "{}", logs.all_text());
    assert!(
        !logs.all_text().contains("to delete"),
        "{}",
        logs.all_text()
    );
}

#[tokio::test]
async fn delete_without_a_running_outbox_is_500_internal_and_deletes_nothing() {
    let env = ChatEnv::new().await;
    let p = populate(&env).await;
    let chat_before = env.chat_snapshot(p.chat).await;
    let attachments_before = env.dump("attachments").await;

    let reply = env.delete(&chat_uri(p.chat)).await;

    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert_eq!(env.chat_snapshot(p.chat).await, chat_before);
    assert_eq!(env.dump("attachments").await, attachments_before);
}

#[tokio::test]
async fn delete_of_an_unknown_or_foreign_chat_is_404_and_changes_nothing() {
    let env = ChatEnv::running_outbox().await;
    let foreign_user = env
        .seed_chat_of(1, USER_TENANT, OTHER_USER, Some("theirs"), 10)
        .await;
    let foreign_tenant = env
        .seed_chat_of(2, OTHER_TENANT, USER, Some("other tenant"), 10)
        .await;
    env.db
        .insert(
            "attachments",
            crate::infra::db::test_support::attachment_row(att_id(1), USER_TENANT, foreign_user)
                .null("cleanup_status"),
        )
        .await
        .unwrap();
    let before = (
        env.chat_snapshot(foreign_user).await,
        env.chat_snapshot(foreign_tenant).await,
        env.dump("attachments").await,
    );

    for id in [chat_id(99), foreign_user, foreign_tenant] {
        let reply = env.delete(&chat_uri(id)).await;

        reply.assert_not_found(CHAT_TYPE);
    }

    let after = (
        env.chat_snapshot(foreign_user).await,
        env.chat_snapshot(foreign_tenant).await,
        env.dump("attachments").await,
    );
    assert_eq!(before, after);
    assert_eq!(env.chat_cleanup_rows().await.len(), 0);
}

#[tokio::test]
async fn concurrent_deletes_of_one_chat_yield_exactly_one_204_and_one_message() {
    let env = ChatEnv::running_outbox().await;
    let id = env.seed_chat(1, Some("race"), 10).await;
    let uri = chat_uri(id);

    let (a, b, c) = tokio::join!(env.delete(&uri), env.delete(&uri), env.delete(&uri));

    let statuses = [a.status, b.status, c.status];
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::NO_CONTENT)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::NOT_FOUND)
            .count(),
        2,
        "{statuses:?}"
    );
    assert_eq!(env.chat_cleanup_rows().await.len(), 1);
}

#[tokio::test]
async fn delete_asks_the_pdp_for_delete_on_the_chat_and_emits_no_other_pdp_call() {
    let env = ChatEnv::running_outbox().await;
    let id = env.seed_chat(1, Some("a"), 10).await;

    env.delete(&chat_uri(id)).await;

    let request = env.only_pdp_request();
    assert_eq!(request.action.name, "delete");
    assert_eq!(request.resource.resource_type, CHAT_PEP_TYPE);
    assert_eq!(request.resource.id, Some(id));
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn delete_makes_the_message_and_reaction_routes_answer_404_for_the_chat_not_the_message() {
    let env = ChatEnv::running_outbox().await;
    let chat = env.seed_chat(1, Some("a"), 10).await;
    let message = env.seed_message(chat, 1, "assistant", 1).await;
    assert_eq!(
        env.delete(&chat_uri(chat)).await.status,
        StatusCode::NO_CONTENT
    );

    let put = env.put_reaction(chat, message, "like").await;
    let delete = env.delete_reaction(chat, message).await;
    let messages = env.list_messages(chat, "").await;

    for reply in [&put, &delete, &messages] {
        assert_ne!(reply.resource_type(), MESSAGE_TYPE);
        reply.assert_not_found(CHAT_TYPE);
    }
    assert_eq!(env.db.count("message_reactions").await, 0);
}
