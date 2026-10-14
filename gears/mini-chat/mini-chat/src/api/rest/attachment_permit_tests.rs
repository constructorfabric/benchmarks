//! The upload concurrency permit and the upload metric on every return path of the upload flows:
//! 201 `ready`, 201 `uploaded` (background indexing still running), the 429 limits, an intake
//! Problem after the permit was taken, an in-request failure, a database failure mapped to 500, a
//! dropped request, and sequential uploads beyond `rag.max_concurrent_uploads`. One
//! `mini_chat_attachment_upload` sample per request that took a permit, `ok` for each 201 and
//! `error` otherwise; also over the real OTLP instruments.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flows
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document` and `-upload-image`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion` and
//! `-metrics`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use serde_json::json;

use crate::config::RagConfig;
use crate::test_support::attachments::{
    AttachRig, AttachRigOptions, FilePart, IN_REQUEST_READS, PDF, RowSeed, assert_created,
    fail_attachment_inserts, pdf_bytes, png, rig, rig_with, seed_documents, seed_row,
    upload_request,
};
use crate::test_support::chats::a1;
use crate::test_support::provider::Gate;
use crate::test_support::storage_mock::{Answer, Route};

fn sample(kind: &str, result: &str) -> (String, String) {
    (kind.to_owned(), result.to_owned())
}

fn assert_free(rig: &AttachRig) {
    assert_eq!(
        rig.permits.available(),
        usize::from(rig.rag.max_concurrent_uploads),
        "a permit leaked"
    );
}

#[tokio::test]
async fn a_201_ready_releases_the_permit_and_records_ok_and_the_byte_count() {
    // inst-doc-metrics
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_free(&rig);
    assert_eq!(rig.uploads(), vec![sample("document", "ok")]);
    assert_eq!(
        rig.metrics.upload_bytes(),
        vec![("document".to_owned(), pdf_bytes().len() as u64)]
    );
}

#[tokio::test]
async fn a_201_uploaded_releases_the_permit_and_records_ok() {
    // inst-doc-complete-uploaded: the permit is released before the response is returned
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage.queue_n(
        Route::VsReadFile,
        &Answer::indexing("in_progress"),
        IN_REQUEST_READS,
    );
    rig.storage.queue(
        Route::VsReadFile,
        Answer::gated(&gate, Answer::indexing("completed")),
    );
    assert_created(&rig.upload_pdf(chat.id).await, "uploaded");
    assert_free(&rig);
    assert_eq!(rig.uploads(), vec![sample("document", "ok")]);
    gate.open();
}

#[tokio::test]
async fn the_429_limits_release_the_permit_and_record_error() {
    // inst-doc-return-limit, inst-img-return-limit
    let rig = rig().await;
    let chat = rig.std_chat().await;
    seed_documents(&rig.db, &chat, 50, 10).await;
    rig.upload_pdf(chat.id).await;
    assert_free(&rig);
    let storage_chat = rig.std_chat().await;
    seed_row(
        &rig.db,
        &storage_chat,
        RowSeed::document().size(100 * 1_048_576),
    )
    .await;
    rig.upload(storage_chat.id, "a.png", "image/png", &png(8, 8))
        .await;
    assert_free(&rig);
    assert_eq!(
        rig.uploads(),
        vec![sample("document", "error"), sample("image", "error")]
    );
}

#[tokio::test]
async fn an_intake_problem_after_the_permit_releases_it_and_records_error() {
    // inst-intake-permit-rule: MISSING_FILE, UNSUPPORTED_CONTENT_TYPE, ...
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.send(upload_request(
        chat.id,
        &a1(),
        &[FilePart::field("note", "x")],
    ))
    .await;
    rig.upload(chat.id, "a.zip", "application/zip", b"x").await;
    rig.upload(chat.id, "sheet.xlsx", "application/octet-stream", b"x")
        .await; // ok: routed to CI
    assert_free(&rig);
    let uploads = rig.uploads();
    assert_eq!(uploads[0], sample("document", "error"));
    assert_eq!(uploads[1], sample("document", "error"));
    assert_eq!(uploads[2], sample("document", "ok"));
    assert_eq!(uploads.len(), 3);
}

