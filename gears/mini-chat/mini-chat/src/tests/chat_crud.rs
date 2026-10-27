#![allow(clippy::wildcard_imports)]
//! Chat CRUD tests: lifecycle, validation, ordering, pagination, `OData`.

use toolkit_odata::{ODataQuery, ODataOrderBy, OrderKey, SortDir};
use toolkit_odata::ast;
use toolkit_odata::ast::CompareOperator;
use uuid::Uuid;

use super::*;
use crate::domain::chat_service::CreateChatInput;
use crate::error::DomainError;

#[tokio::test]
async fn create_get_update_delete_lifecycle() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;

    // Create with title and explicit model.
    let chat = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: Some("  Hello  ".to_owned()), model: Some("gpt-standard".to_owned()) })
        .await
        .expect("create");
    assert_eq!(chat.title.as_deref(), Some("Hello")); // trimmed
    assert_eq!(chat.model, "gpt-standard");
    assert!(!chat.is_temporary);
    assert_eq!(chat.message_count, 0);

    // Get.
    let got = svc.chats.get_chat(ctx, chat.id).await.expect("get");
    assert_eq!(got.id, chat.id);
    assert_eq!(got.title.as_deref(), Some("Hello"));

    // Update title bumps updated_at.
    let updated = svc
        .chats
        .update_chat(ctx, chat.id, "Renamed".to_owned())
        .await
        .expect("update");
    assert_eq!(updated.title.as_deref(), Some("Renamed"));
    assert!(updated.updated_at >= got.updated_at);
    assert_eq!(updated.model, "gpt-standard"); // model unchanged

    // Unknown fields are ignored by the DTO (model not updatable) — that is
    // enforced by the DTO shape; here we check the service only renames.

    // Delete; second delete is 404.
    svc.chats.delete_chat(ctx, chat.id).await.expect("delete");
    let err = svc.chats.get_chat(ctx, chat.id).await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
    let err = svc.chats.delete_chat(ctx, chat.id).await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn create_validation() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;

    // Whitespace-only title.
    let err = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: Some("   ".to_owned()), model: None })
        .await;
    assert!(matches!(err, Err(DomainError::InvalidTitle)));
    // Too long.
    let err = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: Some("x".repeat(256)), model: None })
        .await;
    assert!(matches!(err, Err(DomainError::InvalidTitle)));
    // Unknown model.
    let err = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: None, model: Some("nope".to_owned()) })
        .await;
    assert!(matches!(err, Err(DomainError::InvalidModel)));
    // Disabled model.
    let err = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: None, model: Some("gpt-disabled".to_owned()) })
        .await;
    assert!(matches!(err, Err(DomainError::InvalidModel)));
    // Default model resolution: first enabled is_default.
    let chat = svc
        .chats
        .create_chat(ctx, CreateChatInput { title: None, model: None })
        .await
        .expect("default model");
    assert_eq!(chat.model, "gpt-premium");
}

#[tokio::test]
async fn list_orders_by_recent_activity() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;
    let a = svc.chats.create_chat(ctx, CreateChatInput { title: Some("a".to_owned()), model: None }).await.expect("a");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let b = svc.chats.create_chat(ctx, CreateChatInput { title: Some("b".to_owned()), model: None }).await.expect("b");
    let page = svc.chats.list_chats(ctx, &odata()).await.expect("list");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id, b.id, "most recent first");
    assert_eq!(page.items[1].id, a.id);

    // Renaming a bumps it to the top.
    svc.chats.update_chat(ctx, a.id, "a2".to_owned()).await.expect("rename");
    let page = svc.chats.list_chats(ctx, &odata()).await.expect("list");
    assert_eq!(page.items[0].id, a.id);

    // A sent message bumps the chat to the top.
    svc.chats.update_chat(ctx, b.id, "b2".to_owned()).await.expect("rename");
    let _ = send_and_drain(svc, &bed.bed, a.id, "hi", None).await;
    let page = svc.chats.list_chats(ctx, &odata()).await.expect("list");
    assert_eq!(page.items[0].id, a.id, "most recent message first");
    assert_eq!(page.items[0].message_count, 2);
}

