//! Attachment read and delete: `GET` / `DELETE /v1/chats/{id}/attachments/{attachment_id}` and the
//! attachment summaries of the message list.
//!
//! Source of truth: DESIGN 3.3 "Get Attachment" (`AttachmentDetail`, optional fields omitted when
//! null, `img_thumbnail` only for a ready image, `error_code` only for a failed row), "Attachment
//! Mutability and Deletion" (409 `attachment_locked`), "Attachment Deletion" (soft delete,
//! `attachment_deleted` outbox message in the same transaction, idempotent repeat, uploader check
//! before the idempotency check), the REST error table (404 with the attachment `resource_type`,
//! `invalid_path_params`, attachment `DELETE` outbox failure = 500 `internal`) and "UI rendering
//! flow for chat history" (`AttachmentSummary`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use std::time::Duration;

use attachment_support::{
    ATTACHMENT_CLEANUP_QUEUE, AttRow, AttachmentHarness, AttachmentOptions, Cleanup, PDF,
    PDF_BYTES, StorageOp, assert_problem, ctx_a, id_of, png, uid,
};
use chat_support::{CHAT_PDP_TYPE, USER_A2, assert_not_found, ctx_a2, ctx_b, parse_ts};
use common::{FakePdp, TENANT_A, TENANT_B, USER_B};
use serde_json::{Value, json};
use stream_support::contains_provider_identifier;
use uuid::Uuid;

fn absent_or_null(body: &Value, key: &str) -> bool {
    body.get(key).is_none_or(Value::is_null)
}

async fn recording() -> AttachmentHarness {
    AttachmentHarness::with(AttachmentOptions {
        attachment_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await
}

// ═════════════════════════════════════════════════════════════════════════════
// GET
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn get_returns_the_attachment_detail_of_a_ready_document() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let uploaded = h
        .uploaded(chat, "Quarterly Report.pdf", PDF, PDF_BYTES)
        .await;
    let id = id_of(&uploaded);

    let resp = h.get(chat, id).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(body["id"], json!(id.to_string()));
    assert_eq!(body["filename"], json!("Quarterly Report.pdf"));
    assert_eq!(body["content_type"], json!(PDF));
    assert_eq!(body["size_bytes"], json!(PDF_BYTES.len()));
    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["kind"], json!("document"));
    assert_eq!(
        parse_ts(&body["created_at"]),
        parse_ts(&uploaded["created_at"])
    );
    for key in [
        "error_code",
        "doc_summary",
        "img_thumbnail",
        "summary_updated_at",
    ] {
        assert!(absent_or_null(&body, key), "`{key}` is omitted: {body}");
    }
    let keys: std::collections::BTreeSet<_> = body.as_object().unwrap().keys().cloned().collect();
    assert!(
        keys.is_subset(
            &[
                "id",
                "filename",
                "content_type",
                "size_bytes",
                "status",
                "kind",
                "created_at",
                "error_code",
                "doc_summary",
                "img_thumbnail",
                "summary_updated_at",
            ]
            .iter()
            .map(|k| (*k).to_owned())
            .collect()
        ),
        "only AttachmentDetail fields: {keys:?}"
    );
    assert!(!contains_provider_identifier(&resp.raw), "{}", resp.raw);
}

