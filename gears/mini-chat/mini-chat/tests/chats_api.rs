#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Chat CRUD, model/title validation, listing (filter, order, pagination) and the
//! canonical error contract of the chat routes (DESIGN section 3.3, ADR-0004).
//!
//! The tests drive the gear through its HTTP contract only (`mini_chat::test_support`).

mod common;

use common::{
    CHAT_RESOURCE_TYPE, CHATS_PATH, DISABLED_MODEL, ODATA_RESOURCE_TYPE, PREMIUM_MODEL,
    STANDARD_MODEL, assert_chat_detail_shape, assert_invalid_argument, assert_not_found,
    assert_problem, catalog_entry, chat_id, chat_path, chat_row, chat_row_count, create_chat,
    create_default_chat, create_titled_chat, create_titled_chats, delete_chat, env_with_catalog,
    expect_status, field_violation_reason, get_chat, has_violation_reason, item_ids, item_titles,
    items, list_chats, list_chats_ok, next_cursor, parse_ts, patch_chat, post_chat, raw_request,
};
use http::{Method, StatusCode};
use mini_chat::test_support::{TestEnv, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

/// `(method, path, body, expected status, expected canonical category)`.
type ErrorCase = (Method, String, Option<Value>, u16, &'static str);

fn long_title(chars: usize) -> String {
    "a".repeat(chars)
}

fn invalid_titles() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("spaces only", "   ".to_owned()),
        ("whitespace control characters only", "\t\n \r".to_owned()),
        ("256 characters", long_title(256)),
        (
            "256 characters surrounded by spaces",
            format!("  {}  ", long_title(256)),
        ),
    ]
}

async fn env_and_user() -> (TestEnv, TestUser) {
    (TestEnv::new().await, TestUser::new())
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
// Create chat
// ===========================================================================

#[tokio::test]
async fn create_chat_returns_201_with_location_header() {
    let (env, user) = env_and_user().await;

    let resp = post_chat(&env, &user, json!({ "title": "Quarterly report" })).await;

    expect_status(&resp, StatusCode::CREATED);
    let id = chat_id(&resp.json());
    let location = resp
        .headers
        .get(http::header::LOCATION)
        .expect("201 must carry a Location header")
        .to_str()
        .unwrap();
    assert_eq!(location, format!("/mini-chat/v1/chats/{id}"));
}

#[tokio::test]
async fn create_chat_returns_chat_detail_with_documented_fields() {
    let (env, user) = env_and_user().await;

    let chat = create_titled_chat(&env, &user, "Quarterly report").await;

    assert_chat_detail_shape(&chat, true);
    assert_eq!(chat["title"], json!("Quarterly report"));
    assert_eq!(chat["is_temporary"], json!(false));
    assert_eq!(chat["message_count"], json!(0));
}

#[tokio::test]
async fn create_chat_body_never_exposes_user_or_tenant_identity() {
    let (env, user) = env_and_user().await;

    let chat = create_titled_chat(&env, &user, "Identity").await;

    let object = chat.as_object().unwrap();
    for key in [
        "user_id",
        "tenant_id",
        "owner_id",
        "owner_tenant_id",
        "deleted_at",
    ] {
        assert!(
            !object.contains_key(key),
            "`{key}` must not be exposed: {chat}"
        );
    }
}

#[tokio::test]
async fn create_chat_without_title_is_untitled_and_omits_title() {
    let (env, user) = env_and_user().await;

    let chat = create_default_chat(&env, &user).await;

    assert_chat_detail_shape(&chat, false);
    let row = chat_row(&env, &chat_id(&chat)).await.unwrap();
    assert_eq!(
        row.title.as_deref(),
        Some(""),
        "an untitled chat is stored as the empty title so it stays orderable and filterable"
    );
}

#[tokio::test]
async fn create_chat_with_null_title_is_untitled_and_omits_title() {
    let (env, user) = env_and_user().await;

    let chat = create_chat(&env, &user, json!({ "title": null })).await;

    assert_chat_detail_shape(&chat, false);
}

#[tokio::test]
async fn create_chat_trims_title_in_response_and_database() {
    let (env, user) = env_and_user().await;

    let chat = create_titled_chat(&env, &user, "  Padded title \t").await;

    assert_eq!(chat["title"], json!("Padded title"));
    let row = chat_row(&env, &chat_id(&chat)).await.unwrap();
    assert_eq!(row.title.as_deref(), Some("Padded title"));
}

#[tokio::test]
async fn create_chat_rejects_invalid_titles_with_invalid_title_reason() {
    let (env, user) = env_and_user().await;

    for (case, title) in invalid_titles() {
        let resp = post_chat(&env, &user, json!({ "title": title })).await;
        assert_invalid_argument(&resp, "title", "INVALID_TITLE");
        assert_eq!(
            chat_row_count(&env).await,
            0,
            "no row may be created: {case}"
        );
    }
}

#[tokio::test]
async fn create_chat_accepts_title_of_exactly_255_characters() {
    let (env, user) = env_and_user().await;

    let chat = create_titled_chat(&env, &user, &long_title(255)).await;

    assert_eq!(chat["title"].as_str().unwrap().chars().count(), 255);
}

#[tokio::test]
async fn create_chat_title_limit_counts_characters_not_bytes() {
    let (env, user) = env_and_user().await;
    let title = "\u{e9}".repeat(255);

    let chat = create_titled_chat(&env, &user, &title).await;

    assert_eq!(chat["title"].as_str(), Some(title.as_str()));
}

#[tokio::test]
async fn create_chat_title_limit_applies_after_trim() {
    let (env, user) = env_and_user().await;
    let padded = format!("  {}  ", long_title(255));

    let chat = create_titled_chat(&env, &user, &padded).await;

    assert_eq!(chat["title"].as_str(), Some(long_title(255).as_str()));
}

#[tokio::test]
async fn create_chat_with_non_string_title_is_422_invalid_json_body() {
    let (env, user) = env_and_user().await;

    let resp = post_chat(&env, &user, json!({ "title": 5 })).await;

    let problem = assert_problem(&resp, 422, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("invalid_json_body")
    );
}

#[tokio::test]
async fn create_chat_with_malformed_json_is_400_json_syntax_error() {
    let (env, user) = env_and_user().await;

    let req = raw_request(
        Method::POST,
        CHATS_PATH,
        Some("application/json"),
        "{not json",
    );
    let resp = env.request_raw(&user, req).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("json_syntax_error")
    );
}