#[tokio::test]
async fn an_in_request_failure_releases_the_permit_and_records_error() {
    // inst-uf-metrics
    let rig = rig().await;
    let chat = rig.std_chat().await;
    rig.storage
        .set_default(Route::VsCreate, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    rig.storage
        .set_default(Route::FilesUpload, Answer::status(500));
    rig.upload_pdf(chat.id).await;
    assert_free(&rig);
    assert_eq!(
        rig.uploads(),
        vec![sample("document", "error"), sample("document", "error")]
    );
    assert!(
        rig.metrics.upload_bytes().is_empty(),
        "bytes are recorded for a 201 only"
    );
}

#[tokio::test]
async fn a_database_failure_mapped_to_500_releases_the_permit_and_records_error() {
    // inst-doc-permit-rule
    let rig = rig().await;
    let chat = rig.std_chat().await;
    fail_attachment_inserts(&rig.db).await;
    rig.upload_pdf(chat.id).await;
    assert_free(&rig);
    assert_eq!(rig.uploads(), vec![sample("document", "error")]);
}

#[tokio::test]
async fn a_dropped_request_releases_its_permit() {
    // inst-uc-release: a client disconnect or a request timeout drops the handler future; the
    // spawned pipeline owns the permit and releases it when it finishes
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let gate = Gate::new();
    rig.storage.set_default(
        Route::FilesUpload,
        Answer::gated(&gate, Answer::ok(&json!({ "id": "file-late" }))),
    );
    let handle = rig.spawn(upload_request(
        chat.id,
        &a1(),
        &[FilePart::file("a.pdf", PDF, &pdf_bytes())],
    ));
    rig.storage.wait_for(Route::FilesUpload, 1).await;
    assert_eq!(rig.permits.available(), 9);
    handle.abort();
    // the pipeline is not abandoned: it still holds the permit while the provider upload runs
    assert_eq!(rig.permits.available(), 9);
    gate.open();
    let wait = async {
        while rig.permits.available() != 10 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .expect("the dropped request kept its permit");
    assert_eq!(rig.uploads(), vec![sample("document", "ok")]);
}

#[tokio::test]
async fn more_sequential_uploads_than_the_limit_are_never_answered_503() {
    // 12 sequential uploads with slow indexing never exhaust the 10 permits; every one is a
    // 201 (`uploaded`) and records `ok`
    let rig = rig().await;
    let chat = rig.std_chat().await;
    // every status read is `in_progress`: each upload is a 201 `uploaded` and its background
    // task (which holds no permit) runs on in the background of the test
    rig.storage
        .set_default(Route::VsAddFile, Answer::indexing("in_progress"));
    rig.storage
        .set_default(Route::VsReadFile, Answer::indexing("in_progress"));
    for n in 0..12 {
        let reply = rig
            .upload(chat.id, &format!("doc-{n}.pdf"), PDF, &pdf_bytes())
            .await;
        assert_eq!(reply.status.as_u16(), 201, "upload {n}: {}", reply.text);
    }
    assert_eq!(rig.uploads().len(), 12);
    assert!(rig.uploads().iter().all(|(_, result)| result == "ok"));
    assert_eq!(rig.permits.available(), 10);
    rig.shutdown.cancel();
}

#[tokio::test]
async fn the_permits_follow_a_changed_concurrency_setting() {
    let rig = rig_with(AttachRigOptions::new().with_rag(RagConfig {
        max_concurrent_uploads: 2,
        ..RagConfig::default()
    }))
    .await;
    assert_eq!(rig.permits.available(), 2);
}

#[tokio::test]
async fn the_otlp_series_record_one_upload_sample_per_request_with_low_cardinality_labels() {
    // the real instruments through the service
    let rig = rig_with(AttachRigOptions::new().with_otlp()).await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    rig.upload(chat.id, "a.zip", "application/zip", b"x").await;
    let harness = rig.otlp.as_ref().expect("otlp harness");
    let points = harness.all_points();
    let upload: Vec<_> = points
        .iter()
        .filter(|(name, _, _)| name == "mini_chat_attachment_upload")
        .collect();
    assert_eq!(upload.len(), 2, "{upload:?}");
    for (_, attributes, value) in &upload {
        assert_eq!(*value, 1);
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes["kind"], "document");
    }
    let results: std::collections::BTreeSet<&str> = upload
        .iter()
        .map(|(_, attributes, _)| attributes["result"].as_str())
        .collect();
    assert_eq!(results, ["error", "ok"].into_iter().collect());
    let pending: Vec<_> = points
        .iter()
        .filter(|(name, _, _)| name == "mini_chat_attachments_pending")
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].2, 0,
        "the pending gauge returned to its prior value"
    );
    for (name, attributes, _) in &points {
        if name.starts_with("mini_chat_attachment") {
            for key in attributes.keys() {
                assert!(
                    ["kind", "result"].contains(&key.as_str()),
                    "{name}: label `{key}`"
                );
            }
        }
    }
}