#[tokio::test]
async fn get_of_a_ready_image_carries_the_thumbnail_object() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let uploaded = h
        .uploaded(chat, "photo.png", "image/png", &png(400, 200))
        .await;
    let body = h.get(chat, id_of(&uploaded)).await.json();

    assert_eq!(body["kind"], json!("image"));
    assert_eq!(body["status"], json!("ready"));
    let thumbnail = &body["img_thumbnail"];
    assert_eq!(thumbnail["content_type"], json!("image/webp"));
    assert_eq!(thumbnail["width"], json!(128));
    assert_eq!(thumbnail["height"], json!(64));
    assert!(
        thumbnail["data_base64"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    assert_eq!(
        thumbnail, &uploaded["img_thumbnail"],
        "the upload response and the GET carry the same thumbnail"
    );
}

#[tokio::test]
async fn get_reports_every_status_and_the_error_code_only_of_a_failed_row() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    for status in ["pending", "uploaded", "ready", "failed"] {
        let row = if status == "failed" {
            AttRow::document(chat, "x.pdf")
                .status(status)
                .error("indexing_failed")
        } else {
            AttRow::document(chat, "x.pdf").status(status)
        };
        let id = h.seed(row).await;
        let body = h.get(chat, id).await.json();
        assert_eq!(body["status"], json!(status));
        if status == "failed" {
            assert_eq!(body["error_code"], json!("indexing_failed"));
        } else {
            assert!(absent_or_null(&body, "error_code"), "{status}: {body}");
        }
    }
}

#[tokio::test]
async fn the_thumbnail_is_only_shown_for_a_ready_image() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let ready = h.seed(AttRow::image(chat, "ready.png")).await;
    assert!(h.get(chat, ready).await.json()["img_thumbnail"].is_object());
    for status in ["pending", "uploaded", "failed"] {
        let id = h
            .seed(AttRow::image(chat, "other.png").status(status))
            .await;
        let body = h.get(chat, id).await.json();
        assert!(absent_or_null(&body, "img_thumbnail"), "{status}: {body}");
    }
}

#[tokio::test]
async fn get_is_404_for_unknown_foreign_and_deleted_attachments() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let other_chat = h.seed_chat().await;
    let in_other_chat = h.seed(AttRow::document(other_chat, "o.pdf")).await;
    // uploaded by another user of the tenant, in the caller's chat
    let by_other_user = h.seed(AttRow::document(chat, "u.pdf").user(USER_A2)).await;
    let deleted = h.seed(AttRow::document(chat, "d.pdf").deleted()).await;
    for id in [uid(0xfeed_f00d), in_other_chat, by_other_user, deleted] {
        let resp = h.get(chat, id).await;
        let problem = assert_not_found(&resp, "attachment");
        assert!(
            !resp.raw.contains("o.pdf") && !resp.raw.contains("u.pdf"),
            "{problem}"
        );
    }
}

#[tokio::test]
async fn get_in_a_chat_the_caller_cannot_see_is_404_for_the_chat() {
    let h = AttachmentHarness::new().await;
    let other_user_chat = h
        .seed_chat_of(USER_A2, TENANT_A, quota_support::PREMIUM)
        .await;
    let other_tenant_chat = h
        .seed_chat_of(USER_B, TENANT_B, quota_support::PREMIUM)
        .await;
    let deleted_chat = h.seed_deleted_chat().await;
    let a = h
        .seed(AttRow::document(other_user_chat, "a.pdf").user(USER_A2))
        .await;
    let b = h
        .seed(
            AttRow::document(other_tenant_chat, "b.pdf")
                .user(USER_B)
                .tenant(TENANT_B),
        )
        .await;
    let c = h.seed(AttRow::document(deleted_chat, "c.pdf")).await;
    for (chat, id) in [
        (other_user_chat, a),
        (other_tenant_chat, b),
        (deleted_chat, c),
        (uid(1), a),
    ] {
        assert_not_found(&h.get(chat, id).await, "chat");
    }
}

#[tokio::test]
async fn a_non_uuid_attachment_or_chat_id_is_400_invalid_path_params() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    for uri in [
        format!("{}/v1/chats/{chat}/attachments/not-a-uuid", h.prefix()),
        format!(
            "{}/v1/chats/not-a-uuid/attachments/{}",
            h.prefix(),
            Uuid::new_v4()
        ),
    ] {
        for method in ["GET", "DELETE"] {
            let resp = h.raw(method, &uri, Some(&ctx_a()), None, vec![]).await;
            let problem = assert_problem(&resp, 400, "invalid_argument");
            chat_support::assert_violation_reason(&problem, "invalid_path_params");
        }
    }
}

