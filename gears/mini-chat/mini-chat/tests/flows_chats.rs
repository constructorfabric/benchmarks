//! Chat CRUD, listing (`OData`), activity ordering, isolation, model
//! immutability and messages listing.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]

mod common;

use common::*;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, SortDir};

fn send(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: None,
        attachment_ids: Vec::new(),
        web_search: false,
    }
}

#[tokio::test]
async fn crud_lifecycle_and_validation() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    // defaults: first enabled is_default model, no title
    let v = h.svc.create_chat(&a, None, None).await.unwrap();
    assert_eq!(v.chat.model, "prem");
    assert_eq!(v.chat.title, None);
    assert_eq!(v.message_count, 0);
    // title trimmed, explicit model
    let v = h
        .svc
        .create_chat(&a, Some("  x  ".into()), Some("std".into()))
        .await
        .unwrap();
    assert_eq!(v.chat.title.as_deref(), Some("x"));
    assert_eq!(v.chat.model, "std");
    // validation: title before model
    for t in ["", "   ", &"y".repeat(256)] {
        let e = h
            .svc
            .create_chat(&a, Some(t.to_owned()), Some("nope".into()))
            .await
            .unwrap_err();
        assert!(matches!(e, DomainError::InvalidTitle), "{t}");
    }
    for m in ["nope", "off"] {
        let e = h
            .svc
            .create_chat(&a, None, Some(m.into()))
            .await
            .unwrap_err();
        assert!(matches!(e, DomainError::InvalidModel { .. }));
    }
    // update title only
    let before = v.chat.updated_at;
    let u = h
        .svc
        .update_chat_title(&a, v.chat.id, " Renamed ")
        .await
        .unwrap();
    assert_eq!(u.chat.title.as_deref(), Some("Renamed"));
    assert_eq!(u.chat.model, "std");
    assert!(u.chat.updated_at > before);
    assert!(matches!(
        h.svc
            .update_chat_title(&a, v.chat.id, "  ")
            .await
            .unwrap_err(),
        DomainError::InvalidTitle
    ));
    // delete: soft delete, then 404
    h.svc.delete_chat(&a, v.chat.id).await.unwrap();
    assert!(matches!(
        h.svc.get_chat(&a, v.chat.id).await.unwrap_err(),
        DomainError::ChatNotFound { .. }
    ));
    assert!(matches!(
        h.svc.delete_chat(&a, v.chat.id).await.unwrap_err(),
        DomainError::ChatNotFound { .. }
    ));
    let rows = h
        .rows(&format!(
            "select deleted_at from chats where hex(id) = upper('{}')",
            uhex(v.chat.id)
        ))
        .await;
    assert!(rows[0]["deleted_at"].is_string());
    h.stop().await;
}

#[tokio::test]
async fn owner_and_tenant_isolation() {
    let h = Harness::new().await;
    let a1 = ctx(TENANT_A, USER_A1);
    let a2 = ctx(TENANT_A, USER_A2);
    let b1 = ctx(TENANT_B, USER_B1);
    let chat = h
        .svc
        .create_chat(&a1, Some("mine".into()), None)
        .await
        .unwrap()
        .chat;
    for other in [&a2, &b1] {
        assert!(matches!(
            h.svc.get_chat(other, chat.id).await.unwrap_err(),
            DomainError::ChatNotFound { .. }
        ));
        assert!(matches!(
            h.svc
                .update_chat_title(other, chat.id, "x")
                .await
                .unwrap_err(),
            DomainError::ChatNotFound { .. }
        ));
        assert!(matches!(
            h.svc.delete_chat(other, chat.id).await.unwrap_err(),
            DomainError::ChatNotFound { .. }
        ));
        assert!(
            h.svc
                .list_messages(other, chat.id, &ODataQuery::default())
                .await
                .is_err()
        );
        assert!(h.svc.send_message(other, chat.id, send("x")).await.is_err());
        let page = h
            .svc
            .list_chats(other, &ODataQuery::default())
            .await
            .unwrap();
        assert!(page.items.iter().all(|v| v.chat.id != chat.id));
    }
    assert!(h.svc.get_chat(&a1, chat.id).await.is_ok());
    h.stop().await;
}

