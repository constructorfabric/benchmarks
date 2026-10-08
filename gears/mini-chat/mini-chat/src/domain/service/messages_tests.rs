#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::ActiveValue;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::api::rest::dto::{
    AttachmentKindDto, AttachmentStatusDto, MessageRoleDto, ReactionKindDto,
};
use crate::domain::error::resource_types;
use crate::domain::service::chats::test_rows::{
    create_chat, enc, env_with_pdp, insert_attachment, insert_message, insert_turn_messages,
    link_attachment, message_am, odata, problem,
};
use crate::domain::service::test_support::{
    DenyPdp, FailingPdp, TENANT_A, TENANT_B, TestEnv, USER_A1, USER_A2, USER_B, ctx, ctx_a1,
};

async fn list(
    env: &TestEnv,
    chat_id: Uuid,
    qs: &str,
) -> toolkit_odata::Page<crate::api::rest::dto::MiniChatMessageDto> {
    env.services
        .messages
        .list(&ctx_a1(), chat_id, odata(qs).await.unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn lists_messages_chronologically_with_contract_fields() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let t0 = OffsetDateTime::now_utc();
    let (u1, a1) = insert_turn_messages(&env, TENANT_A, chat, t0).await;
    let (u2, a2) = insert_turn_messages(&env, TENANT_A, chat, t0 + Duration::seconds(1)).await;
    let mut deleted = message_am(
        TENANT_A,
        chat,
        "user",
        Some(Uuid::new_v4()),
        t0 + Duration::seconds(2),
    );
    deleted.deleted_at = ActiveValue::Set(Some(t0));
    insert_message(&env, deleted).await;

    let page = list(&env, chat, "").await;
    let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
    assert_eq!(ids, vec![u1.id, a1.id, u2.id, a2.id]);
    let roles: Vec<MessageRoleDto> = page.items.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRoleDto::User,
            MessageRoleDto::Assistant,
            MessageRoleDto::User,
            MessageRoleDto::Assistant
        ]
    );
    assert_eq!(page.items[0].request_id, page.items[1].request_id);
    assert_eq!(page.items[0].request_id, u1.request_id.unwrap());
    assert!(page.items.iter().all(|m| m.attachments.is_empty()));
    assert!(page.items.iter().all(|m| m.my_reaction.is_none()));
    assert_eq!(page.items[0].model, None);
    assert_eq!(page.items[0].input_tokens, None);
    assert_eq!(page.items[0].output_tokens, None);
    assert_eq!(page.items[1].model.as_deref(), Some("gpt-premium"));
    assert_eq!(page.items[1].input_tokens, Some(100));
    assert_eq!(page.items[1].output_tokens, Some(50));

    let json = serde_json::to_value(&page.items[0]).unwrap();
    assert_eq!(json["attachments"], serde_json::json!([]));
    assert!(json.as_object().unwrap().contains_key("my_reaction"));
    assert!(json["my_reaction"].is_null());
    assert!(json.get("model").is_none());
    assert!(json.get("input_tokens").is_none());
    assert!(json.get("output_tokens").is_none());
    assert_eq!(json["role"], "user");
    env.shutdown().await;
}

#[tokio::test]
async fn same_timestamp_uses_id_tiebreaker_and_paginates() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let t = OffsetDateTime::now_utc();
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            insert_message(
                &env,
                message_am(TENANT_A, chat, "user", Some(Uuid::new_v4()), t),
            )
            .await
            .id,
        );
    }
    // Two more at a later time.
    for i in 1..=2 {
        ids.push(
            insert_message(
                &env,
                message_am(
                    TENANT_A,
                    chat,
                    "user",
                    Some(Uuid::new_v4()),
                    t + Duration::milliseconds(i),
                ),
            )
            .await
            .id,
        );
    }
    let mut expected = ids[..5].to_vec();
    expected.sort();
    expected.extend_from_slice(&ids[5..]);

    for limit in [1_u64, 2, 3] {
        let mut seen = Vec::new();
        let mut qs = format!("limit={limit}");
        loop {
            let page = list(&env, chat, &qs).await;
            seen.extend(page.items.iter().map(|m| m.id));
            match page.page_info.next_cursor {
                Some(c) => qs = format!("limit={limit}&cursor={c}"),
                None => break,
            }
        }
        assert_eq!(seen, expected, "limit={limit}");
    }
    env.shutdown().await;
}

