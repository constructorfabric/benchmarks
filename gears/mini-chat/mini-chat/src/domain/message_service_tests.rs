#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use toolkit_odata::{CursorV1, ODataOrderBy, ODataQuery, OrderKey, SortDir, ast};
use uuid::Uuid;

use super::*;
use crate::domain::chat_service::tests::{OwnerPdp, a1, ctx_of, owner_authz};
use crate::domain::error::{DomainError, InvalidArgumentReason, PreconditionSubject, ResourceKind};
use crate::domain::ports::{AttachmentKind, AttachmentStatus, MessageRole, Reaction, TurnState};
use crate::infra::db::repo::tests::{
    AttachmentSeed, MessageSeed, TENANT_A, TENANT_B, TestStore, TurnSeed, USER_A1, USER_A2,
    USER_B1, at, link_attachment, reaction_rows, seed_attachment, seed_chat, seed_message,
    seed_reaction, seed_turn,
};

struct Fixture {
    store: TestStore,
    pdp: Arc<OwnerPdp>,
    service: MessageService,
    chat: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let store = TestStore::without_outbox().await;
        let pdp = OwnerPdp::new();
        let service = MessageService::new(store.repo.repositories(), owner_authz(&pdp));
        let chat = seed_chat(&store, TENANT_A, USER_A1, Some("messages")).await;
        Self {
            store,
            pdp,
            service,
            chat,
        }
    }

    async fn message(&self, seed: MessageSeed) -> Uuid {
        seed_message(&self.store, self.chat, TENANT_A, seed).await
    }

    /// A user + assistant message pair sharing one request id.
    async fn turn(&self, offset_ms: i64) -> (Uuid, Uuid) {
        let request_id = Uuid::new_v4();
        let mut user = MessageSeed::new(MessageRole::User, offset_ms);
        user.request_id = Some(request_id);
        let mut assistant = MessageSeed::new(MessageRole::Assistant, offset_ms + 5);
        assistant.request_id = Some(request_id);
        (self.message(user).await, self.message(assistant).await)
    }

    async fn list(&self, query: &ODataQuery) -> Page<MessageView> {
        self.service
            .list_messages(&a1(), self.chat, query)
            .await
            .unwrap()
    }
}

fn ids(page: &Page<MessageView>) -> Vec<Uuid> {
    page.items.iter().map(|m| m.id).collect()
}

fn compare(field: &str, value: ast::Value) -> ODataQuery {
    ODataQuery::new().with_filter(ast::Expr::Compare(
        Box::new(ast::Expr::Identifier(field.to_owned())),
        ast::CompareOperator::Eq,
        Box::new(ast::Expr::Value(value)),
    ))
}

fn is_not_found(err: &DomainError, kind: ResourceKind) -> bool {
    matches!(err, DomainError::NotFound { resource, .. } if *resource == kind)
}

// ── List messages ────────────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_live_messages_chronologically_with_all_fields() {
    let fx = Fixture::new().await;
    let (user, assistant) = fx.turn(-100).await;
    let mut tokens = MessageSeed::new(MessageRole::Assistant, -50);
    tokens.input_tokens = 21;
    tokens.output_tokens = 7;
    let counted = fx.message(tokens).await;
    let mut gone = MessageSeed::new(MessageRole::User, -75);
    gone.deleted = true;
    fx.message(gone).await;

    let page = fx.list(&ODataQuery::new()).await;

    assert_eq!(page.page_info.limit, 20);
    assert_eq!(page.page_info.next_cursor, None);
    assert_eq!(ids(&page), [user, assistant, counted]);
    let first = &page.items[0];
    assert_eq!(first.role, MessageRole::User);
    assert_eq!(first.model, None);
    assert_eq!(first.request_id, page.items[1].request_id);
    assert!(first.attachments.is_empty());
    assert_eq!(first.my_reaction, None);
    assert_eq!(page.items[1].model.as_deref(), Some("standard"));
    assert_eq!(
        (page.items[2].input_tokens, page.items[2].output_tokens),
        (21, 7)
    );
}

