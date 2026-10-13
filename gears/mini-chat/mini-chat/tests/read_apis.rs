//! Messages list, reactions, Models API and quota status over REST
//! (DESIGN §3.3 "List Messages", "Message Reaction API", "Models API",
//! §3.2 "Quota Status Endpoint").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::{Method, StatusCode};
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use mini_chat::domain::clock::now_utc;
use mini_chat::domain::estimation::period_starts;
use mini_chat::domain::model::PeriodType;
use mini_chat::infra::db::entities::message_reaction;
use mini_chat::testing::seed::{self, NewAttachment, NewMessage};
use mini_chat::testing::{PdpMode, TestApp, TestResponse, TestUser, catalog, midnight_safe};
use mini_chat_sdk::TierLimits;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use toolkit_db::secure::{AccessScope, SecureEntityExt};
use uuid::Uuid;

const CHATS: &str = "/mini-chat/v1/chats";
const MODELS: &str = "/mini-chat/v1/models";
const QUOTA: &str = "/mini-chat/v1/quota/status";

async fn app() -> TestApp {
    TestApp::builder().build().await
}

async fn create_chat(app: &TestApp, user: TestUser) -> Uuid {
    let r = app.call(user, Method::POST, CHATS, Some(json!({}))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    Uuid::parse_str(r.json["id"].as_str().unwrap()).unwrap()
}

fn messages_path(chat: Uuid) -> String {
    format!("{CHATS}/{chat}/messages")
}

fn reaction_path(chat: Uuid, msg: Uuid) -> String {
    format!("{CHATS}/{chat}/messages/{msg}/reaction")
}

/// 2026-01-01T00:00:00Z plus `secs` seconds.
fn t(secs: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(secs)
}

async fn get(app: &TestApp, user: TestUser, path: &str) -> TestResponse {
    app.call(user, Method::GET, path, None).await
}

fn ids(r: &TestResponse) -> Vec<String> {
    r.json["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A user message and its assistant answer of one turn.
async fn seed_turn(app: &TestApp, chat: Uuid, at: i64) -> (Uuid, Uuid, Uuid) {
    let request_id = Uuid::new_v4();
    let user =
        seed::insert_message(&app.db, chat, "user", "question", Some(request_id), t(at)).await;
    let assistant = seed::insert_message_with(
        &app.db,
        NewMessage::new(chat, "assistant", "answer", Some(request_id), t(at + 1))
            .model("gpt-premium")
            .tokens(10, 20),
    )
    .await;
    (request_id, user, assistant)
}

// ---------------------------------------------------------------------------------------------
// Messages list
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn messages_chronological_with_required_fields() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    // Inserted out of order: the list is chronological.
    let (req, user_id, assistant_id) = seed_turn(&app, chat, 10).await;
    let first =
        seed::insert_message(&app.db, chat, "user", "earlier", Some(Uuid::new_v4()), t(0)).await;
    let zero_out = seed::insert_message_with(
        &app.db,
        NewMessage::new(chat, "assistant", "partial", Some(Uuid::new_v4()), t(20))
            .model("gpt-standard")
            .tokens(5, 0),
    )
    .await;

    let r = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(
        ids(&r),
        [first, user_id, assistant_id, zero_out].map(|u| u.to_string())
    );
    assert_eq!(r.json["page_info"]["limit"], 20);
    assert!(r.json["page_info"]["next_cursor"].is_null());

    let items = r.json["items"].as_array().unwrap();
    for m in items {
        assert!(m["request_id"].is_string(), "{m}");
        assert_eq!(m["attachments"], json!([]), "{m}");
        assert!(m.get("my_reaction").is_some_and(Value::is_null), "{m}");
        assert!(m["created_at"].is_string(), "{m}");
        assert!(m["content"].is_string(), "{m}");
        assert!(
            m.get("chat_id").is_none() && m.get("tenant_id").is_none(),
            "{m}"
        );
    }
    let user_msg = &items[1];
    assert_eq!(user_msg["role"], "user");
    assert_eq!(user_msg["request_id"], req.to_string());
    assert!(user_msg.get("model").is_none(), "{user_msg}");
    assert!(user_msg.get("input_tokens").is_none(), "{user_msg}");
    assert!(user_msg.get("output_tokens").is_none(), "{user_msg}");

    let assistant = &items[2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["request_id"], req.to_string());
    assert_eq!(assistant["model"], "gpt-premium");
    assert_eq!(assistant["input_tokens"], 10);
    assert_eq!(assistant["output_tokens"], 20);
    assert_eq!(assistant["content"], "answer");

    // Zero counts are omitted, non-zero ones stay.
    let partial = &items[3];
    assert_eq!(partial["input_tokens"], 5);
    assert!(partial.get("output_tokens").is_none(), "{partial}");
}

#[tokio::test]
async fn messages_pagination_filter_orderby() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let mut all = Vec::new();
    for i in 0..3 {
        let (_, u, a) = seed_turn(&app, chat, i * 10).await;
        all.push((u.to_string(), a.to_string()));
    }
    let users: Vec<_> = all.iter().map(|p| p.0.clone()).collect();
    let assistants: Vec<_> = all.iter().map(|p| p.1.clone()).collect();

    // Default order, cursor paging over all six messages.
    let mut seen = Vec::new();
    let mut url = format!("{}?limit=4", messages_path(chat));
    loop {
        let r = get(&app, TestUser::A1, &url).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        seen.extend(ids(&r));
        match r.json["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{}?limit=4&cursor={c}", messages_path(chat)),
            None => break,
        }
    }
    let expected: Vec<_> = all
        .iter()
        .flat_map(|p| [p.0.clone(), p.1.clone()])
        .collect();
    assert_eq!(seen, expected);

    // Filter + order desc, then the cursor (it carries the order and the filter hash;
    // `$orderby` must not be repeated next to a cursor).
    let filter = "$filter=role%20eq%20'assistant'";
    let first = format!(
        "{}?{filter}&$orderby=created_at%20desc&limit=2",
        messages_path(chat)
    );
    let r = get(&app, TestUser::A1, &first).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(ids(&r), [assistants[2].clone(), assistants[1].clone()]);
    let cursor = r.json["page_info"]["next_cursor"].as_str().unwrap();
    let r = get(
        &app,
        TestUser::A1,
        &format!("{}?{filter}&limit=2&cursor={cursor}", messages_path(chat)),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(ids(&r), [assistants[0].clone()]);
    assert!(r.json["page_info"]["next_cursor"].is_null());
    assert!(r.json["page_info"]["prev_cursor"].is_string());

    // A cursor issued for a filter is rejected without it.
    let r = get(
        &app,
        TestUser::A1,
        &format!("{}?cursor={cursor}", messages_path(chat)),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);

    let r = get(
        &app,
        TestUser::A1,
        &format!(
            "{}?$filter=role%20eq%20'user'&$orderby=id%20desc",
            messages_path(chat)
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let mut by_id_desc = users.clone();
    by_id_desc.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids(&r), by_id_desc);

    // Unknown field, bad cursor, limit=0 are 400s; a large limit is clamped.
    for q in ["$filter=content%20eq%20'x'", "cursor=garbage", "limit=0"] {
        let r = get(&app, TestUser::A1, &format!("{}?{q}", messages_path(chat))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}: {}", r.json);
    }
    let r = get(
        &app,
        TestUser::A1,
        &format!("{}?limit=500", messages_path(chat)),
    )
    .await;
    assert_eq!(r.json["page_info"]["limit"], 100);
}

#[tokio::test]
async fn messages_equal_timestamps_break_ties_by_id_asc() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let mut tied = Vec::new();
    for _ in 0..3 {
        tied.push(
            seed::insert_message(&app.db, chat, "user", "same", Some(Uuid::new_v4()), t(5))
                .await
                .to_string(),
        );
    }
    tied.sort();
    let mut seen = Vec::new();
    let mut url = format!("{}?limit=1", messages_path(chat));
    loop {
        let r = get(&app, TestUser::A1, &url).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        seen.extend(ids(&r));
        match r.json["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{}?limit=1&cursor={c}", messages_path(chat)),
            None => break,
        }
    }
    assert_eq!(seen, tied);
}