#[tokio::test]
async fn get_and_delete_ask_the_pdp_once_for_their_action_on_the_chat() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.seed(AttRow::document(chat, "a.pdf")).await;

    let before = h.pdp.recorded().len();
    assert_eq!(h.get(chat, id).await.status.as_u16(), 200);
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let sent: Vec<_> = h.pdp.recorded().into_iter().skip(before).collect();
    assert_eq!(sent.len(), 2, "one evaluation per call");
    for (request, action) in sent.iter().zip(["read_attachment", "delete_attachment"]) {
        assert_eq!(request.action.name, action);
        assert_eq!(request.resource.resource_type, CHAT_PDP_TYPE);
        assert_eq!(request.resource.id, Some(chat));
        assert!(request.context.require_constraints);
    }
}

async fn refused_by(pdp: std::sync::Arc<FakePdp>, status: u16) {
    let h = AttachmentHarness::with(AttachmentOptions {
        pdp,
        attachment_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let id = h.seed(AttRow::document(chat, "a.pdf")).await;
    for resp in [h.get(chat, id).await, h.delete(chat, id).await] {
        assert_eq!(resp.status.as_u16(), status, "fail-closed: {}", resp.raw);
        assert!(!resp.raw.contains("internal-policy-rule-42") && !resp.raw.contains("10.0.0.7"));
    }
    assert!(
        h.row(id).await.deleted_at.is_none(),
        "a refused delete changes nothing"
    );
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_pdp_denial_is_403_for_get_and_delete() {
    refused_by(FakePdp::deny("internal-policy-rule-42"), 403).await;
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_for_get_and_delete() {
    refused_by(FakePdp::unreachable("pdp-host-10.0.0.7 timed out"), 503).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// DELETE
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn delete_soft_deletes_the_attachment_and_returns_204_without_a_body() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;

    let resp = h.delete(chat, id).await;
    assert_eq!(resp.status.as_u16(), 204, "{}", resp.raw);
    assert!(resp.raw.is_empty());
    let row = h.row(id).await;
    assert!(row.deleted_at.is_some(), "soft delete: the row stays");
    assert_eq!(
        row.cleanup_status.as_deref(),
        Some("pending"),
        "provider cleanup is outstanding"
    );
    assert_eq!(row.status, "ready", "the status is not rewritten");
    assert!(
        row.provider_file_id.is_some(),
        "the provider id stays for the cleanup"
    );

    // gone from the API at once
    assert_not_found(&h.get(chat, id).await, "attachment");
}

#[tokio::test]
async fn delete_enqueues_one_attachment_deleted_message_with_the_documented_payload() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    let provider_file = h.provider_file_of(id).await;

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let payloads = h.attachment_log.wait_for(1).await;
    assert_eq!(payloads.len(), 1);
    let payload = &payloads[0];
    assert_eq!(payload["event_type"], json!("attachment_deleted"));
    assert_eq!(payload["tenant_id"], json!(TENANT_A.to_string()));
    assert_eq!(payload["chat_id"], json!(chat.to_string()));
    assert_eq!(payload["attachment_id"], json!(id.to_string()));
    assert_eq!(payload["provider_file_id"], json!(provider_file));
    assert!(payload["vector_store_id"].is_null(), "always null in P1");
    assert_eq!(payload["storage_backend"], json!("openai"));
    assert_eq!(payload["attachment_kind"], json!("document"));
    assert!(payload["secondary_ref"].is_null());
    parse_ts(&payload["deleted_at"]);
    h.attachment_log.assert_count_stays(1).await;
}

#[tokio::test]
async fn the_message_of_an_image_names_its_kind_and_a_secondary_copy_is_described() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let image = h
        .seed(AttRow::image(chat, "photo.png").secondary("file-anthropic-secondary-01"))
        .await;
    assert_eq!(h.delete(chat, image).await.status.as_u16(), 204);

    let payload = h.attachment_log.wait_for(1).await.remove(0);
    assert_eq!(payload["attachment_kind"], json!("image"));
    let secondary = &payload["secondary_ref"];
    assert_eq!(secondary["file_id"], json!("file-anthropic-secondary-01"));
    assert_eq!(secondary["provider_kind"], json!("anthropic"));
    assert!(secondary["upstream_alias"].is_string(), "{secondary}");
}

#[tokio::test]
async fn a_row_without_a_provider_file_is_deleted_with_a_null_provider_file_id() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h
        .seed(
            AttRow::document(chat, "a.pdf")
                .status("failed")
                .error("upload_failed")
                .provider_file(None),
        )
        .await;
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let payload = h.attachment_log.wait_for(1).await.remove(0);
    assert!(payload["provider_file_id"].is_null(), "{payload}");
    assert_eq!(payload["attachment_id"], json!(id.to_string()));
}

#[tokio::test]
async fn pending_uploaded_and_failed_attachments_can_be_deleted() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    for status in ["pending", "uploaded", "failed"] {
        let id = h.seed(AttRow::document(chat, "x.pdf").status(status)).await;
        assert_eq!(h.delete(chat, id).await.status.as_u16(), 204, "{status}");
        assert!(h.row(id).await.deleted_at.is_some(), "{status}");
    }
    h.attachment_log.wait_for(3).await;
}