#[tokio::test]
async fn list_filters_by_role_and_by_plain_or_quoted_id() {
    let fx = Fixture::new().await;
    let (_, first) = fx.turn(-200).await;
    let (_, second) = fx.turn(-100).await;

    let by_role = fx
        .list(&compare("role", ast::Value::String("assistant".to_owned())))
        .await;
    assert_eq!(ids(&by_role), [first, second]);

    let plain = fx.list(&compare("id", ast::Value::Uuid(second))).await;
    assert_eq!(ids(&plain), [second]);

    let quoted = fx
        .list(&compare("id", ast::Value::String(second.to_string())))
        .await;
    assert_eq!(ids(&quoted), [second]);
}

#[tokio::test]
async fn list_orders_by_created_at_desc_on_request() {
    let fx = Fixture::new().await;
    let (u1, a1_id) = fx.turn(-200).await;
    let (u2, a2_id) = fx.turn(-100).await;

    let query = ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
        field: "created_at".to_owned(),
        dir: SortDir::Desc,
    }]));

    assert_eq!(ids(&fx.list(&query).await), [a2_id, u2, a1_id, u1]);
}

#[tokio::test]
async fn list_pages_with_cursors() {
    let fx = Fixture::new().await;
    let mut expected = Vec::new();
    for offset in [-300, -200, -100] {
        let (user, assistant) = fx.turn(offset).await;
        expected.extend([user, assistant]);
    }

    let first = fx.list(&ODataQuery::new().with_limit(4)).await;
    assert_eq!(ids(&first), expected[..4]);
    let next = CursorV1::decode(first.page_info.next_cursor.as_deref().unwrap()).unwrap();

    let second = fx
        .list(&ODataQuery::new().with_limit(4).with_cursor(next))
        .await;
    assert_eq!(ids(&second), expected[4..]);
    assert_eq!(second.page_info.next_cursor, None);
    assert!(second.page_info.prev_cursor.is_some());
}

#[tokio::test]
async fn list_query_errors_are_invalid_query() {
    let fx = Fixture::new().await;

    let cases = [
        compare("content", ast::Value::String("x".to_owned())),
        ODataQuery::new().with_order(ODataOrderBy(vec![OrderKey {
            field: "content".to_owned(),
            dir: SortDir::Asc,
        }])),
        ODataQuery::new().with_limit(0),
        compare("id", ast::Value::String("not-a-uuid".to_owned())),
    ];
    for query in cases {
        let err = fx
            .service
            .list_messages(&a1(), fx.chat, &query)
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::InvalidQuery(_)), "{err:?}");
    }
    assert_eq!(
        fx.list(&ODataQuery::new().with_limit(1000))
            .await
            .page_info
            .limit,
        100
    );
}

#[tokio::test]
async fn list_of_foreign_unknown_or_deleted_chat_is_chat_not_found() {
    let fx = Fixture::new().await;
    fx.turn(0).await;

    for ctx in [ctx_of(TENANT_A, USER_A2), ctx_of(TENANT_B, USER_B1)] {
        let err = fx
            .service
            .list_messages(&ctx, fx.chat, &ODataQuery::new())
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Chat));
    }
    let err = fx
        .service
        .list_messages(&a1(), Uuid::new_v4(), &ODataQuery::new())
        .await
        .unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat));

    crate::infra::db::repo::chats::mark_deleted(
        &fx.store.conn(),
        fx.store.ts(),
        &crate::infra::db::repo::tests::subject_scope(TENANT_A, USER_A1),
        fx.chat,
        at(0),
    )
    .await
    .unwrap();
    let err = fx
        .service
        .list_messages(&a1(), fx.chat, &ODataQuery::new())
        .await
        .unwrap_err();
    assert!(is_not_found(&err, ResourceKind::Chat));
}

#[tokio::test]
async fn a_stored_message_without_request_id_fails_the_request() {
    let fx = Fixture::new().await;
    let mut broken = MessageSeed::new(MessageRole::System, 0);
    broken.request_id = None;
    fx.message(broken).await;

    let err = fx
        .service
        .list_messages(&a1(), fx.chat, &ODataQuery::new())
        .await
        .unwrap_err();

    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
}