#[tokio::test]
async fn list_orders_by_activity_and_paginates() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(
            h.svc
                .create_chat(&a, Some(format!("c{i}")), Some("std".into()))
                .await
                .unwrap()
                .chat
                .id,
        );
    }
    let page = h.svc.list_chats(&a, &ODataQuery::default()).await.unwrap();
    let listed: Vec<_> = page.items.iter().map(|v| v.chat.id).collect();
    let mut expected = ids.clone();
    expected.reverse();
    assert_eq!(listed, expected);
    // a sent message moves the chat to the top
    let start = h.svc.send_message(&a, ids[0], send("bump")).await.unwrap();
    let ev = Harness::collect(start).await;
    assert!(matches!(ev.last(), Some(StreamEvent::Done(_))));
    let page = h.svc.list_chats(&a, &ODataQuery::default()).await.unwrap();
    assert_eq!(page.items[0].chat.id, ids[0]);
    assert_eq!(page.items[0].message_count, 2);
    // pagination with limit and cursor
    let q = ODataQuery::default().with_limit(2);
    let p1 = h.svc.list_chats(&a, &q).await.unwrap();
    assert_eq!(p1.items.len(), 2);
    let cursor = p1.page_info.next_cursor.clone().unwrap();
    let c = toolkit_odata::CursorV1::decode(&cursor).unwrap();
    let mut q2 = ODataQuery::default().with_limit(2).with_cursor(c);
    q2 = q2.with_order(ODataOrderBy::empty());
    let p2 = h.svc.list_chats(&a, &q2).await.unwrap();
    assert_eq!(p2.items.len(), 2);
    assert!(
        p1.items
            .iter()
            .all(|x| p2.items.iter().all(|y| x.chat.id != y.chat.id))
    );
    // explicit order
    let q = ODataQuery::default().with_order(ODataOrderBy(vec![OrderKey {
        field: "title".into(),
        dir: SortDir::Asc,
    }]));
    let titles: Vec<_> = h
        .svc
        .list_chats(&a, &q)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|v| v.chat.title.unwrap())
        .collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted);
    h.stop().await;
}

#[tokio::test]
async fn messages_listing_contract() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    for i in 0..2 {
        Harness::collect(
            h.svc
                .send_message(&a, chat.id, send(&format!("q{i}")))
                .await
                .unwrap(),
        )
        .await;
    }
    let page = h
        .svc
        .list_messages(&a, chat.id, &ODataQuery::default())
        .await
        .unwrap();
    let roles: Vec<_> = page.items.iter().map(|m| m.message.role.as_str()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert!(
        page.items
            .windows(2)
            .all(|w| w[0].message.created_at < w[1].message.created_at)
    );
    assert_eq!(page.items[0].request_id, page.items[1].request_id);
    assert!(
        page.items
            .iter()
            .all(|m| m.attachments.is_empty() && m.my_reaction.is_none())
    );
    assert_eq!(page.items[1].message.model.as_deref(), Some("std"));
    assert_eq!(page.items[1].message.input_tokens, 11);
    // reactions on assistant messages only
    let asst = page.items[1].message.id;
    let user_msg = page.items[0].message.id;
    let r = h.svc.set_reaction(&a, chat.id, asst, "like").await.unwrap();
    assert_eq!(r.reaction, "like");
    h.svc
        .set_reaction(&a, chat.id, asst, "dislike")
        .await
        .unwrap();
    let page = h
        .svc
        .list_messages(&a, chat.id, &ODataQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items[1].my_reaction.as_deref(), Some("dislike"));
    assert!(matches!(
        h.svc
            .set_reaction(&a, chat.id, user_msg, "like")
            .await
            .unwrap_err(),
        DomainError::ReactionTargetNotAssistant
    ));
    assert!(matches!(
        h.svc
            .set_reaction(&a, chat.id, asst, "love")
            .await
            .unwrap_err(),
        DomainError::InvalidReaction
    ));
    assert!(matches!(
        h.svc
            .delete_reaction(&a, chat.id, user_msg)
            .await
            .unwrap_err(),
        DomainError::ReactionTargetNotAssistant
    ));
    h.svc.delete_reaction(&a, chat.id, asst).await.unwrap();
    h.svc.delete_reaction(&a, chat.id, asst).await.unwrap();
    let reactions = h.rows("select count(*) n from message_reactions").await;
    assert_eq!(reactions[0]["n"], 0);
    // another user cannot react
    let a2 = ctx(TENANT_A, USER_A2);
    assert!(matches!(
        h.svc
            .set_reaction(&a2, chat.id, asst, "like")
            .await
            .unwrap_err(),
        DomainError::ChatNotFound { .. }
    ));
    h.stop().await;
}

#[tokio::test]
async fn models_api_visibility() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let ids: Vec<_> = h
        .svc
        .list_models(&a)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert!(ids.contains(&"prem".to_owned()) && !ids.contains(&"off".to_owned()));
    assert!(h.svc.get_model(&a, "std").await.is_ok());
    assert!(matches!(
        h.svc.get_model(&a, "off").await.unwrap_err(),
        DomainError::ModelNotFound { .. }
    ));
    assert!(matches!(
        h.svc.get_model(&a, "nope").await.unwrap_err(),
        DomainError::ModelNotFound { .. }
    ));
    h.stop().await;
}