#[tokio::test]
async fn deleting_twice_is_idempotent_and_enqueues_once() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;

    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let first = h.row(id).await.deleted_at;
    assert_eq!(
        h.delete(chat, id).await.status.as_u16(),
        204,
        "a repeated DELETE is 204"
    );
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    assert_eq!(
        h.row(id).await.deleted_at,
        first,
        "the first deletion time is kept"
    );
    h.attachment_log.assert_count_stays(1).await;
}

#[tokio::test]
async fn a_repeated_delete_by_another_user_is_404_not_204() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    // an attachment of another user in the caller's chat, already deleted
    let id = h
        .seed(AttRow::document(chat, "u.pdf").user(USER_A2).deleted())
        .await;
    assert_not_found(&h.delete(chat, id).await, "attachment");
    // ... and a live one is 404 as well (the uploader check comes first)
    let live = h.seed(AttRow::document(chat, "v.pdf").user(USER_A2)).await;
    assert_not_found(&h.delete(chat, live).await, "attachment");
    assert!(h.row(live).await.deleted_at.is_none());
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn delete_is_404_for_unknown_and_foreign_attachments_and_for_foreign_chats() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let other_chat = h.seed_chat().await;
    let in_other_chat = h.seed(AttRow::document(other_chat, "o.pdf")).await;
    for id in [uid(0xfeed_f00d), in_other_chat] {
        assert_not_found(&h.delete(chat, id).await, "attachment");
    }
    assert!(h.row(in_other_chat).await.deleted_at.is_none());

    let other_user_chat = h
        .seed_chat_of(USER_A2, TENANT_A, quota_support::PREMIUM)
        .await;
    let foreign = h
        .seed(AttRow::document(other_user_chat, "f.pdf").user(USER_A2))
        .await;
    assert_not_found(&h.delete(other_user_chat, foreign).await, "chat");
    assert_not_found(
        &h.delete_attachment(other_user_chat, foreign, &ctx_b())
            .await,
        "chat",
    );
    assert_not_found(&h.delete_attachment(chat, foreign, &ctx_a2()).await, "chat");
    assert!(h.row(foreign).await.deleted_at.is_none());
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn delete_in_a_deleted_chat_is_404() {
    let h = recording().await;
    let chat = h.seed_deleted_chat().await;
    let id = h.seed(AttRow::document(chat, "a.pdf")).await;
    assert_not_found(&h.delete(chat, id).await, "chat");
    assert!(h.row(id).await.deleted_at.is_none());
}