#[tokio::test]
async fn create_chat_without_json_content_type_is_415() {
    let (env, user) = env_and_user().await;

    let req = raw_request(Method::POST, CHATS_PATH, Some("text/plain"), "{}");
    let resp = env.request_raw(&user, req).await;

    let problem = assert_problem(&resp, 415, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("missing_json_content_type")
    );
}

// ---------------------------------------------------------------------------
// Create chat: model selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_chat_without_model_uses_the_default_model() {
    let (env, user) = env_and_user().await;

    let chat = create_default_chat(&env, &user).await;

    assert_eq!(chat["model"], json!(PREMIUM_MODEL));
}

#[tokio::test]
async fn create_chat_with_explicit_model_stores_it_on_the_chat_row() {
    let (env, user) = env_and_user().await;

    let chat = create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await;

    assert_eq!(chat["model"], json!(STANDARD_MODEL));
    let row = chat_row(&env, &chat_id(&chat)).await.unwrap();
    assert_eq!(row.model, STANDARD_MODEL);
}

#[tokio::test]
async fn create_chat_with_unknown_model_is_400_invalid_model() {
    let (env, user) = env_and_user().await;

    let resp = post_chat(&env, &user, json!({ "model": "no-such-model" })).await;

    assert_invalid_argument(&resp, "model", "INVALID_MODEL");
    assert_eq!(chat_row_count(&env).await, 0);
}

#[tokio::test]
async fn create_chat_with_disabled_model_is_400_invalid_model() {
    let (env, user) = env_and_user().await;

    let resp = post_chat(&env, &user, json!({ "model": DISABLED_MODEL })).await;

    assert_invalid_argument(&resp, "model", "INVALID_MODEL");
    assert_eq!(chat_row_count(&env).await, 0);
}

#[tokio::test]
async fn create_chat_default_model_is_first_enabled_when_none_is_default() {
    let env = env_with_catalog(vec![
        catalog_entry("m-off", "Premium", false, false),
        catalog_entry("m-first", "Standard", true, false),
        catalog_entry("m-second", "Premium", true, false),
    ])
    .await;

    let chat = create_default_chat(&env, &TestUser::new()).await;

    assert_eq!(chat["model"], json!("m-first"));
}

#[tokio::test]
async fn create_chat_default_model_is_the_flagged_default_even_when_listed_later() {
    let env = env_with_catalog(vec![
        catalog_entry("m-first", "Standard", true, false),
        catalog_entry("m-flagged", "Standard", true, true),
    ])
    .await;

    let chat = create_default_chat(&env, &TestUser::new()).await;

    assert_eq!(chat["model"], json!("m-flagged"));
}

