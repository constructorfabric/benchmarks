//! The upload checks before and at the multipart intake over the router: admission (404 masking,
//! 403, 503) and the chat model and storage resolution before any body byte is read, the
//! concurrency limit, the multipart shape errors, unsupported types, the code-interpreter routing
//! and the `disable_images` kill switch. Every refusal leaves no row, no provider call and no
//! leaked permit; a refusal after the permit was taken records one `error` upload sample.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flows
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document` and `-upload-image`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake`,
//! `-purpose-routing`, `-upload-completion` and `-error-contract`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-intake` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::Ordering;

use axum::body::Body;
use serde_json::Value;

use crate::config::RagConfig;
use crate::test_support::attachments::{
    ATTACHMENT_RESOURCE, AttachRigOptions, FilePart, NO_CI_MODEL, ORPHAN_MODEL, PDF, XLSX,
    assert_created, catalog_with, pdf_bytes, png, rig, rig_with, switches, tracked_body,
    upload_request, upload_request_raw,
};
use crate::test_support::chats::{
    CHAT_RESOURCE, a1, a2, assert_chat_not_found, assert_invalid_argument, assert_problem, b1,
    seed_chat,
};
use crate::test_support::provider::Gate;
use crate::test_support::storage_mock::{Answer, Route};
use crate::test_support::{Pdp, raw_exec};

fn assert_error_sample(rig: &crate::test_support::attachments::AttachRig, kind: &str) {
    assert_eq!(
        rig.uploads(),
        vec![(kind.to_owned(), "error".to_owned())],
        "one error sample for the request"
    );
    assert_eq!(
        rig.permits.available(),
        usize::from(rig.rag.max_concurrent_uploads),
        "a permit leaked"
    );
}

// ── before the body is read ──────────────────────────────────────────────────

#[tokio::test]
async fn a_foreign_chat_is_404_with_the_chat_type_before_any_body_byte_is_read() {
    // inst-doc-return-refusal: 404 not_found with the chat resource_type
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let (body, polled) = tracked_body(&[FilePart::file("a.pdf", PDF, &pdf_bytes())]);
    let who = a2();
    let reply = rig
        .send(upload_request_raw(
            chat.id,
            &who,
            Some("multipart/form-data; boundary=XBOUNDARYX"),
            body,
        ))
        .await;
    let problem = assert_chat_not_found(&reply);
    assert_eq!(problem["context"]["resource_type"], CHAT_RESOURCE);
    assert!(
        !polled.load(Ordering::SeqCst),
        "the body was read before admission"
    );
    rig.assert_untouched(chat.id).await;
    assert!(
        rig.uploads().is_empty(),
        "no permit was taken, so no sample"
    );
}

#[tokio::test]
async fn another_tenants_chat_a_missing_chat_and_a_deleted_chat_are_the_same_404() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let deleted = rig.std_chat().await;
    raw_exec(
        &rig.db.raw,
        &format!(
            "UPDATE chats SET deleted_at = '2026-01-01 00:00:00+00:00' WHERE id = {}",
            crate::test_support::chats::blob(deleted.id)
        ),
    )
    .await
    .unwrap();
    let replies = vec![
        rig.upload_as(&b1(), chat.id, "a.pdf", PDF, &pdf_bytes())
            .await,
        rig.upload(uuid::Uuid::new_v4(), "a.pdf", PDF, &pdf_bytes())
            .await,
        rig.upload(deleted.id, "a.pdf", PDF, &pdf_bytes()).await,
    ];
    for reply in &replies {
        assert_chat_not_found(reply);
    }
    assert_eq!(rig.storage.call_count(), 0);
    assert!(rig.uploads().is_empty());
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_400_invalid_path_params() {
    // inst-err-path
    let rig = rig().await;
    let mut request = upload_request_raw(
        uuid::Uuid::nil(),
        &a1(),
        Some("multipart/form-data; boundary=XBOUNDARYX"),
        Body::empty(),
    );
    *request.uri_mut() = "/mini-chat/v1/chats/not-a-uuid/attachments"
        .parse()
        .unwrap();
    let reply = rig.send(request).await;
    let problem = assert_problem(&reply, 400, "invalid_argument");
    assert!(
        problem.to_string().contains("invalid_path_params"),
        "{problem}"
    );
    assert_eq!(rig.storage.call_count(), 0);
}