#[tokio::test]
async fn messages_exclude_soft_deleted() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let kept =
        seed::insert_message(&app.db, chat, "user", "kept", Some(Uuid::new_v4()), t(0)).await;
    seed::insert_message_with(
        &app.db,
        NewMessage::new(chat, "user", "gone", Some(Uuid::new_v4()), t(1)).deleted(t(2)),
    )
    .await;
    let r = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(ids(&r), [kept.to_string()]);

    // A soft-deleted chat is 404.
    let r = app
        .call(
            TestUser::A1,
            Method::DELETE,
            &format!("{CHATS}/{chat}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
}

#[tokio::test]
async fn messages_of_foreign_chat_404() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    seed::insert_message(&app.db, chat, "user", "secret", Some(Uuid::new_v4()), t(0)).await;
    for user in [TestUser::A2, TestUser::B1] {
        let r = get(&app, user, &messages_path(chat)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        assert_eq!(
            r.json["context"]["resource_type"],
            "gts.cf.core.mini_chat.chat.v1~"
        );
    }
    let r = get(&app, TestUser::A1, &messages_path(Uuid::new_v4())).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
}

#[tokio::test]
async fn messages_null_request_id_is_500() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    seed::insert_message(&app.db, chat, "user", "orphan", None, t(0)).await;
    let r = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR, "{}", r.json);
}

