//! `DELETE /v1/chats/{id}/attachments/{attachment_id}` over the router, SQLite, the real outbox
//! and a mock OAGW: 204 with the soft delete and one `attachment_deleted` message in the same
//! transaction and no provider call, the identical repeated 204 without a second message, the
//! exclusion from the get flow, the 404 with the attachment type (unknown, other chat, other
//! uploader, deleted by another user, other tenant) and with the chat type, the 409 `already_exists`
//! with `resource_name` `attachment_locked` for a referenced attachment, the 500 for an oversized
//! payload or a database failure with nothing changed, the admission (`delete_attachment` on the
//! chat), the path validation and the absence of provider text in every body.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`: flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-delete-attachment` (`inst-del-*`),
//! algorithm `-delete-transaction` and `-error-contract` (`inst-err-locked`,
//! `inst-err-attachment-404`, `inst-err-path`, `inst-err-internal`); definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::Value;
use uuid::Uuid;

use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, ANTHROPIC_CHAT_MODEL, ATTACHMENT_RESOURCE, AttachRig, AttachRigOptions,
    RowSeed, assert_no_provider_text, rig, rig_with, seed_row,
};
use crate::test_support::chats::{
    CHAT_RESOURCE, a1, a2, assert_chat_not_found, assert_problem, at, b1, fail_outbox_inserts,
    request,
};
use crate::test_support::send::{MessageSeed, link_att, seed_msg, t};
use crate::test_support::{Pdp, TENANT_B, USER_A2};

fn assert_attachment_not_found(reply: &crate::test_support::chats::Reply) -> Value {
    let body = assert_problem(reply, 404, "not_found");
    assert_eq!(
        body["context"]["resource_type"], ATTACHMENT_RESOURCE,
        "{body}"
    );
    body
}

async fn linked(rig: &AttachRig, chat: &crate::infra::storage::repo_impl::ChatRecord, id: Uuid) {
    let message = seed_msg(&rig.db, chat, MessageSeed::new("user", "see it", t(1))).await;
    link_att(&rig.db, chat, message, id).await;
}

// ── 204 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn deleting_an_unreferenced_attachment_is_204_without_a_body_or_a_provider_call() {
    // inst-del-request, inst-del-transaction, inst-del-case-204, inst-dtx-exclusion
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let reply = rig.delete_attachment(chat.id, id).await;
    assert_eq!(reply.status.as_u16(), 204, "{}", reply.text);
    assert!(reply.text.is_empty(), "no body: {}", reply.text);
    assert_eq!(
        rig.storage.call_count(),
        0,
        "no provider call inside the request"
    );
}

#[tokio::test]
async fn the_delete_soft_deletes_the_row_and_enqueues_one_message_atomically() {
    // inst-dtx-soft-delete, inst-dtx-enqueue, inst-dtx-commit
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(
        rig.delete_attachment(chat.id, id).await.status.as_u16(),
        204
    );
    let row = rig.row(chat.id, id).await;
    assert!(row.deleted_at.is_some());
    assert_eq!(row.cleanup_status.as_deref(), Some("pending"));
    let messages = rig.attachment_cleanup_rows().await;
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].payload["event_type"], "attachment_deleted");
    assert_eq!(messages[0].payload["attachment_id"], id.to_string());
}

#[tokio::test]
async fn the_deleted_attachment_is_gone_from_the_get_flow() {
    // inst-dtx-exclusion: excluded from chat metadata at once
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    assert_eq!(rig.get_attachment(chat.id, id).await.status.as_u16(), 200);
    rig.delete_attachment(chat.id, id).await;
    assert_attachment_not_found(&rig.get_attachment(chat.id, id).await);
}

#[tokio::test]
async fn deleting_works_for_every_attachment_status_and_kind() {
    for seed in [
        RowSeed::document().status("pending").no_provider_file(),
        RowSeed::document().status("uploaded"),
        RowSeed::document()
            .status("failed")
            .error("indexing_failed"),
        RowSeed::image(),
        RowSeed::document().code_interpreter_only(),
    ] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let id = seed_row(&rig.db, &chat, seed).await;
        assert_eq!(
            rig.delete_attachment(chat.id, id).await.status.as_u16(),
            204
        );
    }
}

#[tokio::test]
async fn an_anthropic_chat_image_with_an_uploaded_secondary_copy_enqueues_its_secondary_ref() {
    // inst-enq-secondary, inst-enq-secondary-ref: the Anthropic `upstream_alias` resolved for the
    // tenant
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().secondary("file_ant_9", "uploaded"),
    )
    .await;
    assert_eq!(
        rig.delete_attachment(chat.id, id).await.status.as_u16(),
        204
    );
    let payload = rig.attachment_cleanup_rows().await.remove(0).payload;
    assert_eq!(
        payload["secondary_ref"],
        serde_json::json!({
            "file_id": "file_ant_9",
            "provider_kind": "anthropic",
            "upstream_alias": ANTHROPIC_ALIAS,
        })
    );
    assert_eq!(
        rig.storage.call_count(),
        0,
        "no provider call inside the request"
    );
}

