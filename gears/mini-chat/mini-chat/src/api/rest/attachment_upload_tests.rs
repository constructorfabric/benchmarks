//! The document upload flow end to end over the router: PDF, XLSX, text types, extension
//! inference, the CSV remap, the filename, the provider requests (URI, multipart, S2S context,
//! no credential), the per-chat vector store reuse, the Azure storage kind, and the 201 body
//! without any provider identifier.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`; flow
//! `cpt-cf-mini-chat-flow-attachments-and-file-search-upload-document`; algorithms
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake`, `-purpose-routing`,
//! `-provider-file-upload`, `-vector-store-provisioning` and `-detail-projection`; definitions
//! of done `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-intake`,
//! `-limits-and-provider-upload` and `-vector-store`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use crate::config::RagConfig;
use crate::test_support::attachments::{
    AZURE_API_VERSION, AZURE_BACKEND, AZURE_MODEL, AZURE_RAG_ALIAS, AttachRigOptions, DOCX,
    FilePart, OPENAI_ALIAS, OPENAI_BACKEND, PDF, XLSX, assert_created, assert_no_provider_text,
    body_id, has_extension, key_set, pdf_bytes, rig, rig_with, upload_request,
};
use crate::test_support::chats::a1;
use crate::test_support::send::provider_s2s;
use crate::test_support::storage_mock::Route;
use crate::test_support::{TENANT_A, USER_A1};

const SEVEN_KEYS: [&str; 7] = [
    "id",
    "filename",
    "content_type",
    "size_bytes",
    "status",
    "kind",
    "created_at",
];

// ── PDF ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_pdf_upload_is_201_ready_with_the_detail_projection() {
    // inst-doc-return-ready, inst-proj-required
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["filename"], "report.pdf");
    assert_eq!(body["content_type"], PDF);
    assert_eq!(body["kind"], "document");
    assert_eq!(body["size_bytes"], pdf_bytes().len());
    let expected: std::collections::BTreeSet<String> =
        SEVEN_KEYS.iter().map(|k| (*k).to_owned()).collect();
    assert_eq!(key_set(&body), expected, "{body}");
    assert!(Uuid::parse_str(body["id"].as_str().unwrap()).is_ok());
}

#[tokio::test]
async fn the_stored_row_is_ready_with_server_derived_ownership() {
    // inst-doc-set-ready, inst-lim-insert
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    let id = body_id(&reply);
    let row = rig.row(chat.id, id).await;
    assert_eq!(row.status, "ready");
    assert_eq!(
        (row.tenant_id, row.chat_id, row.uploaded_by_user_id),
        (TENANT_A, chat.id, USER_A1)
    );
    assert_eq!(row.attachment_kind, "document");
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(row.content_type.as_deref(), Some(PDF));
    assert_eq!(
        row.size_bytes,
        Some(i64::try_from(pdf_bytes().len()).unwrap())
    );
    assert_eq!(row.storage_backend, OPENAI_BACKEND);
    assert!(row.provider_file_id.is_some());
    assert_eq!(row.error_code, None);
    assert_eq!(row.cleanup_status, None);
    assert_eq!(row.secondary_status, "not_attempted");
    assert!(row.img_thumbnail.is_none());
    assert!(row.doc_summary.is_none() && row.summary_model.is_none());
    assert_eq!(row.deleted_at, None);
}

#[tokio::test]
async fn the_provider_sees_the_files_vector_store_and_add_calls_in_order() {
    // inst-doc-provider-upload, inst-doc-vector-store, inst-doc-indexing-wait
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let routes: Vec<Route> = rig.storage.calls().iter().map(|call| call.route).collect();
    assert_eq!(
        routes,
        vec![Route::FilesUpload, Route::VsCreate, Route::VsAddFile]
    );
}

#[tokio::test]
async fn the_files_upload_is_a_multipart_with_purpose_assistants_and_the_structured_filename() {
    // inst-pfu-build, inst-pfu-post, the acceptance criterion on the provider file upload
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    let id = body_id(&reply);
    let call = rig.storage.calls_of(Route::FilesUpload).remove(0);
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, format!("/{OPENAI_ALIAS}/v1/files"));
    assert!(
        call.header("content-type")
            .unwrap()
            .starts_with("multipart/form-data; boundary=")
    );
    assert_eq!(call.text_part("purpose").as_deref(), Some("assistants"));
    let file = call.part("file").expect("file part");
    assert_eq!(
        file.filename.as_deref(),
        Some(format!("{}_{id}.pdf", chat.id).as_str())
    );
    assert_eq!(file.content_type.as_deref(), Some(PDF));
    assert_eq!(&file.data[..], &pdf_bytes()[..]);
}

#[tokio::test]
async fn every_provider_call_runs_under_the_s2s_context_without_a_credential() {
    // implementation note 5; constraint no-credentials
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let s2s = provider_s2s();
    let calls = rig.storage.calls();
    assert!(!calls.is_empty());
    for call in calls {
        assert_eq!(
            (call.subject, call.tenant),
            (s2s.subject_id(), s2s.subject_tenant_id()),
            "{} ran under the user context",
            call.uri
        );
        assert_ne!(call.subject, USER_A1);
        for name in ["authorization", "api-key", "x-api-key", "idempotency-key"] {
            assert!(call.header(name).is_none(), "{} carries `{name}`", call.uri);
        }
    }
}

#[tokio::test]
async fn the_file_is_added_to_the_chat_vector_store_with_only_the_attachment_id_attribute() {
    // inst-iw-add
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let id = body_id(&rig.upload_pdf(chat.id).await);
    let store_id = rig.vector_stores().await.remove(0).vector_store_id.unwrap();
    let add = rig.storage.calls_of(Route::VsAddFile).remove(0);
    assert_eq!(
        add.uri,
        format!("/{OPENAI_ALIAS}/v1/vector_stores/{store_id}/files")
    );
    let row = rig.row(chat.id, id).await;
    assert_eq!(
        add.json(),
        serde_json::json!({
            "file_id": row.provider_file_id.unwrap(),
            "attributes": { "attachment_id": id.to_string() }
        })
    );
}

#[tokio::test]
async fn the_first_document_creates_the_chat_vector_store_and_binds_it() {
    // inst-vs-insert, inst-vs-cas
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let stores = rig.vector_stores().await;
    assert_eq!(stores.len(), 1);
    assert_eq!(
        (stores[0].tenant_id, stores[0].chat_id),
        (TENANT_A, chat.id)
    );
    assert_eq!(stores[0].provider, OPENAI_BACKEND);
    assert!(stores[0].vector_store_id.is_some());
    assert_eq!(rig.storage.count_of(Route::VsCreate), 1);
}

#[tokio::test]
async fn a_second_document_reuses_the_vector_store_of_the_chat() {
    // inst-vs-fast-path: at most one provider vector store per chat
    let rig = rig().await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_created(
        &rig.upload(chat.id, "second.pdf", PDF, &pdf_bytes()).await,
        "ready",
    );
    assert_eq!(rig.storage.count_of(Route::VsCreate), 1);
    assert_eq!(rig.vector_stores().await.len(), 1);
    let adds = rig.storage.calls_of(Route::VsAddFile);
    assert_eq!(adds.len(), 2);
    assert_eq!(adds[0].resource_id, adds[1].resource_id);
}

#[tokio::test]
async fn the_vector_stores_of_two_chats_are_independent() {
    let rig = rig().await;
    let first = rig.std_chat().await;
    let second = rig.std_chat().await;
    assert_created(&rig.upload_pdf(first.id).await, "ready");
    assert_created(&rig.upload_pdf(second.id).await, "ready");
    assert_eq!(rig.storage.count_of(Route::VsCreate), 2);
    let stores = rig.vector_stores().await;
    assert_eq!(stores.len(), 2);
    assert_ne!(stores[0].vector_store_id, stores[1].vector_store_id);
}

#[tokio::test]
async fn the_201_body_never_carries_a_provider_identifier() {
    // inst-pfu-internal-id, inst-proj-no-internal
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig.upload_pdf(chat.id).await;
    assert_created(&reply, "ready");
    assert_no_provider_text(&reply.text);
    let row = rig.only_row(chat.id).await;
    assert!(!reply.text.contains(&row.provider_file_id.unwrap()));
}

// ── XLSX and the code interpreter ────────────────────────────────────────────

#[tokio::test]
async fn an_xlsx_upload_is_201_ready_without_any_vector_store_call() {
    // inst-doc-ci-path, inst-route-derive
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "sheet.xlsx", XLSX, b"PK\x03\x04 spreadsheet")
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["content_type"], XLSX);
    assert_eq!(body["kind"], "document");
    let routes: Vec<Route> = rig.storage.calls().iter().map(|call| call.route).collect();
    assert_eq!(
        routes,
        vec![Route::FilesUpload],
        "an XLSX is uploaded and not indexed"
    );
    assert!(rig.vector_stores().await.is_empty());
    let row = rig.only_row(chat.id).await;
    assert!(row.for_code_interpreter && !row.for_file_search);
    assert_eq!(row.status, "ready");
    assert!(row.provider_file_id.is_some());
}

// ── MIME resolution and the filename ─────────────────────────────────────────

#[tokio::test]
async fn text_markdown_html_and_json_documents_are_accepted() {
    for (filename, mime) in [
        ("a.txt", "text/plain"),
        ("a.md", "text/markdown"),
        ("a.html", "text/html"),
        ("a.json", "application/json"),
        ("a.docx", DOCX),
    ] {
        let rig = rig().await;
        let chat = rig.std_chat().await;
        let reply = rig.upload(chat.id, filename, mime, b"content").await;
        let body = assert_created(&reply, "ready");
        assert_eq!(body["content_type"], mime, "{filename}");
    }
}

#[tokio::test]
async fn an_octet_stream_part_takes_its_type_from_the_filename_extension() {
    // inst-intake-mime
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "contract.docx", "application/octet-stream", b"PK")
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["content_type"], DOCX);
    assert_eq!(
        rig.only_row(chat.id).await.content_type.as_deref(),
        Some(DOCX)
    );
}

#[tokio::test]
async fn csv_is_stored_as_plain_text_while_csv_upload_is_allowed() {
    // inst-intake-csv-remap
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .upload(chat.id, "table.csv", "text/csv", b"a,b\n1,2\n")
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["content_type"], "text/plain");
}

#[tokio::test]
async fn csv_is_refused_while_csv_upload_is_disabled() {
    let rig = rig_with(AttachRigOptions::new().with_rag(RagConfig {
        allow_csv_upload: false,
        ..RagConfig::default()
    }))
    .await;
    let chat = rig.std_chat().await;
    let reply = rig.upload(chat.id, "table.csv", "text/csv", b"a,b\n").await;
    crate::test_support::chats::assert_invalid_argument(
        &reply,
        400,
        "content_type",
        "UNSUPPORTED_CONTENT_TYPE",
    );
    rig.assert_untouched(chat.id).await;
}

#[tokio::test]
async fn a_part_without_a_filename_is_stored_as_upload() {
    // inst-intake-filename
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .send(upload_request(
            chat.id,
            &a1(),
            &[FilePart::nameless(PDF, &pdf_bytes())],
        ))
        .await;
    let body = assert_created(&reply, "ready");
    assert_eq!(body["filename"], "upload");
    assert_eq!(rig.only_row(chat.id).await.filename, "upload");
}

#[tokio::test]
async fn a_long_filename_is_truncated_to_255_characters_keeping_the_extension() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let long = format!("{}.pdf", "n".repeat(400));
    let reply = rig.upload(chat.id, &long, PDF, &pdf_bytes()).await;
    let body = assert_created(&reply, "ready");
    let stored = body["filename"].as_str().unwrap();
    assert_eq!(stored.chars().count(), 255);
    assert!(has_extension(stored, "pdf"));
    assert_eq!(rig.only_row(chat.id).await.filename, stored);
}

#[tokio::test]
async fn a_text_field_before_the_file_part_is_skipped() {
    let rig = rig().await;
    let chat = rig.std_chat().await;
    let reply = rig
        .send(upload_request(
            chat.id,
            &a1(),
            &[
                FilePart::field("note", "hello"),
                FilePart::file("a.pdf", PDF, &pdf_bytes()),
            ],
        ))
        .await;
    assert_created(&reply, "ready");
}

// ── kill switches that do not touch file search routing ──────────────────────

#[tokio::test]
async fn the_file_search_kill_switch_does_not_change_the_upload_routing() {
    // inst-route-file-search-untouched: disable_file_search gates the tool at stream time only
    let rig = rig_with(AttachRigOptions::new().with_snapshot(
        crate::test_support::attachments::catalog_with(crate::test_support::attachments::switches(
            false, false, true,
        )),
    ))
    .await;
    let chat = rig.std_chat().await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    assert_eq!(rig.storage.count_of(Route::VsAddFile), 1);
    assert!(rig.only_row(chat.id).await.for_file_search);
}

// ── Azure storage ────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_azure_storage_chat_uses_the_openai_prefix_with_the_api_version_and_its_label() {
    // the URI form /{alias}/openai/files with api-version; the storage_backend label
    let rig = rig().await;
    let chat = rig.chat(AZURE_MODEL).await;
    assert_created(&rig.upload_pdf(chat.id).await, "ready");
    let upload = rig.storage.calls_of(Route::FilesUpload).remove(0);
    assert_eq!(upload.alias, AZURE_RAG_ALIAS);
    assert_eq!(upload.path, "/openai/files");
    assert_eq!(
        upload.query.as_deref(),
        Some(format!("api-version={AZURE_API_VERSION}").as_str())
    );
    for route in [Route::VsCreate, Route::VsAddFile] {
        let call = rig.storage.calls_of(route).remove(0);
        assert_eq!(call.alias, AZURE_RAG_ALIAS);
        assert!(
            call.path.starts_with("/openai/vector_stores"),
            "{}",
            call.path
        );
        assert_eq!(
            call.query.as_deref(),
            Some(format!("api-version={AZURE_API_VERSION}").as_str())
        );
    }
    let row = rig.only_row(chat.id).await;
    assert_eq!(row.storage_backend, AZURE_BACKEND);
    assert_eq!(rig.vector_stores().await[0].provider, AZURE_BACKEND);
}

// ── another owner of the same tenant ─────────────────────────────────────────

#[tokio::test]
async fn a_second_user_uploads_to_their_own_chat_and_owns_the_row() {
    let rig = rig().await;
    let a2 = crate::test_support::chats::a2();
    let chat = crate::test_support::attachments::seed_chat_for(&rig, &a2).await;
    let reply = rig
        .upload_as(&a2, chat.id, "report.pdf", PDF, &pdf_bytes())
        .await;
    assert_created(&reply, "ready");
    assert_eq!(
        rig.only_row(chat.id).await.uploaded_by_user_id,
        crate::test_support::USER_A2
    );
}