#[tokio::test]
async fn the_pdp_is_asked_for_upload_attachment_on_the_chat() {
    // inst-doc-admission: action upload_attachment with the chat id as resource.id
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "upload_attachment");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_pdp_deny_is_403_authz_denied_before_the_body_is_read() {
    // inst-doc-refused
    let rig = rig_with(AttachRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = rig.std_chat().await;
    let (body, polled) = tracked_body(&[FilePart::file("a.pdf", PDF, &pdf_bytes())]);
    let reply = rig
        .send(upload_request_raw(
            chat.id,
            &a1(),
            Some("multipart/form-data; boundary=XBOUNDARYX"),
            body,
        ))
        .await;
    let problem = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(problem["context"]["reason"], "AUTHZ_DENIED");
    assert!(!polled.load(Ordering::SeqCst));
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_with_retry_after_5() {
    let rig = rig_with(AttachRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_chat_model_that_left_the_catalog_is_400_invalid_model_before_the_body_is_read() {
    // inst-intake-return-invalid-model
    let rig = rig().await;
    let chat = seed_chat(
        &rig.db,
        &a1(),
        uuid::Uuid::new_v4(),
        Some("gone"),
        "removed-model-1",
        crate::test_support::chats::at(0),
    )
    .await;
    let (body, polled) = tracked_body(&[FilePart::file("a.pdf", PDF, &pdf_bytes())]);
    let reply = rig
        .send(upload_request_raw(
            chat.id,
            &a1(),
            Some("multipart/form-data; boundary=XBOUNDARYX"),
            body,
        ))
        .await;
    assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
    assert!(!polled.load(Ordering::SeqCst));
    rig.assert_untouched(chat.id).await;
    assert!(rig.uploads().is_empty(), "no permit was taken yet");
}

#[tokio::test]
async fn a_model_without_a_resolvable_storage_provider_is_500_internal_before_the_body_is_read() {
    // inst-intake-return-storage: generic detail, no fallback provider
    let rig = rig().await;
    let chat = rig.chat(ORPHAN_MODEL).await;
    let (body, polled) = tracked_body(&[FilePart::file("a.pdf", PDF, &pdf_bytes())]);
    let reply = rig
        .send(upload_request_raw(
            chat.id,
            &a1(),
            Some("multipart/form-data; boundary=XBOUNDARYX"),
            body,
        ))
        .await;
    let problem = assert_problem(&reply, 500, "internal");
    assert!(!problem.to_string().contains("ghost"), "{problem}");
    assert!(!polled.load(Ordering::SeqCst));
    rig.assert_untouched(chat.id).await;
    assert!(rig.uploads().is_empty());
}

#[tokio::test]
async fn a_policy_plugin_failure_is_500_internal_without_a_fallback() {
    // inst-intake-return-policy
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.gateway.fail_current.store(true, Ordering::SeqCst);
    let reply = rig.upload_pdf(chat.id).await;
    let problem = assert_problem(&reply, 500, "internal");
    assert!(
        !problem.to_string().contains("provider-secret-detail"),
        "{problem}"
    );
    rig.assert_untouched(chat.id).await;
    assert!(rig.uploads().is_empty());
}

// ── the concurrency limit ────────────────────────────────────────────────────

#[tokio::test]
async fn the_eleventh_concurrent_upload_is_503_with_retry_after_5() {
    // inst-intake-return-busy: the default limit is 10; no Files request for the refused one
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.set_default(
        Route::FilesUpload,
        Answer::gated(&gate, Answer::ok(&serde_json::json!({ "id": "file-held" }))),
    );
    let mut held = Vec::new();
    for n in 0..10 {
        let name = format!("held-{n}.xlsx");
        held.push(rig.spawn(upload_request(
            chat.id,
            &a1(),
            &[FilePart::file(&name, XLSX, b"PK")],
        )));
    }
    rig.storage.wait_for(Route::FilesUpload, 10).await;
    assert_eq!(rig.permits.available(), 0);
    let refused = rig.upload(chat.id, "eleventh.xlsx", XLSX, b"PK").await;
    let problem = assert_problem(&refused, 503, "service_unavailable");
    assert_eq!(refused.retry_after.as_deref(), Some("5"));
    assert_eq!(problem["context"]["retry_after_seconds"], 5);
    assert_eq!(
        rig.storage.count_of(Route::FilesUpload),
        10,
        "the refused upload reached the provider"
    );
    assert_eq!(
        rig.rows(chat.id).await.len(),
        10,
        "the refused upload wrote a row"
    );
    gate.open();
    for handle in held {
        assert_created(&handle.await.unwrap(), "ready");
    }
    assert_eq!(rig.permits.available(), 10);
    // the refused request took no permit, so it recorded no sample: ten `ok` samples only
    assert_eq!(rig.uploads().len(), 10);
    assert!(rig.uploads().iter().all(|(_, result)| result == "ok"));
}

#[tokio::test]
async fn a_busy_limit_is_reported_after_admission_so_a_foreign_chat_stays_404() {
    let rig = rig_with(AttachRigOptions::new().with_rag(RagConfig {
        max_concurrent_uploads: 1,
        ..RagConfig::default()
    }))
    .await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.set_default(
        Route::FilesUpload,
        Answer::gated(&gate, Answer::ok(&serde_json::json!({ "id": "file-held" }))),
    );
    let held = rig.spawn(upload_request(
        chat.id,
        &a1(),
        &[FilePart::file("a.xlsx", XLSX, b"PK")],
    ));
    rig.storage.wait_for(Route::FilesUpload, 1).await;
    let foreign = rig.upload_as(&a2(), chat.id, "b.xlsx", XLSX, b"PK").await;
    assert_chat_not_found(&foreign);
    let own = rig.upload(chat.id, "c.xlsx", XLSX, b"PK").await;
    assert_problem(&own, 503, "service_unavailable");
    gate.open();
    assert_created(&held.await.unwrap(), "ready");
}

// ── the multipart shape ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_content_type_without_a_boundary_is_400_boundary_required() {
    // inst-intake-multipart; the permit was taken, so one error sample
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for content_type in [Some("multipart/form-data"), Some("application/json"), None] {
        let reply = rig
            .send(upload_request_raw(
                chat.id,
                &a1(),
                content_type,
                Body::from("irrelevant"),
            ))
            .await;
        assert_invalid_argument(&reply, 400, "content_type", "BOUNDARY_REQUIRED");
    }
    assert_eq!(rig.uploads().len(), 3);
    assert!(rig.rows(chat.id).await.is_empty());
    assert_eq!(rig.storage.call_count(), 0);
    assert_eq!(rig.permits.available(), 10);
}

#[tokio::test]
async fn an_unreadable_body_is_400_multipart_error() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let broken = futures::stream::iter(vec![Err::<bytes::Bytes, std::io::Error>(
        std::io::Error::other("connection reset"),
    )]);
    let reply = rig
        .send(upload_request_raw(
            chat.id,
            &a1(),
            Some("multipart/form-data; boundary=XBOUNDARYX"),
            Body::from_stream(broken),
        ))
        .await;
    assert_invalid_argument(&reply, 400, "multipart", "MULTIPART_ERROR");
    assert_error_sample(&rig, "document");
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_body_without_a_file_part_is_400_missing_file() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .send(upload_request(
            chat.id,
            &a1(),
            &[FilePart::field("note", "hello")],
        ))
        .await;
    assert_invalid_argument(&reply, 400, "file", "MISSING_FILE");
    assert_error_sample(&rig, "document");
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn an_empty_multipart_is_400_missing_file() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.send(upload_request(chat.id, &a1(), &[])).await;
    assert_invalid_argument(&reply, 400, "file", "MISSING_FILE");
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_file_part_without_a_content_type_is_400_missing_content_type() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .send(upload_request(
            chat.id,
            &a1(),
            &[FilePart::untyped("a.pdf", &pdf_bytes())],
        ))
        .await;
    assert_invalid_argument(&reply, 400, "content_type", "MISSING_CONTENT_TYPE");
    assert_error_sample(&rig, "document");
    rig.assert_untouched(chat.id).await;
}

// ── types and routing ────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unsupported_type_is_400_unsupported_content_type_and_never_415() {
    // inst-intake-return-unsupported: no row, no file byte read
    let rig = rig().await;
    let chat = rig.std_chat().await;
    for (filename, mime) in [
        ("a.zip", "application/zip"),
        ("a.mp4", "video/mp4"),
        ("a.svg", "image/svg+xml"),
        ("blob.bin", "application/octet-stream"),
    ] {
        let reply = rig.upload(chat.id, filename, mime, b"bytes").await;
        assert_invalid_argument(&reply, 400, "content_type", "UNSUPPORTED_CONTENT_TYPE");
    }
    assert_eq!(rig.uploads().len(), 4);
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn an_xlsx_is_400_code_interpreter_unavailable_while_the_kill_switch_is_on() {
    // inst-route-return-ci-unavailable: attachment resource type, no row
    let rig =
        rig_with(AttachRigOptions::new().with_snapshot(catalog_with(switches(false, true, false))))
            .await;
    let chat = rig.std_chat().await;
    let reply = rig.upload(chat.id, "sheet.xlsx", XLSX, b"PK").await;
    let problem = assert_invalid_argument(&reply, 400, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(problem["context"]["resource_type"], ATTACHMENT_RESOURCE);
    assert_error_sample(&rig, "document");
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn an_xlsx_is_400_code_interpreter_unavailable_on_a_model_without_support() {
    let rig = rig().await;
    let chat = rig.chat(NO_CI_MODEL).await;
    let reply = rig.upload(chat.id, "sheet.xlsx", XLSX, b"PK").await;
    let problem = assert_invalid_argument(&reply, 400, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(problem["context"]["resource_type"], ATTACHMENT_RESOURCE);
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_pdf_still_uploads_on_a_model_without_code_interpreter_support() {
    let rig = rig().await;
    let chat = rig.chat(NO_CI_MODEL).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn an_image_is_400_feature_disabled_while_disable_images_is_on() {
    // inst-intake-return-images-disabled: no vision check at upload, no file byte read
    let rig =
        rig_with(AttachRigOptions::new().with_snapshot(catalog_with(switches(true, false, false))))
            .await;
    let chat = rig.std_chat().await;
    let reply = rig.upload(chat.id, "a.png", "image/png", &png(8, 8)).await;
    let problem = assert_problem(&reply, 400, "failed_precondition");
    let violation = &problem["context"]["violations"][0];
    assert_eq!(violation["subject"], "images");
    assert_eq!(violation["type"], "FEATURE_DISABLED");
    assert_error_sample(&rig, "image");
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_document_still_uploads_while_disable_images_is_on() {
    let rig =
        rig_with(AttachRigOptions::new().with_snapshot(catalog_with(switches(true, false, false))))
            .await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
}

#[tokio::test]
async fn a_failed_intake_never_echoes_the_filename_or_provider_text() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "secret-plan.zip", "application/zip", b"x")
        .await;
    let body: Value = reply.json();
    assert!(!body.to_string().contains("secret-plan"), "{body}");
}