#[tokio::test]
async fn messages_list_authz_denied_403() {
    let app = TestApp::builder().pdp(PdpMode::Deny).build().await;
    let r = get(&app, TestUser::A1, &messages_path(Uuid::new_v4())).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
}

#[tokio::test]
async fn message_attachments_listed_without_deleted() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, user_msg, assistant_msg) = seed_turn(&app, chat, 0).await;
    let other_user = seed::insert_message(
        &app.db,
        chat,
        "user",
        "no files",
        Some(Uuid::new_v4()),
        t(50),
    )
    .await;

    let doc = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, TestUser::A1.user_id, "report.pdf"),
    )
    .await;
    let img = seed::insert_attachment(
        &app.db,
        NewAttachment::image(
            chat,
            TestUser::A1.user_id,
            "cat.png",
            Some((vec![1, 2, 3, 4], 64, 32)),
        ),
    )
    .await;
    let img_failed = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, TestUser::A1.user_id, "bad.png", Some((vec![9], 8, 8)))
            .status("failed"),
    )
    .await;
    let img_no_thumb = seed::insert_attachment(
        &app.db,
        NewAttachment::image(chat, TestUser::A1.user_id, "plain.png", None),
    )
    .await;
    let deleted = seed::insert_attachment(
        &app.db,
        NewAttachment::document(chat, TestUser::A1.user_id, "old.pdf").deleted(t(100)),
    )
    .await;
    for a in [doc, img, img_failed, img_no_thumb, deleted] {
        seed::link_attachment(&app.db, chat, user_msg, a).await;
    }

    let r = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let items = r.json["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["id"], user_msg.to_string());
    assert_eq!(items[1]["id"], assistant_msg.to_string());
    assert_eq!(items[1]["attachments"], json!([]));
    assert_eq!(items[2]["id"], other_user.to_string());
    assert_eq!(items[2]["attachments"], json!([]));

    let atts = items[0]["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 4, "deleted attachment must be absent: {atts:?}");
    let by_name = |name: &str| {
        atts.iter()
            .find(|a| a["filename"] == name)
            .unwrap_or_else(|| panic!("{name} missing in {atts:?}"))
    };
    assert!(atts.iter().all(|a| a["filename"] != "old.pdf"));

    let pdf = by_name("report.pdf");
    assert_eq!(pdf["attachment_id"], doc.to_string());
    assert_eq!(pdf["kind"], "document");
    assert_eq!(pdf["status"], "ready");
    // Optional fields are omitted, not null.
    assert_eq!(
        pdf.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["attachment_id", "filename", "kind", "status"]),
    );

    let png = by_name("cat.png");
    assert_eq!(png["kind"], "image");
    assert_eq!(png["status"], "ready");
    assert_eq!(
        png["img_thumbnail"],
        json!({
            "content_type": "image/webp",
            "width": 64,
            "height": 32,
            "data_base64": "AQIDBA==",
        })
    );

    // Only ready images carry a thumbnail.
    let failed = by_name("bad.png");
    assert_eq!(failed["status"], "failed");
    assert!(failed.get("img_thumbnail").is_none(), "{failed}");
    let plain = by_name("plain.png");
    assert!(plain.get("img_thumbnail").is_none(), "{plain}");
}