#[tokio::test]
async fn create_chat_default_model_does_not_consider_tier() {
    let env = env_with_catalog(vec![
        catalog_entry("m-premium", "Premium", true, false),
        catalog_entry("m-standard-default", "Standard", true, true),
    ])
    .await;

    let chat = create_default_chat(&env, &TestUser::new()).await;

    assert_eq!(chat["model"], json!("m-standard-default"));
}

#[tokio::test]
async fn create_chat_default_model_skips_a_disabled_default_entry() {
    let env = env_with_catalog(vec![
        catalog_entry("m-default-off", "Premium", false, true),
        catalog_entry("m-on", "Standard", true, false),
    ])
    .await;

    let chat = create_default_chat(&env, &TestUser::new()).await;

    assert_eq!(chat["model"], json!("m-on"));
}

#[tokio::test]
async fn create_chat_validates_title_before_model_lookup() {
    let (env, user) = env_and_user().await;

    let resp = post_chat(
        &env,
        &user,
        json!({ "title": "  ", "model": "no-such-model" }),
    )
    .await;

    assert_invalid_argument(&resp, "title", "INVALID_TITLE");
}

// ===========================================================================
// Get chat
// ===========================================================================

#[tokio::test]
async fn get_chat_returns_the_created_chat_detail() {
    let (env, user) = env_and_user().await;
    let created = create_titled_chat(&env, &user, "Readable").await;

    let resp = get_chat(&env, &user, &chat_id(&created)).await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json(), created);
}

#[tokio::test]
async fn get_chat_of_unknown_id_is_404_naming_the_chat_resource() {
    let (env, user) = env_and_user().await;

    let resp = get_chat(&env, &user, &Uuid::new_v4().to_string()).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn chat_routes_reject_a_non_uuid_chat_id_with_400_invalid_path_params() {
    let (env, user) = env_and_user().await;
    let path = chat_path("not-a-uuid");
    let cases = vec![
        (Method::GET, None),
        (Method::PATCH, Some(json!({ "title": "x" }))),
        (Method::DELETE, None),
    ];

    for (method, body) in cases {
        let resp = env.request(&user, method.clone(), &path, body).await;
        let problem = assert_problem(&resp, 400, Some("invalid_argument"));
        assert!(
            has_violation_reason(&problem, "invalid_path_params"),
            "{method} must report invalid_path_params: {problem}"
        );
    }
}

// ===========================================================================
// Update chat title
// ===========================================================================

#[tokio::test]
async fn patch_chat_renames_and_returns_the_updated_chat_detail() {
    let (env, user) = env_and_user().await;
    let created = create_titled_chat(&env, &user, "Before").await;
    let id = chat_id(&created);

    let resp = patch_chat(&env, &user, &id, json!({ "title": "After" })).await;

    expect_status(&resp, StatusCode::OK);
    let updated = resp.json();
    assert_chat_detail_shape(&updated, true);
    assert_eq!(updated["title"], json!("After"));
    assert_eq!(updated["id"], created["id"]);
    assert_eq!(updated["model"], created["model"]);
    assert_eq!(updated["message_count"], json!(0));
    let fetched = get_chat(&env, &user, &id).await.json();
    assert_eq!(fetched["title"], json!("After"));
}

#[tokio::test]
async fn patch_chat_bumps_updated_at_and_keeps_created_at() {
    let (env, user) = env_and_user().await;
    let created = create_titled_chat(&env, &user, "Before").await;

    let resp = patch_chat(&env, &user, &chat_id(&created), json!({ "title": "After" })).await;

    expect_status(&resp, StatusCode::OK);
    let updated = resp.json();
    assert_eq!(updated["created_at"], created["created_at"]);
    assert!(
        parse_ts(&updated["updated_at"]) > parse_ts(&created["updated_at"]),
        "updated_at must move forward: {created} -> {updated}"
    );
}

#[tokio::test]
async fn patch_chat_trims_the_title() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_titled_chat(&env, &user, "Before").await);

    let resp = patch_chat(&env, &user, &id, json!({ "title": "  After  " })).await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json()["title"], json!("After"));
    assert_eq!(
        chat_row(&env, &id).await.unwrap().title.as_deref(),
        Some("After")
    );
}

