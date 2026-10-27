#![allow(clippy::wildcard_imports)]
//! Messages API tests: list contract, reactions, message count.

use uuid::Uuid;

use super::*;
use crate::error::DomainError;

#[tokio::test]
async fn message_contract_after_turn() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let events = send_and_drain(&bed.services, &bed.bed, chat.id, "hello", None).await;
    eprintln!("events: {}", events.len());
    for e in &events { eprintln!("ev: {e:?}"); }

    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    assert_eq!(page.items.len(), 2);
    let (user, assistant) = (&page.items[0], &page.items[1]);
    // Required fields always present.
    assert_eq!(user.role, crate::api::rest::dto::MessageRoleDto::User);
    assert_eq!(assistant.role, crate::api::rest::dto::MessageRoleDto::Assistant);
    assert_eq!(user.request_id, assistant.request_id);
    assert!(user.attachments.is_empty());
    assert!(assistant.attachments.is_empty());
    assert!(user.my_reaction.is_none());
    assert!(assistant.my_reaction.is_none());
    assert!(user.model.is_none());
    assert_eq!(assistant.model.as_deref(), Some("gpt-premium"));
    assert_eq!(assistant.input_tokens, Some(10));
    assert_eq!(assistant.output_tokens, Some(5));

    // Chat detail message_count.
    let detail = bed.services.chats.get_chat(&bed.bed.security, chat.id).await.expect("get");
    assert_eq!(detail.message_count, 2);
}

#[tokio::test]
async fn reactions_lifecycle() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    send_and_drain(&bed.services, &bed.bed, chat.id, "hello", None).await;
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let assistant = page.items.iter().find(|m| m.role == crate::api::rest::dto::MessageRoleDto::Assistant).expect("assistant");
    let user = page.items.iter().find(|m| m.role == crate::api::rest::dto::MessageRoleDto::User).expect("user");

    // Invalid value -> 400 INVALID_REACTION (before authorization).
    let err = bed
        .services
        .chats
        .set_reaction(&bed.bed.security, chat.id, assistant.id, "meh")
        .await;
    assert!(matches!(err, Err(DomainError::InvalidReaction)));

    // On user message -> 400 failed_precondition.
    let err = bed
        .services
        .chats
        .set_reaction(&bed.bed.security, chat.id, user.id, "like")
        .await;
    assert!(matches!(err, Err(DomainError::ReactionTargetNotAssistant)));
    let err = bed
        .services
        .chats
        .delete_reaction(&bed.bed.security, chat.id, user.id)
        .await;
    assert!(matches!(err, Err(DomainError::ReactionTargetNotAssistant)));

    // Set like.
    let r = bed
        .services
        .chats
        .set_reaction(&bed.bed.security, chat.id, assistant.id, "like")
        .await
        .expect("like");
    assert_eq!(r.message_id, assistant.id);

    // my_reaction shows up in the list.
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let assistant = page.items.iter().find(|m| m.id == assistant.id).expect("assistant");
    assert_eq!(assistant.my_reaction, Some(crate::api::rest::dto::ReactionKindDto::Like));

    // Change to dislike (upsert).
    bed.services
        .chats
        .set_reaction(&bed.bed.security, chat.id, assistant.id, "dislike")
        .await
        .expect("dislike");
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let assistant = page.items.iter().find(|m| m.id == assistant.id).expect("assistant");
    assert_eq!(assistant.my_reaction, Some(crate::api::rest::dto::ReactionKindDto::Dislike));

    // Remove; idempotent.
    bed.services
        .chats
        .delete_reaction(&bed.bed.security, chat.id, assistant.id)
        .await
        .expect("remove");
    bed.services
        .chats
        .delete_reaction(&bed.bed.security, chat.id, assistant.id)
        .await
        .expect("remove again");
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    let assistant = page.items.iter().find(|m| m.id == assistant.id).expect("assistant");
    assert!(assistant.my_reaction.is_none());

    // Unknown message -> 404.
    let err = bed
        .services
        .chats
        .set_reaction(&bed.bed.security, chat.id, Uuid::new_v4(), "like")
        .await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn message_pagination_desc_ordering() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    for _ in 0..3 {
        send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;
    }
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    assert_eq!(page.items.len(), 6);
    // Default chronological order.
    for w in page.items.windows(2) {
        assert!(w[0].created_at <= w[1].created_at);
    }
}
