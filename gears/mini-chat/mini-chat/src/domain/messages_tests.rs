//! The message projection (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): required and optional fields, the NULL
//! `request_id` guard, the attachment summaries and the caller's reactions.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-message-projection`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-messages-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use time::OffsetDateTime;
use uuid::Uuid;

use super::{AttachmentRow, MessageRole, MessageRow, Reaction, ThumbnailRow, project_messages};
use crate::domain::error::DomainError;

fn instant(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

fn row(role: &str, content: &str, secs: i64) -> MessageRow {
    MessageRow {
        id: Uuid::new_v4(),
        request_id: Some(Uuid::new_v4()),
        role: role.to_owned(),
        content: content.to_owned(),
        model: None,
        input_tokens: 0,
        output_tokens: 0,
        created_at: instant(secs),
    }
}

fn attachment(message: Uuid, kind: &str, status: &str, thumbnail: bool) -> AttachmentRow {
    AttachmentRow {
        message_id: message,
        attachment_id: Uuid::new_v4(),
        kind: kind.to_owned(),
        filename: format!("{kind}.bin"),
        status: status.to_owned(),
        thumbnail: thumbnail.then(|| ThumbnailRow {
            width: 64,
            height: 48,
            data: vec![1, 2, 3],
        }),
    }
}

fn project(rows: Vec<MessageRow>) -> Vec<super::MessageView> {
    project_messages(rows, &[], &BTreeMap::new()).unwrap()
}

#[test]
fn the_required_fields_are_carried_and_the_optional_ones_absent() {
    let user = row("user", "hello", 5);
    let views = project(vec![user.clone()]);
    assert_eq!(views.len(), 1);
    let view = &views[0];
    assert_eq!(view.id, user.id);
    assert_eq!(Some(view.request_id), user.request_id);
    assert_eq!(view.role, MessageRole::User);
    assert_eq!(view.content, "hello");
    assert_eq!(view.created_at, instant(5));
    assert!(view.attachments.is_empty());
    assert_eq!(view.my_reaction, None);
    assert_eq!(view.model, None);
    assert_eq!(view.input_tokens, None);
    assert_eq!(view.output_tokens, None);
}

#[test]
fn the_page_order_is_kept() {
    let rows = vec![
        row("user", "a", 1),
        row("assistant", "b", 2),
        row("user", "c", 3),
    ];
    let ids: Vec<Uuid> = rows.iter().map(|r| r.id).collect();
    let views = project(rows);
    assert_eq!(views.iter().map(|v| v.id).collect::<Vec<_>>(), ids);
}

#[test]
fn all_three_roles_are_projected() {
    let views = project(vec![
        row("user", "u", 1),
        row("assistant", "a", 2),
        row("system", "s", 3),
    ]);
    let roles: Vec<MessageRole> = views.iter().map(|v| v.role).collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::System
        ]
    );
}

#[test]
fn a_null_request_id_fails_the_whole_page_with_an_internal_error() {
    let mut orphan = row("user", "no key", 2);
    orphan.request_id = None;
    let result = project_messages(vec![row("user", "ok", 1), orphan], &[], &BTreeMap::new());
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
}

#[test]
fn model_is_reported_only_when_set_and_tokens_only_when_not_zero() {
    let mut assistant = row("assistant", "answer", 2);
    assistant.model = Some("gpt-5.2".to_owned());
    assistant.input_tokens = 120;
    assistant.output_tokens = 0;
    let view = project(vec![assistant]).remove(0);
    assert_eq!(view.model.as_deref(), Some("gpt-5.2"));
    assert_eq!(view.input_tokens, Some(120));
    assert_eq!(view.output_tokens, None);

    let mut other = row("assistant", "answer", 3);
    other.input_tokens = 0;
    other.output_tokens = 30;
    let view = project(vec![other]).remove(0);
    assert_eq!(view.model, None);
    assert_eq!(view.input_tokens, None);
    assert_eq!(view.output_tokens, Some(30));
}