#[tokio::test]
async fn patch_chat_rejects_invalid_titles_and_leaves_the_chat_unchanged() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_titled_chat(&env, &user, "Stable").await);

    for (case, title) in invalid_titles() {
        let resp = patch_chat(&env, &user, &id, json!({ "title": title })).await;
        assert_invalid_argument(&resp, "title", "INVALID_TITLE");
        let row = chat_row(&env, &id).await.unwrap();
        assert_eq!(
            row.title.as_deref(),
            Some("Stable"),
            "title changed: {case}"
        );
    }
}

#[tokio::test]
async fn patch_chat_accepts_title_of_exactly_255_characters() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_titled_chat(&env, &user, "Short").await);

    let resp = patch_chat(&env, &user, &id, json!({ "title": long_title(255) })).await;

    expect_status(&resp, StatusCode::OK);
}

#[tokio::test]
async fn patch_chat_with_missing_null_or_non_string_title_is_422() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_titled_chat(&env, &user, "Stable").await);
    let bodies = vec![json!({}), json!({ "title": null }), json!({ "title": 7 })];

    for body in bodies {
        let resp = patch_chat(&env, &user, &id, body.clone()).await;
        let problem = assert_problem(&resp, 422, Some("invalid_argument"));
        assert_eq!(
            field_violation_reason(&problem, "body").as_deref(),
            Some("invalid_json_body"),
            "body {body}"
        );
    }
    assert_eq!(
        chat_row(&env, &id).await.unwrap().title.as_deref(),
        Some("Stable")
    );
}

#[tokio::test]
async fn patch_chat_with_malformed_json_is_400_json_syntax_error() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);

    let req = raw_request(
        Method::PATCH,
        &chat_path(&id),
        Some("application/json"),
        "{\"title\":",
    );
    let resp = env.request_raw(&user, req).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        field_violation_reason(&problem, "body").as_deref(),
        Some("json_syntax_error")
    );
}

#[tokio::test]
async fn patch_chat_without_json_content_type_is_415() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);

    let req = raw_request(
        Method::PATCH,
        &chat_path(&id),
        Some("text/plain"),
        "{\"title\":\"x\"}",
    );
    let resp = env.request_raw(&user, req).await;

    assert_problem(&resp, 415, Some("invalid_argument"));
}

#[tokio::test]
async fn patch_chat_of_unknown_id_is_404_naming_the_chat_resource() {
    let (env, user) = env_and_user().await;

    let resp = patch_chat(
        &env,
        &user,
        &Uuid::new_v4().to_string(),
        json!({ "title": "x" }),
    )
    .await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn patch_chat_ignores_a_model_field_and_keeps_the_model() {
    let (env, user) = env_and_user().await;
    let created = create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await;
    let id = chat_id(&created);

    let resp = patch_chat(
        &env,
        &user,
        &id,
        json!({ "title": "x", "model": PREMIUM_MODEL }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    let updated = resp.json();
    assert_eq!(updated["title"], json!("x"));
    assert_eq!(updated["model"], json!(STANDARD_MODEL));
    let row = chat_row(&env, &id).await.unwrap();
    assert_eq!(row.model, STANDARD_MODEL, "chats.model must be immutable");
    assert_eq!(
        get_chat(&env, &user, &id).await.json()["model"],
        json!(STANDARD_MODEL)
    );
}

#[tokio::test]
async fn patch_chat_with_an_unknown_model_value_still_renames_and_keeps_the_model() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);

    let resp = patch_chat(
        &env,
        &user,
        &id,
        json!({ "title": "renamed", "model": "no-such-model" }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json()["model"], json!(PREMIUM_MODEL));
    assert_eq!(chat_row(&env, &id).await.unwrap().model, PREMIUM_MODEL);
}

#[tokio::test]
async fn patch_chat_without_title_cannot_change_the_model() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_chat(&env, &user, json!({ "model": STANDARD_MODEL })).await);

    let resp = patch_chat(&env, &user, &id, json!({ "model": PREMIUM_MODEL })).await;

    assert_problem(&resp, 422, Some("invalid_argument"));
    assert_eq!(chat_row(&env, &id).await.unwrap().model, STANDARD_MODEL);
}

#[tokio::test]
async fn patch_chat_does_not_change_is_temporary() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);

    let resp = patch_chat(
        &env,
        &user,
        &id,
        json!({ "title": "x", "is_temporary": true }),
    )
    .await;

    expect_status(&resp, StatusCode::OK);
    assert_eq!(resp.json()["is_temporary"], json!(false));
}

// ===========================================================================
// Delete chat
// ===========================================================================

#[tokio::test]
async fn delete_chat_returns_204_with_empty_body() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);

    let resp = delete_chat(&env, &user, &id).await;

    expect_status(&resp, StatusCode::NO_CONTENT);
    assert!(resp.body.is_empty());
}