#[tokio::test]
async fn list_embeds_live_attachment_summaries_with_ready_image_thumbnails() {
    let fx = Fixture::new().await;
    let (user, assistant) = fx.turn(0).await;
    let image = seed_attachment(
        &fx.store,
        fx.chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Ready, "cat.png"),
    )
    .await;
    let document = seed_attachment(
        &fx.store,
        fx.chat,
        TENANT_A,
        AttachmentSeed::new(
            AttachmentKind::Document,
            AttachmentStatus::Ready,
            "spec.pdf",
        ),
    )
    .await;
    let unready = seed_attachment(
        &fx.store,
        fx.chat,
        TENANT_A,
        AttachmentSeed::new(AttachmentKind::Image, AttachmentStatus::Uploaded, "dog.png"),
    )
    .await;
    let mut gone = AttachmentSeed::new(
        AttachmentKind::Document,
        AttachmentStatus::Ready,
        "gone.pdf",
    );
    gone.deleted = true;
    let gone = seed_attachment(&fx.store, fx.chat, TENANT_A, gone).await;
    for (offset, attachment) in [image, document, unready, gone].into_iter().enumerate() {
        link_attachment(
            &fx.store,
            fx.chat,
            TENANT_A,
            user,
            attachment,
            i64::try_from(offset).unwrap(),
        )
        .await;
    }

    let page = fx.list(&ODataQuery::new()).await;

    let by_id: std::collections::HashMap<Uuid, &MessageView> =
        page.items.iter().map(|m| (m.id, m)).collect();
    assert!(by_id[&assistant].attachments.is_empty());
    let summaries = &by_id[&user].attachments;
    assert_eq!(
        summaries
            .iter()
            .map(|a| a.attachment_id)
            .collect::<Vec<_>>(),
        [image, document, unready]
    );
    let thumbnail = summaries[0]
        .img_thumbnail
        .as_ref()
        .expect("ready image thumbnail");
    assert_eq!(thumbnail.content_type, "image/webp");
    assert_eq!((thumbnail.width, thumbnail.height), (64, 48));
    assert_eq!(thumbnail.data, [0x52, 0x49, 0x46, 0x46]);
    assert_eq!(summaries[0].filename, "cat.png");
    assert_eq!(summaries[1].kind, AttachmentKind::Document);
    assert_eq!(summaries[1].img_thumbnail, None);
    assert_eq!(summaries[2].status, AttachmentStatus::Uploaded);
    assert_eq!(
        summaries[2].img_thumbnail, None,
        "thumbnail only for a ready image"
    );
}

#[tokio::test]
async fn my_reaction_is_the_callers_reaction_on_assistant_messages_only() {
    let fx = Fixture::new().await;
    let (user_one, first) = fx.turn(-200).await;
    let (_, second) = fx.turn(-100).await;
    seed_reaction(&fx.store, first, TENANT_A, USER_A2, Reaction::Like).await;
    seed_reaction(&fx.store, second, TENANT_A, USER_A2, Reaction::Like).await;
    seed_reaction(&fx.store, user_one, TENANT_A, USER_A1, Reaction::Like).await;
    fx.service
        .set_reaction(&a1(), fx.chat, first, "dislike")
        .await
        .unwrap();

    let page = fx.list(&ODataQuery::new()).await;

    let reactions: std::collections::HashMap<Uuid, Option<Reaction>> =
        page.items.iter().map(|m| (m.id, m.my_reaction)).collect();
    assert_eq!(reactions[&first], Some(Reaction::Dislike));
    assert_eq!(reactions[&second], None);
    assert_eq!(
        reactions[&user_one], None,
        "user messages never carry a reaction"
    );
}

// ── Reactions ────────────────────────────────────────────────────────────

#[tokio::test]
async fn set_reaction_upserts_and_returns_the_stored_reaction() {
    let fx = Fixture::new().await;
    let (_, assistant) = fx.turn(0).await;

    let liked = fx
        .service
        .set_reaction(&a1(), fx.chat, assistant, "like")
        .await
        .unwrap();
    assert_eq!(
        (liked.message_id, liked.reaction),
        (assistant, Reaction::Like)
    );

    let disliked = fx
        .service
        .set_reaction(&a1(), fx.chat, assistant, "dislike")
        .await
        .unwrap();
    assert_eq!(disliked.reaction, Reaction::Dislike);
    let rows = reaction_rows(&fx.store, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].tenant_id, rows[0].user_id), (TENANT_A, USER_A1));
    assert_eq!(rows[0].reaction, "dislike");
    assert_eq!(rows[0].created_at, disliked.created_at);
}

