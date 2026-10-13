#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Repository basics: every repo round-trips a complete row (UUID BLOBs,
//! timestamps, JSON, DATE, bytes) and applies the caller's scope.

mod common;

use common::{
    attachment_row, chat_row, message_attachment_row, message_row, quota_row, reaction_row,
    tenant_scope, test_db, thread_summary_row, turn_row, vector_store_row,
};
use mini_chat::infra::db::repos::{
    AttachmentRepo, ChatRepo, MessageAttachmentRepo, MessageRepo, QuotaUsageRepo, ReactionRepo,
    ThreadSummaryRepo, TurnRepo, VectorStoreRepo,
};
use toolkit_db::secure::AccessScope;
use uuid::Uuid;

#[tokio::test]
async fn every_repo_round_trips_a_row() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);

    let mut new_chat = chat_row(tenant, user);
    new_chat.title = Some("Title".to_owned());
    let chat = ChatRepo
        .insert(&conn, &scope, new_chat.clone())
        .await
        .unwrap();
    assert_eq!(chat, new_chat);
    assert_eq!(
        ChatRepo
            .find_by_id(&conn, &scope, new_chat.id)
            .await
            .unwrap(),
        Some(new_chat.clone())
    );

    let turn = turn_row(&chat, Uuid::new_v4(), "running");
    TurnRepo.insert(&conn, &scope, turn.clone()).await.unwrap();
    assert_eq!(
        TurnRepo.find_by_id(&conn, &scope, turn.id).await.unwrap(),
        Some(turn)
    );

    let mut msg = message_row(&chat, Some(Uuid::new_v4()), "assistant");
    msg.features_used = serde_json::json!(["web_search"]);
    msg.model = Some("gpt-4.1-mini".to_owned());
    MessageRepo
        .insert(&conn, &scope, msg.clone())
        .await
        .unwrap();
    assert_eq!(
        MessageRepo.find_by_id(&conn, &scope, msg.id).await.unwrap(),
        Some(msg.clone())
    );

    let mut att = attachment_row(&chat);
    att.img_thumbnail = Some(vec![0x52, 0x49, 0x46, 0x46]);
    att.attachment_kind = "image".to_owned();
    att.secondary_provider_kind = Some("anthropic".to_owned());
    AttachmentRepo
        .insert(&conn, &scope, att.clone())
        .await
        .unwrap();
    assert_eq!(
        AttachmentRepo
            .find_by_id(&conn, &scope, att.id)
            .await
            .unwrap(),
        Some(att.clone())
    );

    let link = message_attachment_row(&msg, &att);
    MessageAttachmentRepo
        .insert(&conn, &scope, link.clone())
        .await
        .unwrap();
    assert_eq!(
        MessageAttachmentRepo
            .list_for_message(&conn, &scope, chat.id, msg.id)
            .await
            .unwrap(),
        vec![link]
    );

    let summary = thread_summary_row(&chat, &msg);
    ThreadSummaryRepo
        .insert(&conn, &scope, summary.clone())
        .await
        .unwrap();
    assert_eq!(
        ThreadSummaryRepo
            .find_by_id(&conn, &scope, summary.id)
            .await
            .unwrap(),
        Some(summary)
    );

    let store = vector_store_row(&chat);
    VectorStoreRepo
        .insert(&conn, &scope, store.clone())
        .await
        .unwrap();
    assert_eq!(
        VectorStoreRepo
            .find_by_chat(&conn, &scope, chat.id)
            .await
            .unwrap(),
        Some(store)
    );

    let quota = quota_row(tenant, user, "total");
    QuotaUsageRepo
        .insert(&conn, &scope, quota.clone())
        .await
        .unwrap();
    assert_eq!(
        QuotaUsageRepo
            .find_by_id(&conn, &scope, quota.id)
            .await
            .unwrap(),
        Some(quota)
    );

    let reaction = reaction_row(&msg, user, "like");
    ReactionRepo
        .insert(&conn, &scope, reaction.clone())
        .await
        .unwrap();
    assert_eq!(
        ReactionRepo
            .find_by_id(&conn, &scope, reaction.id)
            .await
            .unwrap(),
        Some(reaction)
    );
}

#[tokio::test]
async fn owner_scoped_rows_are_invisible_to_other_users_and_tenants() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();

    let other_user = tenant_scope(tenant, Uuid::new_v4());
    let other_tenant = tenant_scope(Uuid::new_v4(), user);
    assert_eq!(
        ChatRepo
            .find_by_id(&conn, &other_user, chat.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        ChatRepo
            .find_by_id(&conn, &other_tenant, chat.id)
            .await
            .unwrap(),
        None
    );

    // Child rows: tenant isolation holds even with a valid owner.
    let msg = MessageRepo
        .insert(&conn, &scope, message_row(&chat, None, "user"))
        .await
        .unwrap();
    assert_eq!(
        MessageRepo
            .find_by_id(&conn, &other_tenant, msg.id)
            .await
            .unwrap(),
        None
    );
    // A background job's tenant-only scope reaches the child row.
    assert!(
        MessageRepo
            .find_by_id(&conn, &AccessScope::for_tenant(tenant), msg.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn insert_outside_scope_is_denied() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);

    // A chat for another user of the same tenant.
    let foreign = chat_row(tenant, Uuid::new_v4());
    assert!(ChatRepo.insert(&conn, &scope, foreign).await.is_err());

    // A child row in another tenant.
    let chat = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let mut msg = message_row(&chat, None, "user");
    msg.tenant_id = Uuid::new_v4();
    assert!(MessageRepo.insert(&conn, &scope, msg).await.is_err());
}

#[tokio::test]
async fn message_attachment_fk_requires_same_chat() {
    let db = test_db().await;
    let conn = db.conn().unwrap();
    let (tenant, user) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = tenant_scope(tenant, user);
    let chat_a = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let chat_b = ChatRepo
        .insert(&conn, &scope, chat_row(tenant, user))
        .await
        .unwrap();
    let msg = MessageRepo
        .insert(&conn, &scope, message_row(&chat_a, None, "user"))
        .await
        .unwrap();
    let att = AttachmentRepo
        .insert(&conn, &scope, attachment_row(&chat_b))
        .await
        .unwrap();

    let link = message_attachment_row(&msg, &att); // chat_id = chat_a
    let err = MessageAttachmentRepo
        .insert(&conn, &scope, link)
        .await
        .expect_err("attachment of another chat must violate the composite FK");
    assert!(err.is_foreign_key_violation(), "unexpected error: {err:?}");
}