#[tokio::test]
async fn delete_chat_soft_deletes_the_row() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);
    assert!(!chat_row(&env, &id).await.unwrap().deleted);

    delete_chat(&env, &user, &id).await;

    let row = chat_row(&env, &id)
        .await
        .expect("a deleted chat keeps its row");
    assert!(row.deleted, "deleted_at must be set");
}

#[tokio::test]
async fn get_after_delete_is_404() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);
    delete_chat(&env, &user, &id).await;

    let resp = get_chat(&env, &user, &id).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn second_delete_of_the_same_chat_is_404() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_default_chat(&env, &user).await);
    expect_status(&delete_chat(&env, &user, &id).await, StatusCode::NO_CONTENT);

    let resp = delete_chat(&env, &user, &id).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

#[tokio::test]
async fn deleted_chat_cannot_be_renamed() {
    let (env, user) = env_and_user().await;
    let id = chat_id(&create_titled_chat(&env, &user, "Gone").await);
    delete_chat(&env, &user, &id).await;

    let resp = patch_chat(&env, &user, &id, json!({ "title": "Back" })).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert_eq!(
        chat_row(&env, &id).await.unwrap().title.as_deref(),
        Some("Gone")
    );
}

#[tokio::test]
async fn deleted_chat_disappears_from_the_list() {
    let (env, user) = env_and_user().await;
    let ids = create_titled_chats(&env, &user, &["keep", "drop"]).await;
    delete_chat(&env, &user, &ids[1]).await;

    let page = list_chats_ok(&env, &user, &[]).await;

    assert_eq!(item_ids(&page), vec![ids[0].clone()]);
}

#[tokio::test]
async fn delete_chat_of_unknown_id_is_404() {
    let (env, user) = env_and_user().await;

    let resp = delete_chat(&env, &user, &Uuid::new_v4().to_string()).await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
}

// ===========================================================================
// List chats: shape, pagination, limits
// ===========================================================================

#[tokio::test]
async fn list_chats_of_a_new_user_is_empty() {
    let (env, user) = env_and_user().await;

    let page = list_chats_ok(&env, &user, &[]).await;

    assert!(items(&page).is_empty());
    assert!(page["page_info"].is_object(), "{page}");
}

#[tokio::test]
async fn list_chats_items_have_the_chat_detail_shape() {
    let (env, user) = env_and_user().await;
    let titled = create_titled_chat(&env, &user, "Titled").await;
    create_default_chat(&env, &user).await;

    let page = list_chats_ok(&env, &user, &[]).await;

    assert_eq!(items(&page).len(), 2);
    for item in items(&page) {
        let expect_title = item["id"] == titled["id"];
        assert_chat_detail_shape(item, expect_title);
    }
}

#[tokio::test]
async fn list_chats_default_page_size_is_20() {
    let (env, user) = env_and_user().await;
    create_default_chat(&env, &user).await;

    let page = list_chats_ok(&env, &user, &[]).await;

    assert_eq!(page["page_info"]["limit"], json!(20));
}

#[tokio::test]
async fn list_chats_clamps_a_limit_above_100_to_100() {
    let (env, user) = env_and_user().await;
    create_default_chat(&env, &user).await;

    let page = list_chats_ok(&env, &user, &[("limit", "1000")]).await;

    assert_eq!(page["page_info"]["limit"], json!(100));
}

#[tokio::test]
async fn list_chats_limit_zero_is_400_invalid_limit() {
    let (env, user) = env_and_user().await;

    let resp = list_chats(&env, &user, &[("limit", "0")]).await;

    assert_odata_problem(&resp, "$top", "INVALID_LIMIT");
}

#[tokio::test]
async fn list_chats_honours_an_explicit_limit_and_offers_a_next_cursor() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["a", "b", "c"]).await;

    let page = list_chats_ok(&env, &user, &[("limit", "2")]).await;

    assert_eq!(items(&page).len(), 2);
    assert_eq!(page["page_info"]["limit"], json!(2));
    assert!(next_cursor(&page).is_some(), "{page}");
}

#[tokio::test]
async fn list_chats_last_page_has_no_next_cursor() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["a", "b"]).await;

    let page = list_chats_ok(&env, &user, &[("limit", "5")]).await;

    assert_eq!(items(&page).len(), 2);
    assert_eq!(next_cursor(&page), None, "{page}");
}