// ---------------------------------------------------------------------------------------------
// Reactions
// ---------------------------------------------------------------------------------------------

async fn put_reaction(
    app: &TestApp,
    user: TestUser,
    chat: Uuid,
    msg: Uuid,
    body: Value,
) -> TestResponse {
    app.call(user, Method::PUT, &reaction_path(chat, msg), Some(body))
        .await
}

async fn reaction_rows(app: &TestApp, msg: Uuid) -> Vec<message_reaction::Model> {
    let conn = app.db.conn().unwrap();
    message_reaction::Entity::find()
        .filter(message_reaction::Column::MessageId.eq(msg))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn reaction_put_upserts_and_shows_in_list() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, _, assistant) = seed_turn(&app, chat, 0).await;
    // Another user's reaction on the same message must not leak into A1's view.
    seed::insert_reaction(&app.db, assistant, TestUser::A2.user_id, "like").await;

    let r = put_reaction(
        &app,
        TestUser::A1,
        chat,
        assistant,
        json!({"reaction": "like"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["message_id"], assistant.to_string());
    assert_eq!(r.json["reaction"], "like");
    assert!(r.json["created_at"].is_string(), "{}", r.json);
    assert_eq!(r.json.as_object().unwrap().len(), 3, "{}", r.json);

    let list = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(list.json["items"][0]["my_reaction"], Value::Null);
    assert_eq!(list.json["items"][1]["my_reaction"], "like");

    // Dislike replaces like.
    let r = put_reaction(
        &app,
        TestUser::A1,
        chat,
        assistant,
        json!({"reaction": "dislike"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["reaction"], "dislike");
    let list = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(list.json["items"][1]["my_reaction"], "dislike");

    let rows = reaction_rows(&app, assistant).await;
    assert_eq!(rows.len(), 2, "one row per (message, user)");
    let mine: Vec<_> = rows
        .iter()
        .filter(|r| r.user_id == TestUser::A1.user_id)
        .collect();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].reaction, "dislike");
    assert_eq!(mine[0].tenant_id, TestUser::A1.tenant_id);
    let theirs = rows
        .iter()
        .find(|r| r.user_id == TestUser::A2.user_id)
        .unwrap();
    assert_eq!(theirs.reaction, "like");

    // Same value again is idempotent.
    let r = put_reaction(
        &app,
        TestUser::A1,
        chat,
        assistant,
        json!({"reaction": "dislike"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(reaction_rows(&app, assistant).await.len(), 2);

    // Remove: gone from the list, the other user's row stays.
    let r = app
        .call(
            TestUser::A1,
            Method::DELETE,
            &reaction_path(chat, assistant),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let list = get(&app, TestUser::A1, &messages_path(chat)).await;
    assert_eq!(list.json["items"][1]["my_reaction"], Value::Null);
    let rows = reaction_rows(&app, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, TestUser::A2.user_id);
}

#[tokio::test]
async fn reaction_on_user_message_400_reaction_target() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, user_msg, _) = seed_turn(&app, chat, 0).await;
    let system_msg =
        seed::insert_message(&app.db, chat, "system", "note", Some(Uuid::new_v4()), t(30)).await;

    for msg in [user_msg, system_msg] {
        let put = put_reaction(&app, TestUser::A1, chat, msg, json!({"reaction": "like"})).await;
        let del = app
            .call(
                TestUser::A1,
                Method::DELETE,
                &reaction_path(chat, msg),
                None,
            )
            .await;
        for r in [put, del] {
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
            let v = &r.json["context"]["violations"][0];
            assert_eq!(v["subject"], "reaction_target", "{}", r.json);
            assert_eq!(v["type"], "STATE", "{}", r.json);
        }
        assert!(reaction_rows(&app, msg).await.is_empty());
    }
}

#[tokio::test]
async fn reaction_invalid_value_400_before_authz() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, _, assistant) = seed_turn(&app, chat, 0).await;
    for bad in ["love", "", "LIKE", " like"] {
        let r = put_reaction(
            &app,
            TestUser::A1,
            chat,
            assistant,
            json!({"reaction": bad}),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad:?}: {}", r.json);
        let v = &r.json["context"]["field_violations"];
        assert_eq!(v.as_array().map(Vec::len), Some(1), "{}", r.json);
        assert_eq!(v[0]["field"], "reaction");
        assert_eq!(v[0]["reason"], "INVALID_REACTION");
    }
    assert!(reaction_rows(&app, assistant).await.is_empty());

    // Validation precedes the PDP: a denying PDP still yields 400, not 403.
    app.pdp.set_mode(PdpMode::Deny);
    let r = put_reaction(
        &app,
        TestUser::A1,
        chat,
        assistant,
        json!({"reaction": "love"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    // A valid value is denied.
    let r = put_reaction(
        &app,
        TestUser::A1,
        chat,
        assistant,
        json!({"reaction": "like"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let r = app
        .call(
            TestUser::A1,
            Method::DELETE,
            &reaction_path(chat, assistant),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
}

#[tokio::test]
async fn reaction_missing_field_422() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, _, assistant) = seed_turn(&app, chat, 0).await;
    for body in [json!({}), json!({"reaction": null}), json!({"reaction": 1})] {
        let r = put_reaction(&app, TestUser::A1, chat, assistant, body.clone()).await;
        assert_eq!(
            r.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            r.json
        );
    }
}

#[tokio::test]
async fn reaction_delete_idempotent_204() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, _, assistant) = seed_turn(&app, chat, 0).await;
    for _ in 0..2 {
        let r = app
            .call(
                TestUser::A1,
                Method::DELETE,
                &reaction_path(chat, assistant),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.json);
        assert!(r.json.is_null());
    }
}

#[tokio::test]
async fn reaction_unknown_message_404_message() {
    let app = app().await;
    let chat = create_chat(&app, TestUser::A1).await;
    let (_, _, assistant) = seed_turn(&app, chat, 0).await;
    let other_chat = create_chat(&app, TestUser::A1).await;
    let (_, _, foreign_assistant) = seed_turn(&app, other_chat, 0).await;
    let deleted = seed::insert_message_with(
        &app.db,
        NewMessage::new(chat, "assistant", "gone", Some(Uuid::new_v4()), t(40)).deleted(t(41)),
    )
    .await;

    // Unknown id, a message of another chat, a soft-deleted message.
    for msg in [Uuid::new_v4(), foreign_assistant, deleted] {
        let put = put_reaction(&app, TestUser::A1, chat, msg, json!({"reaction": "like"})).await;
        let del = app
            .call(
                TestUser::A1,
                Method::DELETE,
                &reaction_path(chat, msg),
                None,
            )
            .await;
        for r in [put, del] {
            assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
            assert_eq!(
                r.json["context"]["resource_type"],
                "gts.cf.core.mini_chat.message.v1~"
            );
        }
    }

    // Unknown or foreign chat: 404 chat.
    for (user, chat_id) in [(TestUser::A1, Uuid::new_v4()), (TestUser::A2, chat)] {
        let r = put_reaction(&app, user, chat_id, assistant, json!({"reaction": "like"})).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        assert_eq!(
            r.json["context"]["resource_type"],
            "gts.cf.core.mini_chat.chat.v1~"
        );
        let r = app
            .call(
                user,
                Method::DELETE,
                &reaction_path(chat_id, assistant),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
    }
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn models_list_only_enabled_without_internal_fields() {
    let mut disabled = catalog::standard_model("gpt-off");
    disabled.enabled = false;
    let mut described = catalog::premium_model("gpt-described");
    described.description = "Best for reasoning".to_owned();
    described.multiplier_display = "2x".to_owned();
    described.multimodal_capabilities = vec!["VISION_INPUT".to_owned(), "RAG".to_owned()];
    described.context_window = 200_000;
    let mut models = catalog::default_catalog();
    models.push(disabled);
    models.push(described);
    let app = TestApp::builder().catalog(models).build().await;

    let r = get(&app, TestUser::A1, MODELS).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let items = r.json["items"].as_array().unwrap();
    let model_ids: Vec<_> = items
        .iter()
        .map(|m| m["model_id"].as_str().unwrap())
        .collect();
    assert_eq!(model_ids, ["gpt-premium", "gpt-standard", "gpt-described"]);
    assert_eq!(r.json.as_object().unwrap().len(), 1);

    for m in items {
        let keys: std::collections::BTreeSet<_> =
            m.as_object().unwrap().keys().map(String::as_str).collect();
        let mut expected = std::collections::BTreeSet::from([
            "model_id",
            "display_name",
            "tier",
            "multiplier_display",
            "multimodal_capabilities",
            "context_window",
        ]);
        if m.get("description").is_some() {
            expected.insert("description");
        }
        assert_eq!(keys, expected, "{m}");
    }
    assert_eq!(items[0]["tier"], "premium");
    assert_eq!(items[0]["multimodal_capabilities"], json!(["VISION_INPUT"]));
    assert_eq!(items[0]["context_window"], 128_000);
    assert!(
        items[0].get("description").is_none(),
        "empty description is omitted"
    );
    assert_eq!(items[1]["tier"], "standard");
    assert_eq!(items[2]["description"], "Best for reasoning");
    assert_eq!(items[2]["multiplier_display"], "2x");
    assert_eq!(
        items[2]["multimodal_capabilities"],
        json!(["VISION_INPUT", "RAG"])
    );
    assert_eq!(items[2]["context_window"], 200_000);

    let one = get(&app, TestUser::A1, &format!("{MODELS}/gpt-described")).await;
    assert_eq!(one.status, StatusCode::OK, "{}", one.json);
    assert_eq!(&one.json, &items[2]);
}

#[tokio::test]
async fn get_disabled_model_404_model_resource() {
    let mut disabled = catalog::standard_model("gpt-off");
    disabled.enabled = false;
    let mut models = catalog::default_catalog();
    models.push(disabled);
    let app = TestApp::builder().catalog(models).build().await;

    for id in ["gpt-off", "nope"] {
        let r = get(&app, TestUser::A1, &format!("{MODELS}/{id}")).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{id}: {}", r.json);
        assert_eq!(
            r.json["context"]["resource_type"],
            "gts.cf.core.mini_chat.model.v1~"
        );
    }
}

#[tokio::test]
async fn models_authz_denied_403() {
    let app = TestApp::builder().pdp(PdpMode::Deny).build().await;
    let r = get(&app, TestUser::A1, MODELS).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let r = get(&app, TestUser::A1, &format!("{MODELS}/gpt-premium")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
}

// ---------------------------------------------------------------------------------------------
// Quota status
// ---------------------------------------------------------------------------------------------

fn limits(daily: i64, monthly: i64) -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: daily,
        limit_monthly_credits_micro: monthly,
    }
}

fn today() -> (NaiveDate, NaiveDate) {
    period_starts(now_utc())
}

fn find_period<'a>(body: &'a Value, tier: &str, period: &str) -> Option<&'a Value> {
    body["tiers"]
        .as_array()?
        .iter()
        .find(|t| t["tier"] == tier)?["periods"]
        .as_array()?
        .iter()
        .find(|p| p["period"] == period)
}

#[tokio::test]
async fn quota_status_reports_tiers_periods() {
    midnight_safe(quota_status_reports_tiers_periods_body).await;
}

async fn quota_status_reports_tiers_periods_body() {
    // total: daily 100M, monthly disabled (0); premium: daily 50M, monthly 500M.
    let app = TestApp::builder()
        .limits(limits(100_000_000, 0), limits(50_000_000, 500_000_000))
        .build()
        .await;
    let u = TestUser::A1;
    let (day, month) = today();
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "total",
        PeriodType::Daily,
        day,
        80_000_000,
        0,
    )
    .await;
    // Other users' rows and other periods do not count.
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        TestUser::A2.user_id,
        "total",
        PeriodType::Daily,
        day,
        90_000_000,
        0,
    )
    .await;
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "total",
        PeriodType::Daily,
        day - Duration::days(1),
        70_000_000,
        0,
    )
    .await;

    let r = get(&app, u, QUOTA).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["warning_threshold_pct"], 80);
    let tiers = r.json["tiers"].as_array().unwrap();
    assert_eq!(
        tiers
            .iter()
            .map(|t| t["tier"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["premium", "total"]
    );

    let total_daily = find_period(&r.json, "total", "daily").expect("total daily");
    assert_eq!(total_daily["limit_credits_micro"], 100_000_000);
    assert_eq!(total_daily["used_credits_micro"], 80_000_000);
    assert_eq!(total_daily["remaining_credits_micro"], 20_000_000);
    assert_eq!(total_daily["remaining_percentage"], 20);
    assert_eq!(total_daily["warning"], true);
    assert_eq!(total_daily["exhausted"], false);
    let next: DateTime<Utc> = total_daily["next_reset"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        next,
        (day + Duration::days(1))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    );

    // Limit 0 -> period omitted.
    assert!(
        find_period(&r.json, "total", "monthly").is_none(),
        "{}",
        r.json
    );
    assert_eq!(tiers[1]["periods"].as_array().unwrap().len(), 1);

    // Premium: no rows -> nothing used.
    let prem_daily = find_period(&r.json, "premium", "daily").unwrap();
    assert_eq!(prem_daily["used_credits_micro"], 0);
    assert_eq!(prem_daily["remaining_credits_micro"], 50_000_000);
    assert_eq!(prem_daily["remaining_percentage"], 100);
    assert_eq!(prem_daily["warning"], false);
    assert_eq!(prem_daily["exhausted"], false);
    let prem_monthly = find_period(&r.json, "premium", "monthly").unwrap();
    assert_eq!(prem_monthly["limit_credits_micro"], 500_000_000);
    assert_eq!(prem_monthly["remaining_percentage"], 100);
    let first_of_next = if month.format("%m").to_string() == "12" {
        NaiveDate::from_ymd_opt(
            month.format("%Y").to_string().parse::<i32>().unwrap() + 1,
            1,
            1,
        )
    } else {
        month.checked_add_months(chrono::Months::new(1))
    }
    .unwrap();
    let next: DateTime<Utc> = prem_monthly["next_reset"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(next, first_of_next.and_hms_opt(0, 0, 0).unwrap().and_utc());

    // Each period has exactly the documented keys.
    let keys: std::collections::BTreeSet<_> = prem_daily
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "period",
            "limit_credits_micro",
            "used_credits_micro",
            "remaining_credits_micro",
            "remaining_percentage",
            "next_reset",
            "warning",
            "exhausted",
        ])
    );
}