#[tokio::test]
async fn filters_and_orderby() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let t0 = OffsetDateTime::now_utc();
    let (u1, a1) = insert_turn_messages(&env, TENANT_A, chat, t0).await;
    let (u2, a2) = insert_turn_messages(&env, TENANT_A, chat, t0 + Duration::seconds(1)).await;

    let page = list(
        &env,
        chat,
        &format!("{}={}", enc("$filter"), enc(&format!("id eq '{}'", a1.id))),
    )
    .await;
    assert_eq!(
        page.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![a1.id]
    );

    let page = list(
        &env,
        chat,
        &format!("{}={}", enc("$filter"), enc("role eq 'assistant'")),
    )
    .await;
    assert_eq!(
        page.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![a1.id, a2.id]
    );

    let page = list(
        &env,
        chat,
        &format!("{}={}", enc("$orderby"), enc("created_at desc")),
    )
    .await;
    assert_eq!(
        page.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![a2.id, u2.id, a1.id, u1.id]
    );

    let ts = (t0 + Duration::milliseconds(500))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let page = list(
        &env,
        chat,
        &format!("{}={}", enc("$filter"), enc(&format!("created_at ge {ts}"))),
    )
    .await;
    assert_eq!(
        page.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![u2.id, a2.id]
    );

    // Bad filter / orderby field -> OData 400.
    for qs in [
        format!("{}={}", enc("$filter"), enc("content eq 'x'")),
        format!("{}={}", enc("$orderby"), enc("content asc")),
    ] {
        let e = env
            .services
            .messages
            .list(&ctx_a1(), chat, odata(&qs).await.unwrap())
            .await
            .unwrap_err();
        let p = problem(e);
        assert_eq!(p["status"], 400, "{p}");
        assert_eq!(p["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    }
    let page = list(&env, chat, "limit=500").await;
    assert_eq!(page.page_info.limit, 100);
    env.shutdown().await;
}

#[tokio::test]
async fn null_request_id_is_internal_error() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    insert_message(
        &env,
        message_am(TENANT_A, chat, "system", None, OffsetDateTime::now_utc()),
    )
    .await;
    let e = env
        .services
        .messages
        .list(&ctx_a1(), chat, odata("").await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(problem(e)["status"], 500);
    env.shutdown().await;
}

#[tokio::test]
async fn attachments_are_embedded() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (u1, a1) = insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    let doc = insert_attachment(&env, TENANT_A, chat, "document", "ready", None, false).await;
    let img = insert_attachment(
        &env,
        TENANT_A,
        chat,
        "image",
        "ready",
        Some(vec![1, 2, 3]),
        false,
    )
    .await;
    let img_pending = insert_attachment(
        &env,
        TENANT_A,
        chat,
        "image",
        "pending",
        Some(vec![9]),
        false,
    )
    .await;
    let img_nothumb = insert_attachment(&env, TENANT_A, chat, "image", "ready", None, false).await;
    let gone = insert_attachment(&env, TENANT_A, chat, "document", "ready", None, true).await;
    for a in [&doc, &img, &img_pending, &img_nothumb, &gone] {
        link_attachment(&env, TENANT_A, chat, u1.id, a.id).await;
    }

    let page = list(&env, chat, "").await;
    let user_msg = page.items.iter().find(|m| m.id == u1.id).unwrap();
    let asst = page.items.iter().find(|m| m.id == a1.id).unwrap();
    assert!(asst.attachments.is_empty());
    assert_eq!(
        user_msg.attachments.len(),
        4,
        "deleted attachment is not listed"
    );
    assert!(
        user_msg
            .attachments
            .iter()
            .all(|a| a.attachment_id != gone.id)
    );

    let by_id = |id: Uuid| {
        user_msg
            .attachments
            .iter()
            .find(|a| a.attachment_id == id)
            .unwrap()
    };
    let d = by_id(doc.id);
    assert_eq!(d.kind, AttachmentKindDto::Document);
    assert_eq!(d.status, AttachmentStatusDto::Ready);
    assert_eq!(d.filename, "document.bin");
    assert!(d.img_thumbnail.is_none());
    let i = by_id(img.id);
    assert_eq!(i.kind, AttachmentKindDto::Image);
    let thumb = i.img_thumbnail.as_ref().expect("thumbnail");
    assert_eq!(thumb.content_type, "image/webp");
    assert_eq!(thumb.width, 64);
    assert_eq!(thumb.height, 32);
    assert_eq!(thumb.data_base64, "AQID");
    let p = by_id(img_pending.id);
    assert_eq!(p.status, AttachmentStatusDto::Pending);
    assert!(p.img_thumbnail.is_none());
    assert!(by_id(img_nothumb.id).img_thumbnail.is_none());

    let json = serde_json::to_value(d).unwrap();
    assert!(json.get("img_thumbnail").is_none());
    env.shutdown().await;
}

#[tokio::test]
async fn my_reaction_reflects_caller_reaction_only_on_assistant() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    let (u1, a1) = insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    env.services
        .reactions
        .set(&ctx_a1(), chat, a1.id, "dislike")
        .await
        .unwrap();
    let page = list(&env, chat, "").await;
    let asst = page.items.iter().find(|m| m.id == a1.id).unwrap();
    assert_eq!(asst.my_reaction, Some(ReactionKindDto::Dislike));
    let user = page.items.iter().find(|m| m.id == u1.id).unwrap();
    assert_eq!(user.my_reaction, None);
    env.shutdown().await;
}

#[tokio::test]
async fn isolation_and_pep() {
    let env = TestEnv::default_env().await;
    let chat = create_chat(&env, USER_A1, TENANT_A, None).await;
    insert_turn_messages(&env, TENANT_A, chat, OffsetDateTime::now_utc()).await;
    for (user, tenant) in [(USER_A2, TENANT_A), (USER_B, TENANT_B)] {
        let e = env
            .services
            .messages
            .list(&ctx(user, tenant), chat, odata("").await.unwrap())
            .await
            .unwrap_err();
        let p = problem(e);
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], resource_types::CHAT);
    }
    // Deleted chat -> 404.
    env.services.chats.delete(&ctx_a1(), chat).await.unwrap();
    let e = env
        .services
        .messages
        .list(&ctx_a1(), chat, odata("").await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(problem(e)["status"], 404);
    env.shutdown().await;

    let env = env_with_pdp(Arc::new(DenyPdp)).await;
    let e = env
        .services
        .messages
        .list(&ctx_a1(), Uuid::new_v4(), odata("").await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(problem(e)["status"], 403);
    env.shutdown().await;

    let env = env_with_pdp(Arc::new(FailingPdp)).await;
    let e = env
        .services
        .messages
        .list(&ctx_a1(), Uuid::new_v4(), odata("").await.unwrap())
        .await
        .unwrap_err();
    assert_eq!(problem(e)["status"], 503);
    env.shutdown().await;
}