#[tokio::test]
async fn list_chats_cursor_pagination_visits_every_chat_once_in_order() {
    let (env, user) = env_and_user().await;
    let created = create_titled_chats(&env, &user, &["c1", "c2", "c3", "c4", "c5"]).await;
    let mut expected: Vec<String> = created.clone();
    expected.reverse();

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..=created.len() {
        let page = match cursor.as_deref() {
            None => list_chats_ok(&env, &user, &[("limit", "2")]).await,
            Some(cursor) => list_chats_ok(&env, &user, &[("limit", "2"), ("cursor", cursor)]).await,
        };
        assert!(items(&page).len() <= 2, "{page}");
        seen.extend(item_ids(&page));
        cursor = next_cursor(&page);
        if cursor.is_none() {
            break;
        }
    }

    assert_eq!(cursor, None, "pagination must terminate");
    assert_eq!(seen, expected, "each chat exactly once, most recent first");
}

#[tokio::test]
async fn list_chats_malformed_cursor_is_400_invalid_cursor() {
    let (env, user) = env_and_user().await;
    create_default_chat(&env, &user).await;

    let resp = list_chats(&env, &user, &[("cursor", "definitely-not-a-cursor")]).await;

    assert_odata_problem(&resp, "cursor", "INVALID_CURSOR");
}

#[tokio::test]
async fn list_chats_cursor_combined_with_orderby_is_400_order_with_cursor() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["a", "b"]).await;
    let first = list_chats_ok(&env, &user, &[("limit", "1")]).await;
    let cursor = next_cursor(&first).expect("two chats at limit=1 must yield a cursor");

    let resp = list_chats(
        &env,
        &user,
        &[("cursor", cursor.as_str()), ("$orderby", "title asc")],
    )
    .await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "ORDER_WITH_CURSOR"),
        "{problem}"
    );
}

// ===========================================================================
// List chats: $filter, $orderby, $select
// ===========================================================================

#[tokio::test]
async fn list_chats_filters_by_title() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["Alpha", "Beta", "Gamma"]).await;

    let page = list_chats_ok(&env, &user, &[("$filter", "title eq 'Beta'")]).await;

    assert_eq!(item_titles(&page), vec!["Beta".to_owned()]);
}

#[tokio::test]
async fn list_chats_filters_by_id() {
    let (env, user) = env_and_user().await;
    let ids = create_titled_chats(&env, &user, &["Alpha", "Beta"]).await;

    let filter = format!("id eq {}", ids[0]);
    let page = list_chats_ok(&env, &user, &[("$filter", filter.as_str())]).await;

    assert_eq!(item_ids(&page), vec![ids[0].clone()]);
}

#[tokio::test]
async fn list_chats_filters_by_updated_at_lower_bound() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["Alpha", "Beta"]).await;

    let page = list_chats_ok(
        &env,
        &user,
        &[("$filter", "updated_at gt 2000-01-01T00:00:00Z")],
    )
    .await;

    assert_eq!(items(&page).len(), 2, "{page}");
}

#[tokio::test]
async fn list_chats_filters_by_updated_at_upper_bound() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["Alpha", "Beta"]).await;

    let page = list_chats_ok(
        &env,
        &user,
        &[("$filter", "updated_at lt 2000-01-01T00:00:00Z")],
    )
    .await;

    assert!(items(&page).is_empty(), "{page}");
}

#[tokio::test]
async fn list_chats_orders_by_title_ascending() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["b", "c", "a"]).await;

    let page = list_chats_ok(&env, &user, &[("$orderby", "title asc")]).await;

    assert_eq!(item_titles(&page), vec!["a", "b", "c"]);
}

#[tokio::test]
async fn list_chats_orders_by_title_descending() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["b", "c", "a"]).await;

    let page = list_chats_ok(&env, &user, &[("$orderby", "title desc")]).await;

    assert_eq!(item_titles(&page), vec!["c", "b", "a"]);
}

#[tokio::test]
async fn list_chats_title_ordering_paginates_with_a_cursor() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["b", "c", "a"]).await;
    let first = list_chats_ok(&env, &user, &[("$orderby", "title asc"), ("limit", "2")]).await;
    let cursor = next_cursor(&first).expect("3 chats at limit=2 must yield a cursor");

    let second = list_chats_ok(&env, &user, &[("limit", "2"), ("cursor", cursor.as_str())]).await;

    assert_eq!(item_titles(&first), vec!["a", "b"]);
    assert_eq!(item_titles(&second), vec!["c"]);
}