#[tokio::test]
async fn quota_status_counts_reserved_and_monthly_rows() {
    midnight_safe(quota_status_counts_reserved_and_monthly_rows_body).await;
}

async fn quota_status_counts_reserved_and_monthly_rows_body() {
    let app = TestApp::builder()
        .limits(
            limits(100_000_000, 1_000_000_000),
            limits(50_000_000, 500_000_000),
        )
        .build()
        .await;
    let u = TestUser::A1;
    let (day, month) = today();
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "tier:premium",
        PeriodType::Daily,
        day,
        10_000_000,
        15_000_000,
    )
    .await;
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "tier:premium",
        PeriodType::Monthly,
        month,
        500_000_000,
        0,
    )
    .await;

    let r = get(&app, u, QUOTA).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let daily = find_period(&r.json, "premium", "daily").unwrap();
    assert_eq!(daily["used_credits_micro"], 25_000_000);
    assert_eq!(daily["remaining_credits_micro"], 25_000_000);
    assert_eq!(daily["remaining_percentage"], 50);
    assert_eq!(daily["warning"], false);
    let monthly = find_period(&r.json, "premium", "monthly").unwrap();
    assert_eq!(monthly["used_credits_micro"], 500_000_000);
    assert_eq!(monthly["remaining_credits_micro"], 0);
    assert_eq!(monthly["remaining_percentage"], 0);
    assert_eq!(monthly["warning"], true);
    assert_eq!(monthly["exhausted"], true);
    // The total bucket is untouched by premium rows.
    assert_eq!(
        find_period(&r.json, "total", "daily").unwrap()["used_credits_micro"],
        0
    );
}