#[tokio::test]
async fn an_image_without_a_secondary_copy_enqueues_a_null_secondary_ref() {
    let rig = rig().await;
    let chat = rig.chat(ANTHROPIC_CHAT_MODEL).await;
    let failed = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().secondary("file_ant_9", "failed"),
    )
    .await;
    let plain = seed_row(&rig.db, &chat, RowSeed::image()).await;
    for id in [failed, plain] {
        assert_eq!(
            rig.delete_attachment(chat.id, id).await.status.as_u16(),
            204
        );
    }
    for message in rig.attachment_cleanup_rows().await {
        assert!(
            message.payload["secondary_ref"].is_null(),
            "{}",
            message.payload
        );
    }
}

// ── the repeat ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_repeated_delete_is_204_again_without_a_second_message() {
    // inst-del-case-idempotent, inst-dtx-already
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    for _ in 0..3 {
        assert_eq!(
            rig.delete_attachment(chat.id, id).await.status.as_u16(),
            204
        );
    }
    assert_eq!(rig.attachment_cleanup_rows().await.len(), 1);
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn a_repeated_delete_of_an_attachment_deleted_elsewhere_is_204_without_a_message() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().deleted(at(5))).await;
    assert_eq!(
        rig.delete_attachment(chat.id, id).await.status.as_u16(),
        204
    );
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

// ── 404 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_attachment_is_404_with_the_attachment_type() {
    // inst-del-case-404, inst-err-attachment-404
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.delete_attachment(chat.id, Uuid::new_v4()).await;
    assert_attachment_not_found(&reply);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

#[tokio::test]
async fn an_attachment_of_another_chat_is_404_and_stays_untouched() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let other = rig.std_chat().await;
    let id = seed_row(&rig.db, &other, RowSeed::document()).await;
    assert_attachment_not_found(&rig.delete_attachment(chat.id, id).await);
    let row = rig.row(other.id, id).await;
    assert_eq!(row.deleted_at, None);
    assert_eq!(row.cleanup_status, None);
}

#[tokio::test]
async fn an_attachment_uploaded_by_another_user_is_404_also_on_a_repeat() {
    // inst-dtx-not-found: the uploader check runs before the idempotency check
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let live = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    let gone = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().uploader(USER_A2).deleted(at(5)),
    )
    .await;
    for id in [live, gone, live, gone] {
        assert_attachment_not_found(&rig.delete_attachment(chat.id, id).await);
    }
    assert_eq!(rig.row(chat.id, live).await.deleted_at, None);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

#[tokio::test]
async fn an_attachment_of_another_tenant_is_404() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().tenant(TENANT_B)).await;
    assert_attachment_not_found(&rig.delete_attachment(chat.id, id).await);
}

#[tokio::test]
async fn the_four_hidden_cases_give_the_same_404() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let other = rig.std_chat().await;
    let in_other_chat = seed_row(&rig.db, &other, RowSeed::document()).await;
    let colleague = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    let mut bodies = Vec::new();
    for id in [Uuid::new_v4(), in_other_chat, colleague] {
        let mut body = assert_attachment_not_found(&rig.delete_attachment(chat.id, id).await);
        body["instance"] = Value::Null;
        body["trace_id"] = Value::Null;
        body["context"]["resource_name"] = Value::Null;
        bodies.push(body);
    }
    assert_eq!(bodies[0], bodies[1]);
    assert_eq!(bodies[0], bodies[2]);
}

#[tokio::test]
async fn a_foreign_missing_or_other_tenant_chat_is_404_with_the_chat_type() {
    // inst-del-admission: 404 for the chat type, never 403
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    for reply in [
        rig.delete_attachment_as(&a2(), chat.id, id).await,
        rig.delete_attachment_as(&b1(), chat.id, id).await,
        rig.delete_attachment(Uuid::new_v4(), id).await,
    ] {
        let body = assert_chat_not_found(&reply);
        assert_eq!(body["context"]["resource_type"], CHAT_RESOURCE);
    }
    assert_eq!(rig.row(chat.id, id).await.deleted_at, None);
}

#[tokio::test]
async fn a_soft_deleted_chat_is_404_with_the_chat_type() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    crate::test_support::attachments::soft_delete_chat(&rig.db, chat.id, at(9)).await;
    assert_chat_not_found(&rig.delete_attachment(chat.id, id).await);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

// ── 409 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_referenced_attachment_is_409_already_exists_attachment_locked() {
    // inst-del-case-locked, inst-err-locked, inst-dtx-return-locked
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, id).await;
    let reply = rig.delete_attachment(chat.id, id).await;
    let body = assert_problem(&reply, 409, "already_exists");
    assert_eq!(
        body["context"]["resource_name"], "attachment_locked",
        "{body}"
    );
    assert_eq!(
        body["context"]["resource_type"], ATTACHMENT_RESOURCE,
        "{body}"
    );
    assert_no_provider_text(&reply.text);
}