#[tokio::test]
async fn list_chats_filter_and_orderby_combine() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["b", "c", "a"]).await;

    let page = list_chats_ok(
        &env,
        &user,
        &[("$filter", "title ne 'b'"), ("$orderby", "title desc")],
    )
    .await;

    assert_eq!(item_titles(&page), vec!["c", "a"]);
}

/// Creates 3 untitled chats and one titled `"m"`; returns `(untitled ids, titled id)`.
async fn create_untitled_and_titled(env: &TestEnv, user: &TestUser) -> (Vec<String>, String) {
    let mut untitled = Vec::new();
    for _ in 0..3 {
        untitled.push(chat_id(&create_default_chat(env, user).await));
    }
    let titled = chat_id(&create_titled_chat(env, user, "m").await);
    (untitled, titled)
}

/// Follows `next_cursor` from `$orderby=title <dir>&limit=2`; returns the ids in visit order.
async fn page_all_by_title(env: &TestEnv, user: &TestUser, dir: &str) -> Vec<String> {
    let orderby = format!("title {dir}");
    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..6 {
        let page = match cursor.as_deref() {
            None => {
                list_chats_ok(env, user, &[("$orderby", orderby.as_str()), ("limit", "2")]).await
            }
            Some(cursor) => list_chats_ok(env, user, &[("cursor", cursor), ("limit", "2")]).await,
        };
        seen.extend(item_ids(&page));
        cursor = next_cursor(&page);
        if cursor.is_none() {
            return seen;
        }
    }
    panic!("pagination must terminate: {seen:?}");
}

#[tokio::test]
async fn list_chats_title_pagination_includes_untitled_chats_ascending() {
    let (env, user) = env_and_user().await;
    let (untitled, titled) = create_untitled_and_titled(&env, &user).await;

    let seen = page_all_by_title(&env, &user, "asc").await;

    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    let mut expected: Vec<String> = untitled.clone();
    expected.push(titled.clone());
    expected.sort();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(sorted, expected, "each chat exactly once");
    assert_eq!(seen.last(), Some(&titled), "untitled chats sort first");
}

#[tokio::test]
async fn list_chats_title_pagination_includes_untitled_chats_descending() {
    let (env, user) = env_and_user().await;
    let (untitled, titled) = create_untitled_and_titled(&env, &user).await;

    let seen = page_all_by_title(&env, &user, "desc").await;

    let mut sorted = seen.clone();
    sorted.sort();
    sorted.dedup();
    let mut expected: Vec<String> = untitled.clone();
    expected.push(titled.clone());
    expected.sort();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert_eq!(sorted, expected, "each chat exactly once");
    assert_eq!(seen.first(), Some(&titled), "untitled chats sort last");
}

#[tokio::test]
async fn list_chats_title_ne_filter_includes_untitled_chats() {
    let (env, user) = env_and_user().await;
    let (untitled, titled) = create_untitled_and_titled(&env, &user).await;
    create_titled_chat(&env, &user, "x").await;

    let page = list_chats_ok(&env, &user, &[("$filter", "title ne 'x'")]).await;

    let mut seen = item_ids(&page);
    seen.sort();
    let mut expected: Vec<String> = untitled;
    expected.push(titled);
    expected.sort();
    assert_eq!(seen, expected, "{page}");
}

#[tokio::test]
async fn list_chats_unknown_filter_field_is_400_with_odata_resource_type() {
    let (env, user) = env_and_user().await;

    let resp = list_chats(&env, &user, &[("$filter", "no_such_field eq 1")]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        problem["context"]["resource_type"].as_str(),
        Some(ODATA_RESOURCE_TYPE),
        "{problem}"
    );
}