#[tokio::test]
async fn quota_status_exhausted_below_one_percent() {
    midnight_safe(quota_status_exhausted_below_one_percent_body).await;
}

async fn quota_status_exhausted_below_one_percent_body() {
    let app = TestApp::builder()
        .limits(
            limits(100_000_000, 1_000_000_000),
            limits(50_000_000, 500_000_000),
        )
        .build()
        .await;
    let u = TestUser::A1;
    let (day, _) = today();
    // 0.5% remains.
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "total",
        PeriodType::Daily,
        day,
        99_500_000,
        0,
    )
    .await;
    let r = get(&app, u, QUOTA).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let p = find_period(&r.json, "total", "daily").unwrap();
    assert_eq!(p["remaining_credits_micro"], 500_000);
    assert_eq!(p["remaining_percentage"], 0);
    assert_eq!(p["exhausted"], true);
    assert_eq!(p["warning"], true);

    // Overspend is clamped: nothing remains, never negative.
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "tier:premium",
        PeriodType::Daily,
        day,
        60_000_000,
        0,
    )
    .await;
    let r = get(&app, u, QUOTA).await;
    let p = find_period(&r.json, "premium", "daily").unwrap();
    assert_eq!(p["used_credits_micro"], 60_000_000);
    assert_eq!(p["remaining_credits_micro"], 0);
    assert_eq!(p["remaining_percentage"], 0);
    assert_eq!(p["exhausted"], true);
}