#[tokio::test]
async fn an_attachment_referenced_by_a_submitted_message_is_409_attachment_locked() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    let sent = h.say_with(chat, "see the file", &[id]).await;
    assert_eq!(sent.terminal().event, "done");

    let resp = h.delete(chat, id).await;
    let problem = assert_problem(&resp, 409, "already_exists");
    assert_eq!(
        problem["context"]["resource_name"],
        json!("attachment_locked")
    );
    assert_eq!(
        problem["context"]["resource_type"],
        json!("gts.cf.core.mini_chat.attachment.v1~")
    );
    let row = h.row(id).await;
    assert!(
        row.deleted_at.is_none() && row.cleanup_status.is_none(),
        "nothing changed"
    );
    h.attachment_log.assert_count_stays(0).await;
    assert_eq!(h.get(chat, id).await.status.as_u16(), 200, "still readable");
    // idempotent refusal
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 409);
}

#[tokio::test]
async fn an_attachment_not_yet_sent_can_be_removed_while_another_one_is_locked() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let locked = h.doc(chat, "sent.pdf").await;
    let free = h.doc(chat, "free.pdf").await;
    h.say_with(chat, "q", &[locked]).await;

    assert_eq!(h.delete(chat, free).await.status.as_u16(), 204);
    assert_eq!(h.delete(chat, locked).await.status.as_u16(), 409);
    // a deleted attachment can no longer be attached
    let resp = h.reject_with(chat, "again", &[free]).await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    chat_support::assert_field_violation(&problem, "attachment", "invalid_attachment");
}

#[tokio::test]
async fn the_locked_rule_follows_the_link_not_the_message_status() {
    let h = recording().await;
    let chat = h.seed_chat().await;
    let id = h.seed(AttRow::document(chat, "a.pdf")).await;
    h.submit_with(chat, id).await;
    let resp = h.delete(chat, id).await;
    let problem = assert_problem(&resp, 409, "already_exists");
    assert_eq!(
        problem["context"]["resource_name"],
        json!("attachment_locked")
    );
}

#[tokio::test]
async fn delete_returns_without_waiting_for_the_provider() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    h.storage.hold(StorageOp::DeleteFile);

    let resp = tokio::time::timeout(Duration::from_secs(5), h.delete(chat, id))
        .await
        .expect("DELETE must not wait for the provider cleanup");
    assert_eq!(resp.status.as_u16(), 204);
    assert_eq!(h.row(id).await.cleanup_status.as_deref(), Some("pending"));
    h.storage.wait_paused(1).await;
    h.storage.release();
    h.wait_cleanup(id, "done").await;
}

#[tokio::test]
async fn deleting_an_attachment_never_touches_the_vector_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    h.wait_cleanup(id, "done").await;

    assert_eq!(h.storage.count(StorageOp::DeleteFile), 1);
    assert_eq!(
        h.storage.count(StorageOp::DeleteVectorStore),
        0,
        "the store lives until the chat goes"
    );
    assert_eq!(h.stores_of(chat).await.len(), 1);
}