#[tokio::test]
async fn invalid_reaction_value_is_rejected_before_authorization() {
    let fx = Fixture::new().await;
    let (_, assistant) = fx.turn(0).await;

    for value in ["love", "LIKE", ""] {
        let err = fx
            .service
            .set_reaction(&ctx_of(TENANT_B, USER_B1), fx.chat, assistant, value)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DomainError::InvalidArgument {
                reason: InvalidArgumentReason::InvalidReaction,
                ..
            }
        ));
    }
    assert_eq!(fx.pdp.calls(), 0);
    assert!(reaction_rows(&fx.store, assistant).await.is_empty());
}

#[tokio::test]
async fn reactions_on_user_or_system_messages_are_failed_precondition() {
    let fx = Fixture::new().await;
    let (user, _) = fx.turn(0).await;
    let system = fx.message(MessageSeed::new(MessageRole::System, 10)).await;

    for message in [user, system] {
        let set = fx
            .service
            .set_reaction(&a1(), fx.chat, message, "like")
            .await
            .unwrap_err();
        let delete = fx
            .service
            .delete_reaction(&a1(), fx.chat, message)
            .await
            .unwrap_err();
        for err in [set, delete] {
            assert!(matches!(
                err,
                DomainError::FailedPrecondition {
                    subject: PreconditionSubject::ReactionTarget
                }
            ));
        }
        assert!(reaction_rows(&fx.store, message).await.is_empty());
    }
}

#[tokio::test]
async fn reaction_targets_must_be_live_messages_of_the_chat() {
    let fx = Fixture::new().await;
    let other_chat = seed_chat(&fx.store, TENANT_A, USER_A1, None).await;
    let mut foreign = MessageSeed::new(MessageRole::Assistant, 0);
    foreign.request_id = Some(Uuid::new_v4());
    let foreign = seed_message(&fx.store, other_chat, TENANT_A, foreign).await;
    let mut gone = MessageSeed::new(MessageRole::Assistant, 0);
    gone.deleted = true;
    let gone = fx.message(gone).await;

    for message in [Uuid::new_v4(), foreign, gone] {
        let err = fx
            .service
            .set_reaction(&a1(), fx.chat, message, "like")
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Message), "{err:?}");
        let err = fx
            .service
            .delete_reaction(&a1(), fx.chat, message)
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Message), "{err:?}");
    }
}

#[tokio::test]
async fn reactions_in_foreign_chats_are_chat_not_found() {
    let fx = Fixture::new().await;
    let (_, assistant) = fx.turn(0).await;

    for ctx in [ctx_of(TENANT_A, USER_A2), ctx_of(TENANT_B, USER_B1)] {
        let err = fx
            .service
            .set_reaction(&ctx, fx.chat, assistant, "like")
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Chat));
        let err = fx
            .service
            .delete_reaction(&ctx, fx.chat, assistant)
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Chat));
    }
    assert!(reaction_rows(&fx.store, assistant).await.is_empty());
}

#[tokio::test]
async fn delete_reaction_is_idempotent_and_removes_only_the_callers_row() {
    let fx = Fixture::new().await;
    let (_, assistant) = fx.turn(0).await;
    seed_reaction(&fx.store, assistant, TENANT_A, USER_A2, Reaction::Like).await;
    fx.service
        .set_reaction(&a1(), fx.chat, assistant, "dislike")
        .await
        .unwrap();

    fx.service
        .delete_reaction(&a1(), fx.chat, assistant)
        .await
        .unwrap();
    fx.service
        .delete_reaction(&a1(), fx.chat, assistant)
        .await
        .unwrap();

    let rows = reaction_rows(&fx.store, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        (rows[0].user_id, rows[0].reaction.as_str()),
        (USER_A2, "like")
    );
}

// ── Turn status ──────────────────────────────────────────────────────────