#[tokio::test]
async fn quota_status_threshold_follows_config_and_authz() {
    midnight_safe(quota_status_threshold_follows_config_and_authz_body).await;
}

async fn quota_status_threshold_follows_config_and_authz_body() {
    let app = TestApp::builder()
        .config(|c| c.quota.warning_threshold_pct = 50)
        .limits(limits(100, 1_000), limits(100, 1_000))
        .build()
        .await;
    let u = TestUser::A1;
    let (day, _) = today();
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "total",
        PeriodType::Daily,
        day,
        40,
        0,
    )
    .await;
    let r = get(&app, u, QUOTA).await;
    assert_eq!(r.json["warning_threshold_pct"], 50);
    let p = find_period(&r.json, "total", "daily").unwrap();
    assert_eq!(p["remaining_percentage"], 60);
    assert_eq!(p["warning"], false, "60% remaining > 100 - 50");
    seed::insert_quota_row(
        &app.db,
        u.tenant_id,
        u.user_id,
        "total",
        PeriodType::Monthly,
        today().1,
        500,
        0,
    )
    .await;
    let r = get(&app, u, QUOTA).await;
    let p = find_period(&r.json, "total", "monthly").unwrap();
    assert_eq!(p["remaining_percentage"], 50);
    assert_eq!(p["warning"], true, "50% remaining <= 100 - 50");

    app.pdp.set_mode(PdpMode::Deny);
    let r = get(&app, u, QUOTA).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
}