#[test]
fn the_reaction_is_the_callers_on_assistant_messages_only() {
    let user = row("user", "q", 1);
    let liked = row("assistant", "a", 2);
    let disliked = row("assistant", "b", 3);
    let plain = row("assistant", "c", 4);
    let system = row("system", "s", 5);
    let reactions = BTreeMap::from([
        (user.id, Reaction::Like),
        (liked.id, Reaction::Like),
        (disliked.id, Reaction::Dislike),
        (system.id, Reaction::Like),
    ]);
    let views = project_messages(
        vec![
            user.clone(),
            liked.clone(),
            disliked.clone(),
            plain.clone(),
            system.clone(),
        ],
        &[],
        &reactions,
    )
    .unwrap();
    let reaction_of = |id: Uuid| views.iter().find(|v| v.id == id).unwrap().my_reaction;
    assert_eq!(
        reaction_of(user.id),
        None,
        "user messages always carry null"
    );
    assert_eq!(reaction_of(liked.id), Some(Reaction::Like));
    assert_eq!(reaction_of(disliked.id), Some(Reaction::Dislike));
    assert_eq!(reaction_of(plain.id), None);
    assert_eq!(
        reaction_of(system.id),
        None,
        "system messages always carry null"
    );
}

#[test]
fn attachments_are_grouped_by_message_in_the_order_of_the_batch() {
    let first = row("user", "one", 1);
    let second = row("user", "two", 2);
    let a = attachment(first.id, "document", "ready", false);
    let b = attachment(second.id, "image", "ready", false);
    let c = attachment(first.id, "image", "uploaded", false);
    let views = project_messages(
        vec![first, second],
        &[a.clone(), b.clone(), c.clone()],
        &BTreeMap::new(),
    )
    .unwrap();
    let ids = |index: usize| -> Vec<Uuid> {
        views[index]
            .attachments
            .iter()
            .map(|x| x.attachment_id)
            .collect()
    };
    assert_eq!(ids(0), vec![a.attachment_id, c.attachment_id]);
    assert_eq!(ids(1), vec![b.attachment_id]);
    let summary = &views[0].attachments[0];
    assert_eq!(summary.kind, "document");
    assert_eq!(summary.filename, "document.bin");
    assert_eq!(summary.status, "ready");
}

#[test]
fn assistant_and_system_messages_always_have_an_empty_attachment_list() {
    let assistant = row("assistant", "a", 1);
    let system = row("system", "s", 2);
    let rows = [
        attachment(assistant.id, "document", "ready", false),
        attachment(system.id, "document", "ready", false),
    ];
    let views = project_messages(vec![assistant, system], &rows, &BTreeMap::new()).unwrap();
    assert!(views.iter().all(|v| v.attachments.is_empty()), "{views:?}");
}

#[test]
fn a_thumbnail_is_attached_only_to_a_ready_image_that_has_one() {
    let message = row("user", "look", 1);
    let ready_image = attachment(message.id, "image", "ready", true);
    let image_without = attachment(message.id, "image", "ready", false);
    let uploaded_image = attachment(message.id, "image", "uploaded", true);
    let failed_image = attachment(message.id, "image", "failed", true);
    let pending_image = attachment(message.id, "image", "pending", true);
    let document = attachment(message.id, "document", "ready", true);
    let views = project_messages(
        vec![message],
        &[
            ready_image.clone(),
            image_without.clone(),
            uploaded_image.clone(),
            failed_image.clone(),
            pending_image.clone(),
            document.clone(),
        ],
        &BTreeMap::new(),
    )
    .unwrap();
    let thumb = |id: Uuid| {
        views[0]
            .attachments
            .iter()
            .find(|a| a.attachment_id == id)
            .unwrap()
            .img_thumbnail
            .clone()
    };
    let shown = thumb(ready_image.attachment_id).expect("ready image with a thumbnail");
    assert_eq!(shown.content_type, "image/webp");
    assert_eq!((shown.width, shown.height), (64, 48));
    assert_eq!(shown.data, vec![1, 2, 3]);
    for hidden in [
        &image_without,
        &uploaded_image,
        &failed_image,
        &pending_image,
        &document,
    ] {
        assert_eq!(thumb(hidden.attachment_id), None, "{hidden:?}");
    }
}
