#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::Duration as TimeDuration;
use time::macros::datetime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_odata::{CursorV1, ODataQuery};
use uuid::Uuid;

use super::MessageService;
use crate::api::rest::dto::MessageDto;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::ChatAction;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{attachment, chat, message, message_reaction};
use crate::test_support::{
    FakeAuthz, ctx_for, insert_message, link_attachment, seed_attachment, seed_chat, seed_message,
    test_ctx, test_provider,
};

fn svc(db: &Arc<DBProvider<DomainError>>) -> (MessageService, Arc<FakeAuthz>) {
    let authz = Arc::new(FakeAuthz::default());
    (MessageService::new(db.clone(), authz.clone()), authz)
}

fn t0() -> time::OffsetDateTime {
    datetime!(2026-10-04 12:00:00 UTC)
}

async fn react(db: &DBProvider<DomainError>, msg: &message::Model, user: Uuid, reaction: &str) {
    let conn = db.conn().unwrap();
    secure_insert::<message_reaction::Entity>(
        message_reaction::ActiveModel {
            id: Set(Uuid::new_v4()),
            message_id: Set(msg.id),
            user_id: Set(user),
            tenant_id: Set(msg.tenant_id),
            reaction: Set(reaction.to_owned()),
            created_at: Set(db_ts(t0())),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn messages_list_chronological_same_second() {
    let db = test_provider().await;
    let (svc, authz) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let mut inserted = Vec::new();
    for (i, role) in [
        "user",
        "assistant",
        "user",
        "assistant",
        "user",
        "assistant",
    ]
    .into_iter()
    .enumerate()
    {
        let at = db_ts(t0() + TimeDuration::microseconds(i64::try_from(i).unwrap()));
        inserted.push(seed_message(&db, &chat, role, at).await.id);
    }

    let first = svc
        .list(&ctx, chat.id, ODataQuery::default().with_limit(4))
        .await
        .unwrap();
    let ids: Vec<_> = first.items.iter().map(|m| m.id).collect();
    assert_eq!(ids, inserted[..4]);
    assert_eq!(first.page_info.limit, 4);

    let cursor = CursorV1::decode(&first.page_info.next_cursor.unwrap()).unwrap();
    let second = svc
        .list(
            &ctx,
            chat.id,
            ODataQuery::default().with_limit(4).with_cursor(cursor),
        )
        .await
        .unwrap();
    let ids: Vec<_> = second.items.iter().map(|m| m.id).collect();
    assert_eq!(ids, inserted[4..]);
    assert!(second.page_info.next_cursor.is_none());
    assert_eq!(authz.chat_actions()[0], ChatAction::ListMessages);
}

#[tokio::test]
async fn messages_list_default_limit_is_twenty_and_ties_break_by_id_asc() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(seed_message(&db, &chat, "user", db_ts(t0())).await.id);
    }
    ids.sort_unstable();

    let page = svc
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap();

    assert_eq!(page.page_info.limit, 20);
    assert_eq!(page.items.iter().map(|m| m.id).collect::<Vec<_>>(), ids);
}

#[tokio::test]
async fn messages_list_hides_deleted_messages_and_foreign_chats() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let kept = seed_message(&db, &chat, "user", db_ts(t0())).await;
    let gone = seed_message(&db, &chat, "assistant", db_ts(t0())).await;
    let other_chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    seed_message(&db, &other_chat, "user", db_ts(t0())).await;
    let conn = db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(
            message::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(db_ts(t0()))),
        )
        .filter(message::Column::Id.eq(gone.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let page = svc
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap();
    assert_eq!(
        page.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        [kept.id]
    );

    let chat_not_found = DomainError::NotFound {
        resource: ResourceKind::Chat,
    };
    let stranger = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    assert_eq!(
        svc.list(&stranger, chat.id, ODataQuery::default())
            .await
            .unwrap_err(),
        chat_not_found
    );
    chat::Entity::update_many()
        .col_expr(
            chat::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(db_ts(t0()))),
        )
        .filter(chat::Column::Id.eq(chat.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(
        svc.list(&ctx, chat.id, ODataQuery::default())
            .await
            .unwrap_err(),
        chat_not_found
    );
}

#[tokio::test]
async fn messages_filter_by_role() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    seed_message(&db, &chat, "user", db_ts(t0())).await;
    let a = seed_message(
        &db,
        &chat,
        "assistant",
        db_ts(t0() + TimeDuration::seconds(1)),
    )
    .await;
    let expr = toolkit_odata::parse_filter_string("role eq 'assistant'")
        .unwrap()
        .into_expr();

    let page = svc
        .list(&ctx, chat.id, ODataQuery::default().with_filter(expr))
        .await
        .unwrap();

    assert_eq!(page.items.iter().map(|m| m.id).collect::<Vec<_>>(), [a.id]);
}

#[tokio::test]
async fn message_dto_always_has_attachments_and_my_reaction() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let user_msg = seed_message(&db, &chat, "user", db_ts(t0())).await;
    let mut assistant = message::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(chat.tenant_id),
        chat_id: Set(chat.id),
        request_id: Set(user_msg.request_id),
        role: Set("assistant".to_owned()),
        content: Set("answer".to_owned()),
        content_type: Set("text".to_owned()),
        token_estimate: Set(1),
        provider_response_id: Set(Some("resp_secret".to_owned())),
        request_kind: Set("chat".to_owned()),
        features_used: Set(serde_json::json!([])),
        input_tokens: Set(12),
        output_tokens: Set(5),
        cache_read_input_tokens: Set(0),
        cache_write_input_tokens: Set(0),
        reasoning_tokens: Set(0),
        model: Set(Some("b".to_owned())),
        is_compressed: Set(false),
        created_at: Set(db_ts(t0() + TimeDuration::seconds(1))),
        deleted_at: Set(None),
    };
    let assistant_msg = insert_message(&db, assistant.clone()).await;
    assistant.id = Set(Uuid::new_v4());
    assistant.request_id = Set(Some(Uuid::new_v4()));
    assistant.input_tokens = Set(0);
    assistant.output_tokens = Set(0);
    assistant.created_at = Set(db_ts(t0() + TimeDuration::seconds(2)));
    let unrated = insert_message(&db, assistant).await;

    let thumb = vec![1_u8, 2, 3];
    let image = seed_attachment(&db, &chat, "image", "ready", Some(thumb.clone())).await;
    let pending_image = seed_attachment(&db, &chat, "image", "pending", Some(thumb)).await;
    let doc = seed_attachment(&db, &chat, "document", "ready", None).await;
    let deleted_doc = seed_attachment(&db, &chat, "document", "ready", None).await;
    for a in [&image, &pending_image, &doc, &deleted_doc] {
        link_attachment(&db, &user_msg, a.id).await;
    }
    let conn = db.conn().unwrap();
    attachment::Entity::update_many()
        .col_expr(
            attachment::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(db_ts(t0()))),
        )
        .filter(attachment::Column::Id.eq(deleted_doc.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    react(&db, &assistant_msg, ctx.subject_id(), "like").await;
    react(&db, &unrated, Uuid::new_v4(), "dislike").await; // someone else's

    let page = svc
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap();
    let json: Vec<serde_json::Value> = page
        .items
        .into_iter()
        .map(|m| serde_json::to_value(MessageDto::from(m)).unwrap())
        .collect();

    assert_eq!(json.len(), 3);
    let user = &json[0];
    assert_eq!(user["id"], user_msg.id.to_string());
    assert_eq!(user["request_id"], user_msg.request_id.unwrap().to_string());
    assert_eq!(user["role"], "user");
    assert_eq!(user["content"], "user message");
    assert!(user["my_reaction"].is_null());
    assert!(user.as_object().unwrap().contains_key("my_reaction"));
    for absent in ["model", "input_tokens", "output_tokens"] {
        assert!(user.get(absent).is_none(), "{absent} in {user}");
    }
    let attachments = user["attachments"].as_array().unwrap();
    let by_id = |id: Uuid| {
        attachments
            .iter()
            .find(|a| a["attachment_id"] == id.to_string())
            .cloned()
    };
    assert_eq!(attachments.len(), 3, "{attachments:?}");
    assert!(by_id(deleted_doc.id).is_none());
    let img = by_id(image.id).unwrap();
    assert_eq!(img["kind"], "image");
    assert_eq!(img["status"], "ready");
    assert_eq!(img["filename"], "image.bin");
    assert_eq!(
        img["img_thumbnail"],
        serde_json::json!({
            "content_type": "image/webp",
            "width": 64,
            "height": 32,
            "data_base64": "AQID"
        })
    );
    assert!(
        by_id(pending_image.id)
            .unwrap()
            .get("img_thumbnail")
            .is_none()
    );
    let d = by_id(doc.id).unwrap();
    assert_eq!(d["kind"], "document");
    assert!(d.get("img_thumbnail").is_none());

    let rated = &json[1];
    assert_eq!(rated["my_reaction"], "like");
    assert_eq!(rated["model"], "b");
    assert_eq!(rated["input_tokens"], 12);
    assert_eq!(rated["output_tokens"], 5);
    assert_eq!(rated["attachments"], serde_json::json!([]));

    let other = &json[2];
    assert!(other["my_reaction"].is_null());
    assert!(other.get("input_tokens").is_none());
    assert_eq!(other["attachments"], serde_json::json!([]));

    let text = serde_json::to_string(&json).unwrap();
    for leaked in ["resp_secret", "file-secret"] {
        assert!(!text.contains(leaked), "{leaked} leaked");
    }
}

#[tokio::test]
async fn message_without_request_id_is_internal() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let msg = seed_message(&db, &chat, "user", db_ts(t0())).await;
    let conn = db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(
            message::Column::RequestId,
            sea_orm::sea_query::Expr::value(Option::<Uuid>::None),
        )
        .filter(message::Column::Id.eq(msg.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    let still = message::Entity::find()
        .filter(message::Column::Id.eq(msg.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap();
    assert!(still.request_id.is_none());

    let err = svc
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn my_reaction_is_null_for_non_assistant_messages() {
    let db = test_provider().await;
    let (svc, _) = svc(&db);
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, db_ts(t0())).await;
    let user_msg = seed_message(&db, &chat, "user", db_ts(t0())).await;
    let system_msg =
        seed_message(&db, &chat, "system", db_ts(t0() + TimeDuration::seconds(1))).await;
    // Stray rows (reactions are only allowed on assistant messages).
    react(&db, &user_msg, ctx.subject_id(), "like").await;
    react(&db, &system_msg, ctx.subject_id(), "dislike").await;

    let page = svc
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap();

    assert_eq!(page.items.len(), 2);
    assert!(
        page.items.iter().all(|m| m.my_reaction.is_none()),
        "{:?}",
        page.items
    );
}