#[tokio::test]
async fn a_failed_enqueue_is_500_and_rolls_the_delete_back() {
    let h = AttachmentHarness::with(AttachmentOptions {
        attachment_cleanup: Cleanup::Missing,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;

    let resp = h.delete(chat, id).await;
    assert_problem(&resp, 500, "internal");
    assert!(
        !resp.raw.contains(ATTACHMENT_CLEANUP_QUEUE),
        "no queue name on the wire: {}",
        resp.raw
    );
    let row = h.row(id).await;
    assert!(
        row.deleted_at.is_none() && row.cleanup_status.is_none(),
        "the soft delete rolled back"
    );
    assert_eq!(h.get(chat, id).await.status.as_u16(), 200);
}

// ═════════════════════════════════════════════════════════════════════════════
// attachment summaries of the message list
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_message_list_summarises_the_attachments_of_a_submitted_message() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.doc(chat, "notes.pdf").await;
    let image = h.image(chat, "photo.png").await;
    let sent = h.say_with(chat, "look at these", &[doc, image]).await;
    assert_eq!(sent.terminal().event, "done");

    let items = h.list_messages(chat).await;
    let user = items
        .iter()
        .find(|m| m["role"] == json!("user"))
        .expect("user message");
    let summaries = user["attachments"].as_array().expect("attachments[]");
    assert_eq!(summaries.len(), 2, "{summaries:?}");
    let doc_summary = summaries
        .iter()
        .find(|s| s["attachment_id"] == json!(doc.to_string()))
        .expect("the document is summarised");
    assert_eq!(doc_summary["kind"], json!("document"));
    assert_eq!(doc_summary["filename"], json!("notes.pdf"));
    assert_eq!(doc_summary["status"], json!("ready"));
    assert!(absent_or_null(doc_summary, "img_thumbnail"));
    let image_summary = summaries
        .iter()
        .find(|s| s["attachment_id"] == json!(image.to_string()))
        .expect("the image is summarised");
    assert_eq!(image_summary["kind"], json!("image"));
    assert_eq!(
        image_summary["img_thumbnail"]["content_type"],
        json!("image/webp")
    );
    assert!(!contains_provider_identifier(
        &serde_json::to_string(summaries).unwrap()
    ));
    let assistant = items
        .iter()
        .find(|m| m["role"] == json!("assistant"))
        .unwrap();
    assert_eq!(
        assistant["attachments"],
        json!([]),
        "only the user message carries attachments"
    );
}

#[tokio::test]
async fn a_message_without_attachments_lists_an_empty_array() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.say(chat, "plain").await;
    for message in h.list_messages(chat).await {
        assert_eq!(message["attachments"], json!([]));
    }
}

#[tokio::test]
async fn attachments_are_linked_once_per_message_and_keep_their_tenant_and_chat() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let a = h.doc(chat, "a.pdf").await;
    let b = h.doc(chat, "b.pdf").await;
    let sent = h.say_with(chat, "q", &[a, b]).await;
    let user_message = h.user_message_of(chat, sent.request_id()).await;

    let links = h.links_of_chat(chat).await;
    assert_eq!(links.len(), 2);
    for link in &links {
        assert_eq!(link.tenant_id, TENANT_A);
        assert_eq!(link.message_id, user_message);
    }
    let mut linked: Vec<_> = links.iter().map(|l| l.attachment_id).collect();
    linked.sort();
    let mut expected = vec![a, b];
    expected.sort();
    assert_eq!(linked, expected);
}

#[tokio::test]
async fn a_message_can_not_reference_foreign_pending_duplicate_or_unknown_attachments() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let other_chat = h.seed_chat().await;
    let ready = h.doc(chat, "ok.pdf").await;
    let in_other_chat = h.doc(other_chat, "o.pdf").await;
    let by_other_user = h.seed(AttRow::document(chat, "u.pdf").user(USER_A2)).await;
    let pending = h
        .seed(AttRow::document(chat, "p.pdf").status("pending"))
        .await;
    let uploaded = h
        .seed(AttRow::document(chat, "up.pdf").status("uploaded"))
        .await;
    let deleted = h.seed(AttRow::document(chat, "d.pdf").deleted()).await;

    let before = h.row_counts().await;
    for ids in [
        vec![in_other_chat],
        vec![by_other_user],
        vec![pending],
        vec![uploaded],
        vec![deleted],
        vec![uid(0xabcd)],
        vec![ready, ready],
        vec![ready, pending],
    ] {
        let resp = h.reject_with(chat, "q", &ids).await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        chat_support::assert_field_violation(&problem, "attachment", "invalid_attachment");
    }
    assert_eq!(
        h.row_counts().await,
        before,
        "a rejection leaves nothing behind"
    );
    h.oagw.assert_no_provider_call();
    // the ready one is fine on its own
    assert_eq!(
        h.say_with(chat, "q", &[ready]).await.terminal().event,
        "done"
    );
}