#[tokio::test]
async fn list_chats_filter_on_an_internal_column_is_rejected() {
    let (env, user) = env_and_user().await;

    let resp = list_chats(&env, &user, &[("$filter", "user_id eq 1")]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert_eq!(
        problem["context"]["resource_type"].as_str(),
        Some(ODATA_RESOURCE_TYPE),
        "{problem}"
    );
}

#[tokio::test]
async fn list_chats_malformed_filter_is_400_invalid_filter() {
    let (env, user) = env_and_user().await;

    let resp = list_chats(&env, &user, &[("$filter", "title eq")]).await;

    assert_odata_problem(&resp, "$filter", "INVALID_FILTER");
}

#[tokio::test]
async fn list_chats_unknown_orderby_field_is_400_invalid_orderby_field() {
    let (env, user) = env_and_user().await;
    create_default_chat(&env, &user).await;

    let resp = list_chats(&env, &user, &[("$orderby", "no_such_field asc")]).await;

    assert_odata_problem(&resp, "$orderby", "INVALID_ORDERBY_FIELD");
}

#[tokio::test]
async fn list_chats_unsupported_query_option_is_400_unsupported_query_param() {
    let (env, user) = env_and_user().await;

    let resp = list_chats(&env, &user, &[("$skip", "1")]).await;

    let problem = assert_problem(&resp, 400, Some("invalid_argument"));
    assert!(
        has_violation_reason(&problem, "UNSUPPORTED_QUERY_PARAM"),
        "{problem}"
    );
}

#[tokio::test]
async fn list_chats_accepts_select_and_still_returns_full_items() {
    let (env, user) = env_and_user().await;
    create_titled_chat(&env, &user, "Selected").await;

    let page = list_chats_ok(&env, &user, &[("$select", "id")]).await;

    assert_eq!(items(&page).len(), 1);
    assert_chat_detail_shape(&items(&page)[0], true);
}

// ===========================================================================
// Chat ordering reflects the most recent activity
// ===========================================================================

#[tokio::test]
async fn list_chats_default_order_is_most_recently_updated_first() {
    let (env, user) = env_and_user().await;
    create_titled_chats(&env, &user, &["first", "second", "third"]).await;

    let page = list_chats_ok(&env, &user, &[]).await;

    assert_eq!(item_titles(&page), vec!["third", "second", "first"]);
}

#[tokio::test]
async fn renaming_an_older_chat_moves_it_to_the_top_of_the_list() {
    let (env, user) = env_and_user().await;
    let ids = create_titled_chats(&env, &user, &["oldest", "middle", "newest"]).await;

    let resp = patch_chat(&env, &user, &ids[0], json!({ "title": "oldest renamed" })).await;
    expect_status(&resp, StatusCode::OK);

    let page = list_chats_ok(&env, &user, &[]).await;
    assert_eq!(
        item_titles(&page),
        vec!["oldest renamed", "newest", "middle"]
    );
}

#[tokio::test]
async fn updated_at_of_the_listed_item_matches_the_chat_detail() {
    let (env, user) = env_and_user().await;
    let created = create_titled_chat(&env, &user, "Consistent").await;
    let id = chat_id(&created);
    let renamed = patch_chat(&env, &user, &id, json!({ "title": "Consistent 2" }))
        .await
        .json();

    let page = list_chats_ok(&env, &user, &[]).await;

    assert_eq!(items(&page)[0]["updated_at"], renamed["updated_at"]);
}

// ===========================================================================
// Canonical error contract (ADR-0004) on the chat routes
// ===========================================================================

#[tokio::test]
async fn chat_route_errors_are_canonical_problems_with_category_status_and_no_code() {
    let (env, user) = env_and_user().await;
    let missing = chat_path(&Uuid::new_v4().to_string());
    let cases: Vec<ErrorCase> = vec![
        (Method::GET, missing.clone(), None, 404, "not_found"),
        (
            Method::PATCH,
            missing.clone(),
            Some(json!({ "title": "x" })),
            404,
            "not_found",
        ),
        (Method::DELETE, missing, None, 404, "not_found"),
        (
            Method::GET,
            chat_path("nope"),
            None,
            400,
            "invalid_argument",
        ),
        (
            Method::POST,
            CHATS_PATH.to_owned(),
            Some(json!({ "title": " " })),
            400,
            "invalid_argument",
        ),
        (
            Method::POST,
            CHATS_PATH.to_owned(),
            Some(json!({ "model": "nope" })),
            400,
            "invalid_argument",
        ),
        (
            Method::POST,
            CHATS_PATH.to_owned(),
            Some(json!({ "title": 1 })),
            422,
            "invalid_argument",
        ),
        (
            Method::GET,
            format!("{CHATS_PATH}?limit=0"),
            None,
            400,
            "invalid_argument",
        ),
    ];

    for (method, path, body, status, category) in cases {
        let resp = env.request(&user, method.clone(), &path, body).await;
        let problem = assert_problem(&resp, status, Some(category));
        assert!(problem["detail"].is_string(), "{method} {path}: {problem}");
        assert!(problem["title"].is_string(), "{method} {path}: {problem}");
    }
}

#[tokio::test]
async fn not_found_problem_names_the_chat_resource_type() {
    let (env, user) = env_and_user().await;

    let resp = get_chat(&env, &user, &Uuid::new_v4().to_string()).await;

    let problem = assert_problem(&resp, 404, Some("not_found"));
    assert_eq!(
        problem["context"]["resource_type"],
        json!("gts.cf.core.mini_chat.chat.v1~")
    );
}
