#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::macros::datetime;
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, SecureUpdateExt};
use toolkit_odata::ODataQuery;
use uuid::Uuid;

use super::ReactionService;
use crate::domain::enums::ReactionKind;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::ports::ChatAction;
use crate::domain::services::message_service::MessageService;
use crate::domain::time::db_ts;
use crate::infra::db::entities::{message, message_reaction};
use crate::test_support::{FakeAuthz, ctx_for, seed_chat, seed_message, test_ctx, test_provider};

const MESSAGE_NOT_FOUND: DomainError = DomainError::NotFound {
    resource: ResourceKind::Message,
};

fn t0() -> time::OffsetDateTime {
    db_ts(datetime!(2026-10-04 12:00:00 UTC))
}

async fn reactions_of(db: &DBProvider<DomainError>, msg: Uuid) -> Vec<message_reaction::Model> {
    let conn = db.conn().unwrap();
    message_reaction::Entity::find()
        .filter(message_reaction::Column::MessageId.eq(msg))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn reaction_upsert_replaces() {
    let db = test_provider().await;
    let authz = Arc::new(FakeAuthz::default());
    let svc = ReactionService::new(db.clone(), authz.clone());
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let msg = seed_message(&db, &chat, "assistant", t0()).await;

    let first = svc.set(&ctx, chat.id, msg.id, "like").await.unwrap();
    assert_eq!(first.message_id, msg.id);
    assert_eq!(first.reaction, ReactionKind::Like);
    let second = svc.set(&ctx, chat.id, msg.id, "dislike").await.unwrap();
    assert_eq!(second.reaction, ReactionKind::Dislike);
    assert!(second.created_at >= first.created_at);

    let rows = reactions_of(&db, msg.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].user_id, ctx.subject_id());

    let listed = MessageService::new(db.clone(), authz.clone())
        .list(&ctx, chat.id, ODataQuery::default())
        .await
        .unwrap();
    assert_eq!(listed.items[0].my_reaction, Some(ReactionKind::Dislike));
    assert_eq!(
        authz.chat_actions()[..2],
        [ChatAction::SetReaction, ChatAction::SetReaction]
    );
}

#[tokio::test]
async fn reaction_remove_is_idempotent_and_only_touches_the_caller() {
    let db = test_provider().await;
    let authz = Arc::new(FakeAuthz::default());
    let svc = ReactionService::new(db.clone(), authz.clone());
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let msg = seed_message(&db, &chat, "assistant", t0()).await;
    svc.set(&ctx, chat.id, msg.id, "like").await.unwrap();
    // Another user's reaction on the same message (inserted directly).
    let other = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    {
        use sea_orm::ActiveValue::Set;
        let conn = db.conn().unwrap();
        toolkit_db::secure::secure_insert::<message_reaction::Entity>(
            message_reaction::ActiveModel {
                id: Set(Uuid::new_v4()),
                message_id: Set(msg.id),
                user_id: Set(other.subject_id()),
                tenant_id: Set(msg.tenant_id),
                reaction: Set("dislike".to_owned()),
                created_at: Set(t0()),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    svc.remove(&ctx, chat.id, msg.id).await.unwrap();
    svc.remove(&ctx, chat.id, msg.id).await.unwrap();

    let rows = reactions_of(&db, msg.id).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, other.subject_id());
    assert_eq!(
        authz.chat_actions().last(),
        Some(&ChatAction::DeleteReaction)
    );
}

#[tokio::test]
async fn reaction_on_user_message_is_failed_precondition() {
    let db = test_provider().await;
    let svc = ReactionService::new(db.clone(), Arc::new(FakeAuthz::default()));
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    for role in ["user", "system"] {
        let msg = seed_message(&db, &chat, role, t0()).await;
        assert_eq!(
            svc.set(&ctx, chat.id, msg.id, "like").await.unwrap_err(),
            DomainError::ReactionTargetNotAssistant
        );
        assert_eq!(
            svc.remove(&ctx, chat.id, msg.id).await.unwrap_err(),
            DomainError::ReactionTargetNotAssistant
        );
        assert!(reactions_of(&db, msg.id).await.is_empty());
    }
}

#[tokio::test]
async fn reaction_invalid_value_checked_before_authz() {
    let db = test_provider().await;
    let authz = Arc::new(FakeAuthz::denying());
    let svc = ReactionService::new(db.clone(), authz.clone());
    let ctx = test_ctx();

    for bad in ["love", "LIKE", "", " like"] {
        assert_eq!(
            svc.set(&ctx, Uuid::new_v4(), Uuid::new_v4(), bad)
                .await
                .unwrap_err(),
            DomainError::InvalidReaction,
            "{bad:?}"
        );
    }
    assert!(authz.chat_actions().is_empty());
    assert_eq!(
        svc.set(&ctx, Uuid::new_v4(), Uuid::new_v4(), "like")
            .await
            .unwrap_err(),
        DomainError::AuthzDenied
    );
}

#[tokio::test]
async fn reaction_on_missing_deleted_or_foreign_message_is_not_found() {
    let db = test_provider().await;
    let svc = ReactionService::new(db.clone(), Arc::new(FakeAuthz::default()));
    let ctx = test_ctx();
    let chat = seed_chat(&db, &ctx, None, t0()).await;
    let other_chat = seed_chat(&db, &ctx, None, t0()).await;
    let elsewhere = seed_message(&db, &other_chat, "assistant", t0()).await;
    let deleted = seed_message(&db, &chat, "assistant", t0()).await;
    let conn = db.conn().unwrap();
    message::Entity::update_many()
        .col_expr(
            message::Column::DeletedAt,
            sea_orm::sea_query::Expr::value(Some(t0())),
        )
        .filter(message::Column::Id.eq(deleted.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    for msg_id in [Uuid::new_v4(), elsewhere.id, deleted.id] {
        assert_eq!(
            svc.set(&ctx, chat.id, msg_id, "like").await.unwrap_err(),
            MESSAGE_NOT_FOUND
        );
        assert_eq!(
            svc.remove(&ctx, chat.id, msg_id).await.unwrap_err(),
            MESSAGE_NOT_FOUND
        );
    }

    let stranger = ctx_for(ctx.subject_tenant_id(), Uuid::new_v4());
    let live = seed_message(&db, &chat, "assistant", t0()).await;
    assert_eq!(
        svc.set(&stranger, chat.id, live.id, "like")
            .await
            .unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    );
}
