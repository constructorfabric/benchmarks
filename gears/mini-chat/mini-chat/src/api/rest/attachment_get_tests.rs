//! `GET /v1/chats/{id}/attachments/{attachment_id}` over the router: the `AttachmentDetail`
//! projection (optional fields omitted when null, `error_code` only for `failed`, the thumbnail
//! only for a `ready` image, no summary or provider field), polling of an `uploaded` row, the
//! admission refusals and the identical 404 with the attachment type for an unknown,
//! soft-deleted, other-chat, other-uploader and other-tenant attachment.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-get-attachment`; algorithm
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection`; definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-get-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::Engine as _;
use serde_json::Value;
use uuid::Uuid;

use crate::test_support::attachments::{
    ATTACHMENT_RESOURCE, AttachRigOptions, RowSeed, assert_no_provider_text, key_set, rig,
    rig_with, seed_row,
};
use crate::test_support::chats::{
    CHAT_RESOURCE, a1, a2, assert_chat_not_found, assert_problem, at, b1, request,
};
use crate::test_support::{Pdp, TENANT_B, USER_A2};

const REQUIRED: [&str; 7] = [
    "id",
    "filename",
    "content_type",
    "size_bytes",
    "status",
    "kind",
    "created_at",
];

fn keys(names: &[&str]) -> std::collections::BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn assert_attachment_not_found(reply: &crate::test_support::chats::Reply) -> Value {
    let body = assert_problem(reply, 404, "not_found");
    assert_eq!(
        body["context"]["resource_type"], ATTACHMENT_RESOURCE,
        "{body}"
    );
    body
}

// ── 200 ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_ready_document_is_200_with_the_required_fields_only() {
    // inst-get-return-200, inst-proj-required, inst-proj-omit-null
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document().size(4321)).await;
    let reply = rig.get_attachment(chat.id, id).await;
    assert_eq!(reply.status.as_u16(), 200, "{}", reply.text);
    assert_eq!(reply.content_type, "application/json");
    let body = reply.json();
    assert_eq!(key_set(&body), keys(&REQUIRED), "{body}");
    assert_eq!(body["id"], id.to_string());
    assert_eq!(body["filename"], "seed.pdf");
    assert_eq!(body["content_type"], "application/pdf");
    assert_eq!(body["size_bytes"], 4321);
    assert_eq!(body["status"], "ready");
    assert_eq!(body["kind"], "document");
}

#[tokio::test]
async fn the_summary_fields_are_never_present() {
    // ADR-0007: doc_summary and summary_updated_at are never populated
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    let body = rig.get_attachment(chat.id, id).await.json();
    assert!(body.get("doc_summary").is_none());
    assert!(body.get("summary_updated_at").is_none());
}

#[tokio::test]
async fn no_provider_or_internal_field_is_returned() {
    // inst-proj-no-internal
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let seed = RowSeed::image().thumbnail(128, 64, &[1, 2, 3]);
    let provider_file_id = seed.provider_file_id.clone().unwrap();
    let id = seed_row(&rig.db, &chat, seed).await;
    let reply = rig.get_attachment(chat.id, id).await;
    assert_no_provider_text(&reply.text);
    assert!(!reply.text.contains(&provider_file_id));
    let body = reply.json();
    for key in [
        "provider_file_id",
        "secondary_file_id",
        "storage_backend",
        "for_file_search",
        "for_code_interpreter",
        "cleanup_status",
        "uploaded_by_user_id",
        "tenant_id",
        "chat_id",
        "vector_store_id",
    ] {
        assert!(body.get(key).is_none(), "`{key}` in {body}");
    }
}

#[tokio::test]
async fn error_code_appears_only_for_a_failed_row() {
    // inst-proj-error-code
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let failed = seed_row(
        &rig.db,
        &chat,
        RowSeed::document()
            .status("failed")
            .error("upload_abandoned"),
    )
    .await;
    let ready = seed_row(&rig.db, &chat, RowSeed::document().error("stale_code")).await;
    let uploaded = seed_row(&rig.db, &chat, RowSeed::document().status("uploaded")).await;
    let failed_body = rig.get_attachment(chat.id, failed).await.json();
    assert_eq!(failed_body["status"], "failed");
    assert_eq!(failed_body["error_code"], "upload_abandoned");
    for id in [ready, uploaded] {
        let body = rig.get_attachment(chat.id, id).await.json();
        assert!(body.get("error_code").is_none(), "{body}");
    }
}

#[tokio::test]
async fn a_ready_image_with_a_stored_thumbnail_carries_it_as_a_webp_object() {
    // inst-proj-thumbnail
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().thumbnail(128, 64, &[9, 8, 7]),
    )
    .await;
    let body = rig.get_attachment(chat.id, id).await.json();
    assert_eq!(body["kind"], "image");
    let thumbnail = &body["img_thumbnail"];
    assert_eq!(thumbnail["content_type"], "image/webp");
    assert_eq!(thumbnail["width"], 128);
    assert_eq!(thumbnail["height"], 64);
    assert_eq!(
        thumbnail["data_base64"],
        base64::engine::general_purpose::STANDARD.encode([9, 8, 7])
    );
}

#[tokio::test]
async fn the_thumbnail_is_omitted_unless_the_row_is_a_ready_image_with_bytes() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let without = seed_row(&rig.db, &chat, RowSeed::image()).await;
    let uploaded = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().status("uploaded").thumbnail(8, 8, &[1]),
    )
    .await;
    let failed = seed_row(
        &rig.db,
        &chat,
        RowSeed::image().status("failed").thumbnail(8, 8, &[1]),
    )
    .await;
    let document = seed_row(&rig.db, &chat, RowSeed::document().thumbnail(8, 8, &[1])).await;
    for id in [without, uploaded, failed, document] {
        let body = rig.get_attachment(chat.id, id).await.json();
        assert!(body.get("img_thumbnail").is_none(), "{body}");
    }
}

#[tokio::test]
async fn a_pending_and_an_uploaded_row_are_visible_by_polling() {
    // the client polls until the status is ready or failed
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for status in ["pending", "uploaded"] {
        let seed = if status == "pending" {
            RowSeed::document().status("pending")
        } else {
            RowSeed::document().status("uploaded")
        };
        let id = seed_row(&rig.db, &chat, seed).await;
        for _ in 0..3 {
            let reply = rig.get_attachment(chat.id, id).await;
            assert_eq!(reply.status.as_u16(), 200);
            assert_eq!(reply.json()["status"], status);
        }
    }
}

#[tokio::test]
async fn get_makes_no_provider_call() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    rig.get_attachment(chat.id, id).await;
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn the_upload_response_and_the_get_response_agree() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let created = rig.upload_pdf(chat.id).await.json();
    let id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    let fetched = rig.get_attachment(chat.id, id).await.json();
    assert_eq!(created, fetched);
}

// ── admission ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_pdp_is_asked_for_read_attachment_on_the_chat() {
    // inst-get-admission
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    rig.get_attachment(chat.id, id).await;
    let evaluated = rig.pdp.only_request();
    assert_eq!(evaluated.action.name, "read_attachment");
    assert_eq!(evaluated.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_deny_is_403_and_an_unreachable_pdp_is_503() {
    let denied = rig_with(AttachRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = denied.std_chat().await;
    let id = seed_row(&denied.db, &chat, RowSeed::document()).await;
    let reply = denied.get_attachment(chat.id, id).await;
    assert_eq!(
        assert_problem(&reply, 403, "permission_denied")["context"]["reason"],
        "AUTHZ_DENIED"
    );

    let down = rig_with(AttachRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = down.std_chat().await;
    let id = seed_row(&down.db, &chat, RowSeed::document()).await;
    let reply = down.get_attachment(chat.id, id).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
}

#[tokio::test]
async fn a_foreign_missing_or_other_tenant_chat_is_404_with_the_chat_type() {
    // inst-get-return-404 is for attachments; the chat is masked by admission first
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = seed_row(&rig.db, &chat, RowSeed::document()).await;
    for reply in [
        rig.get_attachment_as(&a2(), chat.id, id).await,
        rig.get_attachment_as(&b1(), chat.id, id).await,
        rig.get_attachment(Uuid::new_v4(), id).await,
    ] {
        let body = assert_chat_not_found(&reply);
        assert_eq!(body["context"]["resource_type"], CHAT_RESOURCE);
    }
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
        let reply = rig.send(request("GET", &uri, &a1())).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        assert!(
            body.to_string().contains("invalid_path_params"),
            "{uri}: {body}"
        );
    }
}

// ── the 404 of the attachment ────────────────────────────────────────────────

#[tokio::test]
async fn unknown_deleted_other_chat_other_uploader_and_other_tenant_attachments_are_the_same_404() {
    // inst-get-missing: the same response for all four cases (plus the foreign tenant)
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let other_chat = rig.std_chat().await;
    let in_other_chat = seed_row(&rig.db, &other_chat, RowSeed::document()).await;
    let deleted = seed_row(&rig.db, &chat, RowSeed::document().deleted(at(5))).await;
    let colleague = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    let foreign_tenant = seed_row(&rig.db, &chat, RowSeed::document().tenant(TENANT_B)).await;
    let mut bodies = Vec::new();
    for id in [
        Uuid::new_v4(),
        deleted,
        in_other_chat,
        colleague,
        foreign_tenant,
    ] {
        let reply = rig.get_attachment(chat.id, id).await;
        let mut body = assert_attachment_not_found(&reply);
        // the echoed resource name is the requested id; everything else is identical
        body["instance"] = Value::Null;
        body["trace_id"] = Value::Null;
        body["context"]["resource_name"] = Value::Null;
        bodies.push(body);
    }
    for body in &bodies[1..] {
        assert_eq!(body, &bodies[0]);
    }
}

#[tokio::test]
async fn the_404_never_reveals_why_the_attachment_is_hidden() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let colleague = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    let reply = rig.get_attachment(chat.id, colleague).await;
    let rendered = reply.text.to_lowercase();
    for word in [
        "forbidden",
        "uploader",
        "another user",
        "soft-deleted",
        "tenant",
    ] {
        assert!(!rendered.contains(word), "`{word}` in {rendered}");
    }
}

#[tokio::test]
async fn an_attachment_uploaded_by_the_caller_in_a_chat_of_another_user_is_not_reachable() {
    // owner scoping comes from the parent chat
    let rig = rig().await;
    let chat = rig.std_chat().await; // owned by user 1
    let id = seed_row(&rig.db, &chat, RowSeed::document().uploader(USER_A2)).await;
    let reply = rig.get_attachment_as(&a2(), chat.id, id).await;
    assert_chat_not_found(&reply);
}