#[tokio::test]
async fn a_locked_attachment_changes_nothing_and_stays_gettable() {
    // the message's `attachments` array is never modified
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, id).await;
    rig.delete_attachment(chat.id, id).await;
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.deleted_at, None);
    assert_eq!(row.cleanup_status, None);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
    assert_eq!(rig.get_attachment(chat.id, id).await.status.as_u16(), 200);
    assert_eq!(
        crate::test_support::send::link_rows(&rig.db, chat.id)
            .await
            .len(),
        1
    );
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn the_lock_repeats_on_every_delete() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, id).await;
    for _ in 0..2 {
        assert_eq!(
            rig.delete_attachment(chat.id, id).await.status.as_u16(),
            409
        );
    }
}

#[tokio::test]
async fn the_404_of_another_uploader_wins_over_the_lock() {
    // the uploader check precedes everything else about the row
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    linked(&rig, &chat, id).await;
    assert_attachment_not_found(&rig.delete_attachment(chat.id, id).await);
}

// ── 500 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_oversized_cleanup_payload_is_500_internal_with_a_generic_detail_and_nothing_changed() {
    // inst-del-case-500, inst-dtx-too-large, inst-err-internal
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::document().file_id(&"f".repeat(70_000)),
    )
    .await;
    let reply = rig.delete_attachment(chat.id, id).await;
    let body = assert_problem(&reply, 500, "internal");
    assert!(
        !reply.text.contains("ffff"),
        "no payload content: {}",
        reply.text
    );
    assert!(
        !body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("outbox"),
        "a generic detail: {body}"
    );
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.deleted_at, None, "the attachment stays active");
    assert_eq!(row.cleanup_status, None);
    assert!(rig.attachment_cleanup_rows().await.is_empty());
}

#[tokio::test]
async fn a_database_failure_is_500_internal_and_nothing_is_changed() {
    // inst-del-case-500, inst-dtx-return-db-failure
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    fail_outbox_inserts(&rig.db).await;
    let reply = rig.delete_attachment(chat.id, id).await;
    let body = assert_problem(&reply, 500, "internal");
    assert!(
        !body.to_string().to_lowercase().contains("forced"),
        "{body}"
    );
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.deleted_at, None);
    assert_eq!(row.cleanup_status, None);
}

#[tokio::test]
async fn an_injected_store_failure_is_500_internal() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    rig.faults
        .delete
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_problem(&rig.delete_attachment(chat.id, id).await, 500, "internal");
}

// ── admission and the path ───────────────────────────────────────────────────

#[tokio::test]
async fn the_pdp_is_asked_for_delete_attachment_on_the_chat() {
    // inst-del-admission: the action `delete_attachment` and the chat id as `resource.id`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    rig.delete_attachment(chat.id, id).await;
    let evaluated = rig.pdp.only_request();
    assert_eq!(evaluated.action.name, "delete_attachment");
    assert_eq!(evaluated.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_deny_is_403_and_an_unreachable_pdp_is_503_without_touching_the_row() {
    // inst-del-admission: its refusal is returned (401, 403, 503)
    let denied = rig_with(AttachRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = denied.std_chat().await;
    let id = seed_row(&denied.db, &chat, RowSeed::document()).await;
    let reply = denied.delete_attachment(chat.id, id).await;
    assert_eq!(
        assert_problem(&reply, 403, "permission_denied")["context"]["reason"],
        "AUTHZ_DENIED"
    );
    assert_eq!(denied.row(chat.id, id).await.deleted_at, None);

    let down = rig_with(AttachRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = down.std_chat().await;
    let id = seed_row(&down.db, &chat, RowSeed::document()).await;
    let reply = down.delete_attachment(chat.id, id).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert!(down.attachment_cleanup_rows().await.is_empty());
}

#[tokio::test]
async fn a_non_uuid_path_parameter_is_400_invalid_path_params() {
    // inst-err-path
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    for uri in [
        format!("/mini-chat/v1/chats/not-a-uuid/attachments/{id}"),
        format!("/mini-chat/v1/chats/{}/attachments/not-a-uuid", chat.id),
    ] {
        let reply = rig.send(request("DELETE", &uri, &a1())).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        assert!(
            body.to_string().contains("invalid_path_params"),
            "{uri}: {body}"
        );
    }
    assert_eq!(rig.row(chat.id, id).await.deleted_at, None);
}

#[tokio::test]
async fn no_provider_identifier_or_cleanup_state_reaches_any_response() {
    // dod-errors-observability: no provider identifier in any body
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let live = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let locked = seed_row(&rig.db, &chat, RowSeed::document()).await;
    linked(&rig, &chat, locked).await;
    for reply in [
        rig.delete_attachment(chat.id, live).await,
        rig.delete_attachment(chat.id, live).await,
        rig.delete_attachment(chat.id, locked).await,
        rig.delete_attachment(chat.id, Uuid::new_v4()).await,
    ] {
        assert_no_provider_text(&reply.text);
        assert!(!reply.text.contains("cleanup"), "{}", reply.text);
    }
}