#[tokio::test]
async fn list_pagination_and_cursors() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;
    let mut ids = Vec::new();
    for i in 0..5 {
        let chat = svc.chats.create_chat(ctx, CreateChatInput { title: Some(format!("c{i}")), model: None }).await.expect("create");
        ids.push(chat.id);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut q = odata();
    q.limit = Some(2);
    let page1 = svc.chats.list_chats(ctx, &q).await.expect("page1");
    assert_eq!(page1.items.len(), 2);
    assert_eq!(page1.page_info.limit, 2);
    let cursor = page1.page_info.next_cursor.clone().expect("next cursor");
    let mut q2 = odata();
    q2.limit = Some(2);
    q2.cursor = Some(toolkit_odata::CursorV1::decode(&cursor).expect("decode"));
    let page2 = svc.chats.list_chats(ctx, &q2).await.expect("page2");
    assert_eq!(page2.items.len(), 2);
    assert!(page2.page_info.prev_cursor.is_some());
    let all: Vec<_> = page1.items.iter().chain(page2.items.iter()).map(|c| c.id).collect();
    assert_eq!(all.len(), 4);
    assert_eq!(std::collections::HashSet::<_>::from_iter(all).len(), 4, "no duplicates across pages");
}

#[tokio::test]
async fn list_odata_filter_and_orderby() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;
    svc.chats.create_chat(ctx, CreateChatInput { title: Some("alpha".to_owned()), model: None }).await.expect("a");
    svc.chats.create_chat(ctx, CreateChatInput { title: Some("beta".to_owned()), model: None }).await.expect("b");

    // contains(title, 'alph')
    let mut q = odata();
    q.filter = Some(Box::new(ast::Expr::Function(
        "contains".to_owned(),
        vec![
            ast::Expr::Identifier("title".to_owned()),
            ast::Expr::Value(ast::Value::String("alph".to_owned())),
        ],
    )));
    let page = svc.chats.list_chats(ctx, &q).await.expect("filtered");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("alpha"));

    // orderby title asc
    let mut q = odata();
    q.order = ODataOrderBy(vec![OrderKey { field: "title".to_owned(), dir: SortDir::Asc }]);
    let page = svc.chats.list_chats(ctx, &q).await.expect("ordered");
    assert_eq!(page.items[0].title.as_deref(), Some("alpha"));
    assert_eq!(page.items[1].title.as_deref(), Some("beta"));

    // Unknown field -> 400.
    let mut q = odata();
    q.filter = Some(Box::new(ast::Expr::Compare(
        Box::new(ast::Expr::Identifier("nope".to_owned())),
        CompareOperator::Eq,
        Box::new(ast::Expr::Value(ast::Value::String("x".to_owned()))),
    )));
    let err = svc.chats.list_chats(ctx, &q).await;
    assert!(matches!(err, Err(DomainError::Canonical(_))));
}

#[tokio::test]
async fn tenant_and_owner_isolation() {
    let bed = stream_bed().await;
    let svc = &bed.services;
    let ctx = &bed.bed.security;
    let chat = svc.chats.create_chat(ctx, CreateChatInput { title: Some("mine".to_owned()), model: None }).await.expect("create");

    // Another user, same tenant.
    let other_user = toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(ctx.subject_tenant_id())
        .build()
        .expect("ctx");
    let err = svc.chats.get_chat(&other_user, chat.id).await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));

    // Same user id, another tenant.
    let other_tenant = toolkit_security::SecurityContext::builder()
        .subject_id(ctx.subject_id())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("ctx");
    let err = svc.chats.get_chat(&other_tenant, chat.id).await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
}