// ---------------------------------------------------------------------------------------------
// OpenAPI
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn openapi_declares_the_subset_enum_schemas() {
    use toolkit::api::{OpenApiInfo, OpenApiRegistryImpl};

    let app = app().await;
    let registry = OpenApiRegistryImpl::new();
    let _ = mini_chat::api::rest::routes::register_routes(
        axum::Router::new(),
        &registry,
        app.services.clone(),
        &app.config,
    );
    let doc =
        serde_json::to_value(registry.build_openapi(&OpenApiInfo::default()).unwrap()).unwrap();
    let schemas = &doc["components"]["schemas"];
    for (name, values) in [
        ("MessageRoleDto", json!(["user", "assistant", "system"])),
        ("AttachmentKindDto", json!(["document", "image"])),
        (
            "AttachmentStatusDto",
            json!(["pending", "uploaded", "ready", "failed"]),
        ),
        ("ReactionKindDto", json!(["like", "dislike"])),
        ("ModelTierDto", json!(["standard", "premium"])),
        ("QuotaPeriod", json!(["daily", "monthly"])),
        ("QuotaTier", json!(["premium", "total"])),
    ] {
        assert_eq!(schemas[name]["enum"], values, "{name}: {schemas}");
    }
    let message = &schemas["MiniChatMessageDto"]["properties"];
    assert_eq!(
        message["role"]["$ref"],
        "#/components/schemas/MessageRoleDto"
    );
    assert_eq!(
        schemas["SetReactionReq"]["properties"]["reaction"]["type"],
        "string"
    );
    assert_eq!(
        schemas["MiniChatReactionDto"]["properties"]["reaction"]["$ref"],
        "#/components/schemas/ReactionKindDto"
    );
}