#[tokio::test]
async fn turn_status_maps_internal_states() {
    let fx = Fixture::new().await;
    let assistant = Uuid::new_v4();
    let cases = [
        (TurnState::Running, None, None, TurnStatusState::Running),
        (
            TurnState::Completed,
            None,
            Some(assistant),
            TurnStatusState::Done,
        ),
        (
            TurnState::Failed,
            Some("orphan_timeout"),
            None,
            TurnStatusState::Error,
        ),
        (
            TurnState::Cancelled,
            None,
            Some(assistant),
            TurnStatusState::Cancelled,
        ),
    ];
    for (state, error_code, assistant_message_id, expected) in cases {
        let chat = seed_chat(&fx.store, TENANT_A, USER_A1, None).await;
        let mut seed = TurnSeed::new(state);
        seed.error_code = error_code.map(str::to_owned);
        seed.assistant_message_id = assistant_message_id;
        let updated_at = seed.updated_at;
        let request_id = seed.request_id;
        seed_turn(&fx.store, chat, TENANT_A, seed).await;

        let status = fx
            .service
            .get_turn_status(&a1(), chat, request_id)
            .await
            .unwrap();

        assert_eq!(status.request_id, request_id);
        assert_eq!(status.state, expected);
        assert_eq!(status.error_code.as_deref(), error_code);
        assert_eq!(status.assistant_message_id, assistant_message_id);
        assert_eq!(status.updated_at, updated_at);
    }
    assert_eq!(TurnStatusState::Done.as_str(), "done");
    assert_eq!(TurnStatusState::Error.as_str(), "error");
}

#[tokio::test]
async fn turn_status_exposes_stored_columns_only_in_their_states() {
    let fx = Fixture::new().await;
    let assistant = Uuid::new_v4();
    // Every turn stores both columns; only the states DESIGN "Turn Status
    // API" allows them in expose them.
    let cases = [
        (TurnState::Running, None, None),
        (TurnState::Completed, None, Some(assistant)),
        (TurnState::Failed, Some("provider_error"), None),
        (TurnState::Cancelled, None, Some(assistant)),
    ];
    for (state, error_code, assistant_message_id) in cases {
        let chat = seed_chat(&fx.store, TENANT_A, USER_A1, None).await;
        let mut seed = TurnSeed::new(state);
        seed.error_code = Some("provider_error".to_owned());
        seed.assistant_message_id = Some(assistant);
        let request_id = seed.request_id;
        seed_turn(&fx.store, chat, TENANT_A, seed).await;

        let status = fx
            .service
            .get_turn_status(&a1(), chat, request_id)
            .await
            .unwrap();

        assert_eq!(status.error_code.as_deref(), error_code, "{state:?}");
        assert_eq!(
            status.assistant_message_id, assistant_message_id,
            "{state:?}"
        );
    }
}

#[tokio::test]
async fn cancelled_turn_without_persisted_content_has_no_assistant_message() {
    let fx = Fixture::new().await;
    let seed = TurnSeed::new(TurnState::Cancelled);
    let request_id = seed.request_id;
    seed_turn(&fx.store, fx.chat, TENANT_A, seed).await;

    let status = fx
        .service
        .get_turn_status(&a1(), fx.chat, request_id)
        .await
        .unwrap();

    assert_eq!(status.state, TurnStatusState::Cancelled);
    assert_eq!(status.assistant_message_id, None);
    assert_eq!(status.error_code, None);
}

#[tokio::test]
async fn deleted_unknown_or_other_chats_turns_are_turn_not_found() {
    let fx = Fixture::new().await;
    let mut deleted = TurnSeed::new(TurnState::Completed);
    deleted.deleted = true;
    let deleted_request = deleted.request_id;
    seed_turn(&fx.store, fx.chat, TENANT_A, deleted).await;
    let other_chat = seed_chat(&fx.store, TENANT_A, USER_A1, None).await;
    let elsewhere = TurnSeed::new(TurnState::Completed);
    let elsewhere_request = elsewhere.request_id;
    seed_turn(&fx.store, other_chat, TENANT_A, elsewhere).await;

    for request_id in [deleted_request, elsewhere_request, Uuid::new_v4()] {
        let err = fx
            .service
            .get_turn_status(&a1(), fx.chat, request_id)
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Turn), "{err:?}");
    }
}

#[tokio::test]
async fn turn_status_in_a_foreign_chat_is_chat_not_found() {
    let fx = Fixture::new().await;
    let seed = TurnSeed::new(TurnState::Completed);
    let request_id = seed.request_id;
    seed_turn(&fx.store, fx.chat, TENANT_A, seed).await;

    for ctx in [ctx_of(TENANT_A, USER_A2), ctx_of(TENANT_B, USER_B1)] {
        let err = fx
            .service
            .get_turn_status(&ctx, fx.chat, request_id)
            .await
            .unwrap_err();
        assert!(is_not_found(&err, ResourceKind::Chat));
    }
}
