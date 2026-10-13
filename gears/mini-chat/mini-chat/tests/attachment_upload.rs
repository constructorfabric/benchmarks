//! Attachment upload: `POST /v1/chats/{id}/attachments` (multipart).
//!
//! Source of truth: DESIGN 3.6 "File Upload" (sequence and description), 3.3 "Upload Attachment"
//! error table and REST error mapping, 2.2 kill switches, 3.5 "Provider API Mapping" (Files API
//! `purpose = assistants`), B.7 / B.8 (`rag.*`, `thumbnail.*`), "RAG Quality & Scale Controls",
//! "Model Capability Constraint (Images)", 3.7 `attachments` / `chat_vector_stores`, PRD "Image
//! Upload" and the MIME allowlist (PRD open question "document file types").
//!
//! Covered here: registration, the 201 contract (`Location`, `AttachmentDetail`), the provider
//! wire (Files API multipart, vector store creation and add), documents / images / XLSX routing,
//! thumbnails, every multipart / type / size / limit / kill switch / capability / model error row,
//! PDP, ownership and tenant isolation, provider failures and the concurrency limit. The
//! asynchronous indexing lifecycle lives in `attachment_indexing.rs`.

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

use attachment_support::{
    AttRow, AttachmentHarness, AttachmentOptions, BOUNDARY, Cleanup, DOCX, PDF, PDF_BYTES, PPTX,
    Part, StorageOp, StorageReply, XLSX, assert_attachment_violation, assert_problem,
    assert_unavailable, boundary_of, ctx_a, filler, id_of, jpeg, multipart_body, multipart_type,
    png, quota_models, uid,
};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

use chat_support::{CHAT_PDP_TYPE, assert_field_violation, assert_not_found};
use common::FakePdp;
use stream_support::{contains_provider_identifier, hex, norm_id};

const ATTACHMENT_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";
const GENERIC_503: &str = "Service temporarily unavailable";
const PROVIDER_SECRET: &str = "provider-secret-detail-4711";

fn is_absent_or_null(body: &Value, key: &str) -> bool {
    body.get(key).is_none_or(Value::is_null)
}

fn location_of(resp: &common::Resp) -> String {
    resp.headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// `{chat}_{attachment}.{ext}` with either UUID spelling.
fn assert_provider_filename(name: &str, chat: Uuid, attachment: Uuid, ext: &str) {
    let (stem, got_ext) = name
        .rsplit_once('.')
        .unwrap_or_else(|| panic!("the provider file name `{name}` has an extension"));
    let (c, a) = stem
        .split_once('_')
        .unwrap_or_else(|| panic!("`{name}` is `{{chat_id}}_{{attachment_id}}.{{ext}}`"));
    assert_eq!(norm_id(c), hex(chat), "{name}");
    assert_eq!(norm_id(a), hex(attachment), "{name}");
    assert_eq!(got_ext, ext, "{name}");
}

fn assert_limit(resp: &common::Resp, subject: &str) -> Value {
    let problem = assert_problem(resp, 429, "resource_exhausted");
    assert_eq!(
        problem["context"]["violations"][0]["subject"],
        json!(subject),
        "{}",
        resp.raw
    );
    assert_eq!(
        problem["context"]["resource_type"],
        json!(ATTACHMENT_TYPE),
        "{}",
        resp.raw
    );
    problem
}

/// Nothing was uploaded, stored or enqueued.
async fn assert_untouched(h: &AttachmentHarness, chat: Uuid) {
    h.storage.assert_no_calls();
    assert!(
        h.live_rows(chat).await.is_empty(),
        "no pending / uploaded / ready attachment may exist: {:?}",
        h.rows_of(chat)
            .await
            .iter()
            .map(|r| &r.status)
            .collect::<Vec<_>>()
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// registration
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_three_attachment_operations_are_registered_with_their_declared_statuses() {
    let h = AttachmentHarness::new().await;
    let cases: [(&str, &str, &[u16]); 3] = [
        (
            "POST:/mini-chat/v1/chats/{id}/attachments",
            "mini_chat.upload_attachment",
            &[201, 400, 401, 403, 404, 409, 429, 500, 503],
        ),
        (
            "GET:/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "mini_chat.get_attachment",
            &[200, 400, 401, 403, 404, 500],
        ),
        (
            "DELETE:/mini-chat/v1/chats/{id}/attachments/{attachment_id}",
            "mini_chat.delete_attachment",
            &[204, 400, 401, 403, 404, 409, 500, 503],
        ),
    ];
    for (key, operation_id, statuses) in cases {
        let spec = h
            .openapi
            .operation_specs
            .get(key)
            .unwrap_or_else(|| panic!("operation `{key}` is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id), "{key}");
        assert!(spec.authenticated, "{key} requires authentication");
        assert!(
            spec.tags.iter().any(|t| t == "Mini Chat Attachments"),
            "{key}: {:?}",
            spec.tags
        );
        let license = spec
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{key} must require a license feature"));
        assert_eq!(
            license.license_names,
            vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()],
            "{key}"
        );
        let declared: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
        for status in statuses {
            assert!(
                declared.contains(status),
                "{key} declares {status}: {declared:?}"
            );
        }
    }
    let upload = h
        .openapi
        .operation_specs
        .get("POST:/mini-chat/v1/chats/{id}/attachments")
        .expect("registered");
    assert!(
        upload.request_body.is_some(),
        "the multipart body is declared"
    );
    let created = upload.responses.iter().find(|r| r.status == 201).unwrap();
    assert!(
        created
            .headers
            .iter()
            .any(|header| header.name.eq_ignore_ascii_case("location")),
        "the 201 declares the Location header"
    );
    let delete = h
        .openapi
        .operation_specs
        .get("DELETE:/mini-chat/v1/chats/{id}/attachments/{attachment_id}")
        .expect("registered");
    assert!(delete.request_body.is_none());
}

#[tokio::test]
async fn the_attachment_routes_need_a_security_context() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.seed(AttRow::document(chat, "a.pdf")).await;
    let body = multipart_body(BOUNDARY, &[Part::file(Some("a.pdf"), Some(PDF), PDF_BYTES)]);
    for (method, uri, payload) in [
        ("POST", h.attachments_uri(chat), body),
        ("GET", h.attachment_uri(chat, doc), vec![]),
        ("DELETE", h.attachment_uri(chat, doc), vec![]),
    ] {
        let resp = h
            .raw(method, &uri, None, Some(&multipart_type()), payload)
            .await;
        assert!(
            resp.status.as_u16() >= 400,
            "{method} without a security context is refused, got {}",
            resp.status
        );
    }
    h.storage.assert_no_calls();
    assert_eq!(
        h.rows_of(chat).await.len(),
        1,
        "the seeded row is untouched"
    );
    assert!(h.row(doc).await.deleted_at.is_none());
}

// ═════════════════════════════════════════════════════════════════════════════
// the 201 contract
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_document_upload_is_201_with_location_and_the_attachment_detail() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let resp = h.upload(chat, "Quarterly Report.pdf", PDF, PDF_BYTES).await;

    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    let id = id_of(&body);
    assert_eq!(
        location_of(&resp),
        format!("/mini-chat/v1/chats/{chat}/attachments/{id}"),
        "Location is the request path plus the new id"
    );
    assert_eq!(body["filename"], json!("Quarterly Report.pdf"));
    assert_eq!(body["content_type"], json!(PDF));
    assert_eq!(body["size_bytes"], json!(PDF_BYTES.len()));
    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["kind"], json!("document"));
    attachment_support::parse_ts(&body["created_at"]);
    for key in [
        "error_code",
        "doc_summary",
        "img_thumbnail",
        "summary_updated_at",
    ] {
        assert!(
            is_absent_or_null(&body, key),
            "`{key}` is omitted for a ready document: {body}"
        );
    }
    assert!(
        !contains_provider_identifier(&resp.raw),
        "no provider identifier on the wire: {}",
        resp.raw
    );
    assert!(!resp.raw.contains("vs_s6fake") && !resp.raw.contains("file-s6fake"));
    assert!(body.get("provider_file_id").is_none());
    assert!(body.get("storage_backend").is_none());
}

#[tokio::test]
async fn the_upload_stores_the_row_with_its_routing_flags() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "notes.pdf").await;

    let row = h.row(id).await;
    assert_eq!(row.tenant_id, common::TENANT_A);
    assert_eq!(row.chat_id, chat);
    assert_eq!(row.uploaded_by_user_id, common::USER_A);
    assert_eq!(row.filename, "notes.pdf");
    assert_eq!(row.content_type.as_deref(), Some(PDF));
    assert_eq!(
        row.size_bytes,
        Some(i64::try_from(PDF_BYTES.len()).unwrap())
    );
    assert_eq!(row.status, "ready");
    assert_eq!(row.attachment_kind, "document");
    assert!(row.for_file_search && !row.for_code_interpreter);
    assert_eq!(
        row.storage_backend, "openai",
        "the label of the provider entry"
    );
    assert!(
        row.provider_file_id
            .as_deref()
            .is_some_and(|f| f.starts_with("file-s6fake"))
    );
    assert!(row.error_code.is_none());
    assert!(row.cleanup_status.is_none() && row.cleanup_attempts == 0);
    assert!(row.deleted_at.is_none());
    assert!(row.img_thumbnail.is_none() && row.doc_summary.is_none());
    assert_eq!(row.secondary_status, "not_attempted");
    assert!(row.secondary_file_id.is_none());
    assert!(row.updated_at >= row.created_at);
    // nothing is linked to a message by an upload
    assert!(h.links_of_chat(chat).await.is_empty());
}

#[tokio::test]
async fn a_document_goes_through_the_files_api_then_the_chat_vector_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "contract.pdf").await;

    assert_eq!(
        h.storage.ops(),
        vec![
            StorageOp::UploadFile,
            StorageOp::CreateVectorStore,
            StorageOp::AddVectorStoreFile
        ],
        "upload, lazy store creation, add (no poll: the add answered completed)"
    );
    let provider_file = h.provider_file_of(id).await;
    let store = h.vector_store_of(chat).await;

    let upload = &h.storage.calls_to(StorageOp::UploadFile)[0];
    assert_eq!(upload.method, http::Method::POST);
    assert_eq!(upload.path(), "/provider-alias/v1/files");
    assert!(upload.content_type().starts_with("multipart/form-data"));
    assert!(boundary_of(&upload.content_type()).is_some());
    assert!(
        upload.headers.get("authorization").is_none(),
        "OAGW injects the credentials, mini-chat sets none"
    );
    let purpose = upload.part("purpose").expect("a purpose field");
    assert_eq!(
        purpose.text(),
        "assistants",
        "purpose=assistants for documents"
    );
    let file = upload.part("file").expect("a file part");
    assert_eq!(file.content_type.as_deref(), Some(PDF));
    assert_eq!(file.data, PDF_BYTES, "the bytes are forwarded unchanged");
    assert_provider_filename(file.filename.as_deref().unwrap(), chat, id, "pdf");

    let create = &h.storage.calls_to(StorageOp::CreateVectorStore)[0];
    assert_eq!(create.method, http::Method::POST);
    assert_eq!(create.path(), "/provider-alias/v1/vector_stores");
    assert!(create.json().is_object());

    let add = &h.storage.calls_to(StorageOp::AddVectorStoreFile)[0];
    assert_eq!(add.method, http::Method::POST);
    assert_eq!(
        add.path(),
        format!("/provider-alias/v1/vector_stores/{store}/files")
    );
    let body = add.json();
    assert_eq!(body["file_id"], json!(provider_file));
    assert_eq!(
        body["attributes"],
        json!({"attachment_id": id.to_string()}),
        "the single attribute of a vector store file is the attachment id"
    );

    let stores = h.stores_of(chat).await;
    assert_eq!(stores.len(), 1);
    assert_eq!(stores[0].tenant_id, common::TENANT_A);
    assert_eq!(stores[0].provider, "openai");
    assert!(store.starts_with("vs_s6fake"));
}

#[tokio::test]
async fn the_second_document_of_a_chat_reuses_its_vector_store() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.doc(chat, "one.pdf").await;
    h.doc(chat, "two.pdf").await;
    h.doc(chat, "three.pdf").await;

    assert_eq!(h.storage.count(StorageOp::UploadFile), 3);
    assert_eq!(
        h.storage.count(StorageOp::CreateVectorStore),
        1,
        "created lazily, once"
    );
    assert_eq!(h.storage.count(StorageOp::AddVectorStoreFile), 3);
    assert_eq!(h.stores_of(chat).await.len(), 1);
    let store = h.vector_store_of(chat).await;
    for add in h.storage.calls_to(StorageOp::AddVectorStoreFile) {
        assert_eq!(add.vector_store_id(), store);
    }
}

#[tokio::test]
async fn every_chat_gets_its_own_vector_store_and_never_a_foreign_one() {
    let h = AttachmentHarness::new().await;
    let a = h.seed_chat().await;
    let b = h.seed_chat().await;
    h.doc(a, "a.pdf").await;
    h.doc(b, "b.pdf").await;

    assert_eq!(h.storage.count(StorageOp::CreateVectorStore), 2);
    assert_ne!(h.vector_store_of(a).await, h.vector_store_of(b).await);
    let rows = h.all_stores().await;
    assert_eq!(rows.len(), 2);
    let adds = h.storage.calls_to(StorageOp::AddVectorStoreFile);
    assert_eq!(adds[0].vector_store_id(), h.vector_store_of(a).await);
    assert_eq!(adds[1].vector_store_id(), h.vector_store_of(b).await);
}

#[tokio::test]
async fn an_xlsx_goes_to_the_code_interpreter_only() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let body = h
        .uploaded(chat, "sales.xlsx", XLSX, b"PK\x03\x04 spreadsheet")
        .await;

    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["kind"], json!("document"));
    assert_eq!(body["content_type"], json!(XLSX));
    assert_eq!(
        h.storage.ops(),
        vec![StorageOp::UploadFile],
        "no vector store, no add: an XLSX is not indexed"
    );
    let row = h.row(id_of(&body)).await;
    assert!(row.for_code_interpreter && !row.for_file_search);
    assert!(h.stores_of(chat).await.is_empty());
    let upload = &h.storage.calls_to(StorageOp::UploadFile)[0];
    assert_eq!(upload.part("purpose").unwrap().text(), "assistants");
    assert_eq!(
        upload.part("file").unwrap().content_type.as_deref(),
        Some(XLSX)
    );
}

#[tokio::test]
async fn an_image_is_uploaded_with_purpose_assistants_and_never_indexed() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let bytes = png(400, 200);
    let body = h.uploaded(chat, "photo.png", "image/png", &bytes).await;

    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["kind"], json!("image"));
    assert_eq!(body["content_type"], json!("image/png"));
    assert_eq!(h.storage.ops(), vec![StorageOp::UploadFile]);
    let upload = &h.storage.calls_to(StorageOp::UploadFile)[0];
    assert_eq!(upload.part("purpose").unwrap().text(), "assistants");
    let file = upload.part("file").unwrap();
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
    assert_eq!(file.data, bytes);
    assert_provider_filename(file.filename.as_deref().unwrap(), chat, id_of(&body), "png");

    let row = h.row(id_of(&body)).await;
    assert_eq!(row.attachment_kind, "image");
    assert!(!row.for_file_search && !row.for_code_interpreter);
    assert!(h.stores_of(chat).await.is_empty());
}

// ═════════════════════════════════════════════════════════════════════════════
// thumbnails
// ═════════════════════════════════════════════════════════════════════════════

fn decode_thumbnail(thumbnail: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(thumbnail["data_base64"].as_str().expect("data_base64"))
        .expect("standard base64")
}

#[tokio::test]
async fn an_image_gets_a_webp_thumbnail_that_fits_inside_the_configured_box() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let body = h
        .uploaded(chat, "wide.png", "image/png", &png(400, 200))
        .await;

    let thumbnail = &body["img_thumbnail"];
    assert_eq!(thumbnail["content_type"], json!("image/webp"));
    assert_eq!(thumbnail["width"], json!(128));
    assert_eq!(
        thumbnail["height"],
        json!(64),
        "aspect ratio kept, no cropping"
    );
    let bytes = decode_thumbnail(thumbnail);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WEBP");
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
        .expect("a decodable WebP");
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
    assert!(bytes.len() <= 131_072, "within thumbnail.max_bytes");

    let row = h.row(id_of(&body)).await;
    assert_eq!(row.img_thumbnail.as_deref(), Some(bytes.as_slice()));
    assert_eq!(row.img_thumbnail_width, Some(128));
    assert_eq!(row.img_thumbnail_height, Some(64));
    // the thumbnail is never sent to the provider
    let uploads = h.storage.calls_to(StorageOp::UploadFile);
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].parts().len(), 2, "purpose and file only");
}

#[tokio::test]
async fn the_thumbnail_box_follows_the_thumbnail_config() {
    let h = AttachmentHarness::with_config(json!({"thumbnail": {"width": 32, "height": 32}})).await;
    let chat = h.seed_chat().await;
    let body = h
        .uploaded(chat, "tall.jpg", "image/jpeg", &jpeg(100, 400))
        .await;
    assert_eq!(body["img_thumbnail"]["width"], json!(8));
    assert_eq!(body["img_thumbnail"]["height"], json!(32));
}

#[tokio::test]
async fn a_thumbnail_that_cannot_be_made_leaves_the_image_ready_without_it() {
    // (config patch, image bytes): too big for max_bytes, over max_pixels, over max_decode_bytes,
    // not a decodable image
    let cases = [
        (json!({"thumbnail": {"max_bytes": 1}}), png(300, 300)),
        (json!({"thumbnail": {"max_pixels": 100}}), png(40, 40)),
        (
            json!({"thumbnail": {"max_decode_bytes": 64}}),
            png(300, 300),
        ),
        (json!({}), b"this is not an image at all".to_vec()),
    ];
    for (config, bytes) in cases {
        let h = AttachmentHarness::with_config(config.clone()).await;
        let chat = h.seed_chat().await;
        let resp = h.upload(chat, "x.png", "image/png", &bytes).await;
        assert_eq!(resp.status.as_u16(), 201, "{config}: {}", resp.raw);
        let body = resp.json();
        assert_eq!(body["status"], json!("ready"), "{config}");
        assert!(
            is_absent_or_null(&body, "img_thumbnail"),
            "{config}: {body}"
        );
        assert!(is_absent_or_null(&body, "error_code"), "{config}: {body}");
        let row = h.row(id_of(&body)).await;
        assert!(row.img_thumbnail.is_none(), "{config}");
        assert_eq!(row.status, "ready");
        assert!(
            row.error_code.is_none(),
            "a thumbnail failure never sets error_code"
        );
    }
}

#[tokio::test]
async fn a_document_never_has_a_thumbnail() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "d.pdf").await;
    let row = h.row(id).await;
    assert!(row.img_thumbnail.is_none());
    assert!(row.img_thumbnail_width.is_none() && row.img_thumbnail_height.is_none());
}

// ═════════════════════════════════════════════════════════════════════════════
// content types
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn every_allowlisted_type_is_accepted_with_its_kind() {
    let cases: [(&str, &str); 13] = [
        (PDF, "document"),
        (DOCX, "document"),
        (PPTX, "document"),
        (XLSX, "document"),
        ("text/plain", "document"),
        ("text/markdown", "document"),
        ("text/html", "document"),
        ("application/json", "document"),
        ("image/png", "image"),
        ("image/jpeg", "image"),
        ("image/webp", "image"),
        ("image/gif", "image"),
        ("text/csv", "document"),
    ];
    let h = AttachmentHarness::new().await;
    for (content_type, kind) in cases {
        let chat = h.seed_chat().await;
        let resp = h
            .upload(chat, "file.bin", content_type, b"some bytes")
            .await;
        assert_eq!(resp.status.as_u16(), 201, "{content_type}: {}", resp.raw);
        let body = resp.json();
        assert_eq!(body["kind"], json!(kind), "{content_type}");
        assert_eq!(body["status"], json!("ready"), "{content_type}");
    }
}

#[tokio::test]
async fn an_unsupported_type_is_400_unsupported_content_type_and_nothing_is_stored() {
    let cases = [
        "application/zip",
        "application/x-msdownload",
        "video/mp4",
        "image/svg+xml",
        "image/tiff",
        "audio/mpeg",
        "application/octet-stream",
    ];
    for content_type in cases {
        let h = AttachmentHarness::new().await;
        let chat = h.seed_chat().await;
        let resp = h.upload(chat, "payload.dat", content_type, b"bytes").await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_field_violation_reason(&problem, "UNSUPPORTED_CONTENT_TYPE");
        assert_eq!(
            problem["context"]["resource_type"],
            json!(ATTACHMENT_TYPE),
            "{content_type}"
        );
        assert_untouched(&h, chat).await;
    }
}

fn assert_field_violation_reason(problem: &Value, reason: &str) {
    chat_support::assert_violation_reason(problem, reason);
}

#[tokio::test]
async fn an_octet_stream_part_gets_its_type_from_the_file_extension() {
    let cases = [
        ("report.pdf", PDF, "document"),
        ("memo.docx", DOCX, "document"),
        ("deck.pptx", PPTX, "document"),
        ("numbers.xlsx", XLSX, "document"),
        ("shot.png", "image/png", "image"),
        ("shot.jpg", "image/jpeg", "image"),
        ("README.md", "text/markdown", "document"),
    ];
    let h = AttachmentHarness::new().await;
    for (filename, inferred, kind) in cases {
        let chat = h.seed_chat().await;
        let resp = h
            .upload(chat, filename, "application/octet-stream", b"opaque bytes")
            .await;
        assert_eq!(resp.status.as_u16(), 201, "{filename}: {}", resp.raw);
        let body = resp.json();
        assert_eq!(body["content_type"], json!(inferred), "{filename}");
        assert_eq!(body["kind"], json!(kind), "{filename}");
    }
}

#[tokio::test]
async fn an_octet_stream_part_with_an_unknown_extension_is_unsupported() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    for filename in ["blob.bin", "no_extension", "archive.exe"] {
        let resp = h
            .upload(chat, filename, "application/octet-stream", b"opaque bytes")
            .await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_field_violation_reason(&problem, "UNSUPPORTED_CONTENT_TYPE");
    }
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn rust_go_and_sql_source_is_a_document_uploaded_to_the_files_api_as_plain_text() {
    let h = AttachmentHarness::new().await;
    for (filename, content_type) in [
        ("main.rs", "text/x-rust"),
        ("main.go", "text/x-go"),
        ("schema.sql", "application/sql"),
        ("alias.sql", "text/x-sql"),
        ("lib.rs", "application/octet-stream"),
        ("util.go", "application/octet-stream"),
        ("query.sql", "application/octet-stream"),
    ] {
        let chat = h.seed_chat().await;
        let resp = h.upload(chat, filename, content_type, b"source code").await;
        assert_eq!(resp.status.as_u16(), 201, "{filename}: {}", resp.raw);
        let body = resp.json();
        assert_eq!(body["kind"], json!("document"), "{filename}");
        assert_eq!(body["status"], json!("ready"), "{filename}");
        let uploads = h.storage.calls_to(StorageOp::UploadFile);
        let file = uploads.last().unwrap().part("file").unwrap();
        assert_eq!(file.content_type.as_deref(), Some("text/plain"));
        assert_eq!(file.data, b"source code");
    }
    assert_eq!(h.storage.calls_to(StorageOp::UploadFile).len(), 7);
}

#[tokio::test]
async fn csv_is_accepted_as_plain_text_while_csv_upload_is_allowed() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let body = h
        .uploaded(chat, "data.csv", "text/csv", b"a,b\n1,2\n")
        .await;

    assert_eq!(body["content_type"], json!("text/plain"), "remapped");
    assert_eq!(body["kind"], json!("document"));
    let upload = &h.storage.calls_to(StorageOp::UploadFile)[0];
    assert_eq!(
        upload.part("file").unwrap().content_type.as_deref(),
        Some("text/plain"),
        "the provider gets the remapped type"
    );
    // an octet-stream .csv is inferred as CSV and remapped the same way
    let chat2 = h.seed_chat().await;
    let inferred = h
        .uploaded(chat2, "more.csv", "application/octet-stream", b"a,b\n")
        .await;
    assert_eq!(inferred["content_type"], json!("text/plain"));
}

#[tokio::test]
async fn csv_is_unsupported_when_csv_upload_is_off() {
    let h = AttachmentHarness::with_config(json!({"rag": {"allow_csv_upload": false}})).await;
    let chat = h.seed_chat().await;
    let resp = h.upload(chat, "data.csv", "text/csv", b"a,b\n1,2\n").await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation_reason(&problem, "UNSUPPORTED_CONTENT_TYPE");
    assert_untouched(&h, chat).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// file names
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_missing_filename_defaults_to_upload() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let resp = h
        .upload_parts(
            chat,
            &ctx_a(),
            &[Part::file(None, Some("text/plain"), b"anonymous")],
        )
        .await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(resp.json()["filename"], json!("upload"));
}

#[tokio::test]
async fn a_long_filename_is_truncated_to_255_characters_keeping_the_extension() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let name = format!("{}.pdf", "a".repeat(300));
    let body = h.uploaded(chat, &name, PDF, PDF_BYTES).await;
    let stored = body["filename"].as_str().unwrap();
    assert_eq!(stored.chars().count(), 255, "{stored}");
    assert!(
        std::path::Path::new(stored)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf")),
        "{stored}"
    );
    assert!(stored.starts_with("aaaa"));
    assert_eq!(h.row(id_of(&body)).await.filename, stored);
}

// ═════════════════════════════════════════════════════════════════════════════
// multipart errors
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_request_without_a_multipart_boundary_is_400_boundary_required() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let body = multipart_body(BOUNDARY, &[Part::file(Some("a.pdf"), Some(PDF), PDF_BYTES)]);
    for content_type in [
        None,
        Some("multipart/form-data"),
        Some("multipart/form-data; charset=utf-8"),
    ] {
        let resp = h
            .upload_raw(chat, &ctx_a(), content_type, body.clone())
            .await;
        assert_attachment_violation(
            &resp,
            "invalid_argument",
            "content_type",
            "BOUNDARY_REQUIRED",
        );
    }
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn an_unreadable_multipart_body_is_400_multipart_error() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let truncated = format!("--{BOUNDARY}\r\nContent-Disp").into_bytes();
    let bad_header = format!(
        "--{BOUNDARY}\r\nContent-Disposition form-data; name=\"file\"\r\n\r\ndata\r\n--{BOUNDARY}--\r\n"
    )
    .into_bytes();
    for body in [truncated, bad_header] {
        let resp = h
            .upload_raw(chat, &ctx_a(), Some(&multipart_type()), body)
            .await;
        assert_attachment_violation(&resp, "invalid_argument", "multipart", "MULTIPART_ERROR");
    }
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn a_body_without_a_file_field_is_400_missing_file() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let bodies = [
        multipart_body(BOUNDARY, &[Part::field("note", "just text")]),
        multipart_body(BOUNDARY, &[]),
        multipart_body(
            BOUNDARY,
            &[Part {
                name: "upload".to_owned(),
                filename: Some("a.pdf".to_owned()),
                content_type: Some(PDF.to_owned()),
                data: PDF_BYTES.to_vec(),
            }],
        ),
    ];
    for body in bodies {
        let resp = h
            .upload_raw(chat, &ctx_a(), Some(&multipart_type()), body)
            .await;
        assert_attachment_violation(&resp, "invalid_argument", "file", "MISSING_FILE");
    }
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn a_file_part_without_a_content_type_is_400_missing_content_type() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let resp = h
        .upload_parts(
            chat,
            &ctx_a(),
            &[Part::file(Some("a.pdf"), None, PDF_BYTES)],
        )
        .await;
    assert_attachment_violation(
        &resp,
        "invalid_argument",
        "content_type",
        "MISSING_CONTENT_TYPE",
    );
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn other_form_fields_are_ignored() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let resp = h
        .upload_parts(
            chat,
            &ctx_a(),
            &[
                Part::field("note", "ignored"),
                Part::file(Some("a.pdf"), Some(PDF), PDF_BYTES),
            ],
        )
        .await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    assert_eq!(resp.json()["filename"], json!("a.pdf"));
}

// ═════════════════════════════════════════════════════════════════════════════
// size limits (DESIGN B.8: min(gear config, model max_file_size_mb))
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_document_over_the_document_limit_is_400_file_too_large() {
    let h = AttachmentHarness::with_config(json!({"rag": {"uploaded_file_max_size_kb": 4}})).await;
    let chat = h.seed_chat().await;

    let exact = h.upload(chat, "exact.pdf", PDF, &filler(4096)).await;
    assert_eq!(
        exact.status.as_u16(),
        201,
        "exactly the limit is accepted: {}",
        exact.raw
    );

    let calls_before = h.storage.calls().len();
    let over = h.upload(chat, "over.pdf", PDF, &filler(4097)).await;
    assert_attachment_violation(&over, "out_of_range", "content_length", "FILE_TOO_LARGE");
    assert_eq!(
        h.storage.calls().len(),
        calls_before,
        "nothing reaches the provider"
    );
    assert_eq!(
        h.live_rows(chat).await.len(),
        1,
        "only the accepted file is stored"
    );
}

#[tokio::test]
async fn an_image_over_the_image_limit_is_400_file_too_large() {
    let h = AttachmentHarness::with_config(json!({"rag": {"uploaded_image_max_size_kb": 2}})).await;
    let chat = h.seed_chat().await;

    let exact = h
        .upload(chat, "exact.webp", "image/webp", &filler(2048))
        .await;
    assert_eq!(exact.status.as_u16(), 201, "{}", exact.raw);
    let over = h
        .upload(chat, "over.webp", "image/webp", &filler(2049))
        .await;
    assert_attachment_violation(&over, "out_of_range", "content_length", "FILE_TOO_LARGE");
    // the document limit is its own: a 3 KiB document is fine
    let doc = h.upload(chat, "doc.pdf", PDF, &filler(3072)).await;
    assert_eq!(doc.status.as_u16(), 201, "{}", doc.raw);
}

#[tokio::test]
async fn the_model_file_size_limit_lowers_the_configured_limit() {
    let mut models = quota_models();
    models[0]["general_config"]["max_file_size_mb"] = json!(1);
    let h = AttachmentHarness::with_models(models).await;
    let chat = h.seed_chat().await;

    // 900 000 bytes fit below 1 MB (10^6 or 2^20 bytes), 1 100 000 do not
    let below = h.upload(chat, "below.pdf", PDF, &filler(900_000)).await;
    assert_eq!(below.status.as_u16(), 201, "{}", below.raw);
    let over = h.upload(chat, "over.pdf", PDF, &filler(1_100_000)).await;
    assert_attachment_violation(&over, "out_of_range", "content_length", "FILE_TOO_LARGE");
    // it applies to images too (the image limit of 5 MiB is higher)
    let image = h
        .upload(chat, "big.webp", "image/webp", &filler(1_100_000))
        .await;
    assert_attachment_violation(&image, "out_of_range", "content_length", "FILE_TOO_LARGE");
}

#[tokio::test]
async fn the_configured_limit_wins_when_it_is_lower_than_the_model_limit() {
    let h = AttachmentHarness::with_config(json!({"rag": {"uploaded_file_max_size_kb": 1}})).await;
    let chat = h.seed_chat().await;
    let over = h.upload(chat, "over.pdf", PDF, &filler(1025)).await;
    assert_attachment_violation(&over, "out_of_range", "content_length", "FILE_TOO_LARGE");
}

// ═════════════════════════════════════════════════════════════════════════════
// per-chat limits (DESIGN "RAG Quality & Scale Controls")
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_document_count_limit_is_429_document_limit() {
    let h = AttachmentHarness::with_config(json!({"rag": {"max_documents_per_chat": 2}})).await;
    let chat = h.seed_chat().await;
    h.doc(chat, "one.pdf").await;
    h.doc(chat, "two.pdf").await;

    let uploads_before = h.storage.count(StorageOp::UploadFile);
    let third = h.upload(chat, "three.pdf", PDF, PDF_BYTES).await;
    assert_limit(&third, "document_limit");
    assert_eq!(
        h.storage.count(StorageOp::UploadFile),
        uploads_before,
        "no provider upload"
    );
    assert_eq!(h.live_rows(chat).await.len(), 2);

    // images are not documents
    let image = h.upload(chat, "pic.png", "image/png", &png(8, 8)).await;
    assert_eq!(image.status.as_u16(), 201, "{}", image.raw);
    // an XLSX is a document
    let xlsx = h.upload(chat, "t.xlsx", XLSX, b"PK").await;
    assert_limit(&xlsx, "document_limit");
    // the limit is per chat
    let other = h.seed_chat().await;
    assert_eq!(
        h.upload(other, "o.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

#[tokio::test]
async fn failed_and_deleted_documents_do_not_count_toward_the_document_limit() {
    let h = AttachmentHarness::with_config(json!({"rag": {"max_documents_per_chat": 1}})).await;
    let chat = h.seed_chat().await;
    h.seed(
        AttRow::document(chat, "failed.pdf")
            .status("failed")
            .error("upload_failed"),
    )
    .await;
    h.seed(AttRow::document(chat, "deleted.pdf").deleted())
        .await;
    let resp = h.upload(chat, "fresh.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);

    let again = h.upload(chat, "second.pdf", PDF, PDF_BYTES).await;
    assert_limit(&again, "document_limit");

    // deleting the live one frees the slot
    let id = id_of(&resp.json());
    assert_eq!(h.delete(chat, id).await.status.as_u16(), 204);
    let after = h.upload(chat, "third.pdf", PDF, PDF_BYTES).await;
    assert_eq!(after.status.as_u16(), 201, "{}", after.raw);
}

#[tokio::test]
async fn the_total_size_limit_counts_documents_and_images_and_is_429_storage_limit() {
    let h =
        AttachmentHarness::with_config(json!({"rag": {"max_total_upload_mb_per_chat": 1}})).await;
    let chat = h.seed_chat().await;
    let limit = 1024 * 1024_i64;
    // an image of the chat fills the whole budget
    h.seed(AttRow::image(chat, "full.png").size(limit)).await;

    let resp = h.upload(chat, "one-more-byte.pdf", PDF, b"x").await;
    assert_limit(&resp, "storage_limit");
    assert!(h.storage.calls_to(StorageOp::UploadFile).is_empty());
    assert_eq!(h.live_rows(chat).await.len(), 1);

    // another chat has its own budget
    let other = h.seed_chat().await;
    assert_eq!(
        h.upload(other, "o.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

#[tokio::test]
async fn an_upload_that_still_fits_the_total_budget_is_accepted() {
    let h =
        AttachmentHarness::with_config(json!({"rag": {"max_total_upload_mb_per_chat": 1}})).await;
    let chat = h.seed_chat().await;
    // 100 bytes below 1 MB (whether a MB counts 10^6 or 2^20 bytes): 10 more bytes fit, 60 000 do not
    h.seed(AttRow::document(chat, "big.pdf").size(1_000_000 - 100))
        .await;
    let resp = h.upload(chat, "small.pdf", PDF, &filler(10)).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
    let bigger = h.upload(chat, "bigger.pdf", PDF, &filler(60_000)).await;
    assert_limit(&bigger, "storage_limit");
}

#[tokio::test]
async fn failed_and_deleted_attachments_do_not_count_toward_the_total_size() {
    let h =
        AttachmentHarness::with_config(json!({"rag": {"max_total_upload_mb_per_chat": 1}})).await;
    let chat = h.seed_chat().await;
    h.seed(
        AttRow::document(chat, "failed.pdf")
            .status("failed")
            .size(1024 * 1024),
    )
    .await;
    h.seed(
        AttRow::document(chat, "deleted.pdf")
            .deleted()
            .size(1024 * 1024),
    )
    .await;
    let resp = h.upload(chat, "fits.pdf", PDF, &filler(1000)).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
}

#[tokio::test]
async fn a_vector_store_of_another_provider_backend_is_409_provider_mismatch() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_store(chat, "vs_foreign_backend_0001", "azure-elsewhere")
        .await;

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    let problem = assert_problem(&resp, 409, "already_exists");
    assert_eq!(
        problem["context"]["resource_name"],
        json!("provider_mismatch")
    );
    assert!(
        h.storage.calls_to(StorageOp::AddVectorStoreFile).is_empty(),
        "the foreign store is never written"
    );
    assert!(h.live_rows(chat).await.is_empty());
    assert_eq!(
        h.stores_of(chat).await.len(),
        1,
        "the foreign row is left alone"
    );
    assert!(!resp.raw.contains("vs_foreign_backend_0001"));
}

// ═════════════════════════════════════════════════════════════════════════════
// kill switches and model capabilities
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_image_is_refused_while_disable_images_is_on() {
    let h = AttachmentHarness::new().await;
    h.policy.set_kill_switches(json!({"disable_images": true}));
    let chat = h.seed_chat().await;

    let resp = h.upload(chat, "photo.png", "image/png", &png(8, 8)).await;
    let problem = assert_problem(&resp, 400, "failed_precondition");
    assert_precondition_images(&problem);
    assert_untouched(&h, chat).await;

    // documents are not affected
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

fn assert_precondition_images(problem: &Value) {
    chat_support::assert_precondition(problem, "images", "FEATURE_DISABLED");
}

#[tokio::test]
async fn an_image_for_a_chat_whose_model_has_no_vision_is_400_vision_not_supported() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat_on(quota_support::STANDARD).await;

    let resp = h.upload(chat, "photo.png", "image/png", &png(8, 8)).await;
    assert_attachment_violation(
        &resp,
        "invalid_argument",
        "content_type",
        "VISION_NOT_SUPPORTED",
    );
    assert_untouched(&h, chat).await;
    // a document is fine on that model
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

#[tokio::test]
async fn an_xlsx_is_refused_while_the_code_interpreter_kill_switch_is_on() {
    let h = AttachmentHarness::new().await;
    h.policy
        .set_kill_switches(json!({"disable_code_interpreter": true}));
    let chat = h.seed_chat().await;

    let resp = h.upload(chat, "sales.xlsx", XLSX, b"PK\x03\x04").await;
    assert_attachment_violation(
        &resp,
        "invalid_argument",
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    assert_untouched(&h, chat).await;
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

#[tokio::test]
async fn an_xlsx_is_refused_when_the_chat_model_has_no_code_interpreter() {
    let mut models = quota_models();
    models[1]["general_config"]["tool_support"]["code_interpreter"] = json!(false);
    let h = AttachmentHarness::with_models(models).await;
    let chat = h.seed_chat_on(quota_support::STANDARD).await;

    let resp = h.upload(chat, "sales.xlsx", XLSX, b"PK\x03\x04").await;
    assert_attachment_violation(
        &resp,
        "invalid_argument",
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn the_chat_model_is_resolved_without_the_enabled_filter() {
    let h = AttachmentHarness::new().await;
    // `gpt-off` is disabled in the catalog but still the model of this chat
    let chat = h.seed_chat_on("gpt-off").await;
    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);
}

#[tokio::test]
async fn a_chat_whose_model_left_the_catalog_is_400_invalid_model_before_the_body_is_read() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.policy
        .set_catalog(&[quota_support::find_model(&quota_models(), "gpt-standard")]);

    // the body is not even multipart: the model check comes first
    let resp = h
        .upload_raw(chat, &ctx_a(), None, b"not a multipart body".to_vec())
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "model", "INVALID_MODEL");
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn a_model_policy_plugin_failure_is_a_generic_500_before_the_body_is_read() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.flaky.fail_with("policy-plugin-secret-text");

    let resp = h
        .upload_raw(chat, &ctx_a(), None, b"not a multipart body".to_vec())
        .await;
    assert_problem(&resp, 500, "internal");
    assert!(
        !resp.raw.contains("policy-plugin-secret-text"),
        "{}",
        resp.raw
    );
    assert_untouched(&h, chat).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// chat resolution, ownership, authorization
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_unknown_foreign_or_deleted_chat_is_404_before_the_body_is_read() {
    let h = AttachmentHarness::new().await;
    let other_user = h
        .seed_chat_of(
            chat_support::USER_A2,
            common::TENANT_A,
            quota_support::PREMIUM,
        )
        .await;
    let other_tenant = h
        .seed_chat_of(common::USER_B, common::TENANT_B, quota_support::PREMIUM)
        .await;
    let deleted = h.seed_deleted_chat().await;
    for chat in [uid(0xdead_beef), other_user, other_tenant, deleted] {
        // even a body that is not multipart gets the 404 first
        let resp = h
            .upload_raw(chat, &ctx_a(), None, b"not a multipart body".to_vec())
            .await;
        assert_not_found(&resp, "chat");
    }
    h.storage.assert_no_calls();
    assert!(h.chat.all_attachments().await.is_empty());
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_400_invalid_path_params() {
    let h = AttachmentHarness::new().await;
    let uri = format!("{}/v1/chats/not-a-uuid/attachments", h.prefix());
    let body = multipart_body(BOUNDARY, &[Part::file(Some("a.pdf"), Some(PDF), PDF_BYTES)]);
    let resp = h
        .raw("POST", &uri, Some(&ctx_a()), Some(&multipart_type()), body)
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    chat_support::assert_violation_reason(&problem, "invalid_path_params");
    h.storage.assert_no_calls();
}

#[tokio::test]
async fn the_upload_asks_the_pdp_once_for_upload_attachment_on_the_chat() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    let before = h.pdp.recorded().len();
    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(resp.status.as_u16(), 201, "{}", resp.raw);

    let sent: Vec<_> = h.pdp.recorded().into_iter().skip(before).collect();
    assert_eq!(
        sent.len(),
        1,
        "one evaluation per upload: {:?}",
        sent.iter()
            .map(|r| r.action.name.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(sent[0].action.name, "upload_attachment");
    assert_eq!(sent[0].resource.resource_type, CHAT_PDP_TYPE);
    assert_eq!(sent[0].resource.id, Some(chat));
    assert!(sent[0].context.require_constraints);
}

#[tokio::test]
async fn a_pdp_denial_is_403_authz_denied_and_nothing_is_uploaded() {
    let h = AttachmentHarness::with(AttachmentOptions {
        pdp: FakePdp::deny("internal-policy-rule-42"),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    let problem = assert_problem(&resp, 403, "permission_denied");
    assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    assert!(
        !resp.raw.contains("internal-policy-rule-42"),
        "{}",
        resp.raw
    );
    assert_untouched(&h, chat).await;
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_with_retry_after_5_and_nothing_is_uploaded() {
    let h = AttachmentHarness::with(AttachmentOptions {
        pdp: FakePdp::unreachable("pdp-host-10.0.0.7 timed out"),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_unavailable(&resp, "5");
    assert!(!resp.raw.contains("10.0.0.7"), "{}", resp.raw);
    assert_untouched(&h, chat).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// storage selection (providers.<id>.storage_kind / storage_backend / rag_provider)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_azure_storage_entry_uses_the_openai_prefix_and_the_api_version_query() {
    let h = AttachmentHarness::with_config(json!({"providers": {"openai": {
        "storage_kind": "azure", "api_version": "2025-03-01-preview"}}}))
    .await;
    let chat = h.seed_chat().await;
    h.doc(chat, "a.pdf").await;

    let upload = &h.storage.calls_to(StorageOp::UploadFile)[0];
    assert_eq!(upload.path(), "/provider-alias/openai/files");
    assert_eq!(upload.query(), "api-version=2025-03-01-preview");
    assert_eq!(
        upload.part("purpose").unwrap().text(),
        "assistants",
        "assistants on Azure too"
    );
    let create = &h.storage.calls_to(StorageOp::CreateVectorStore)[0];
    assert_eq!(create.path(), "/provider-alias/openai/vector_stores");
    assert_eq!(create.query(), "api-version=2025-03-01-preview");
    let add = &h.storage.calls_to(StorageOp::AddVectorStoreFile)[0];
    assert!(
        add.path()
            .starts_with("/provider-alias/openai/vector_stores/")
    );
    assert!(add.path().ends_with("/files"));
    assert_eq!(add.query(), "api-version=2025-03-01-preview");
}

#[tokio::test]
async fn the_storage_backend_label_is_stored_on_the_row_and_the_vector_store() {
    let h = AttachmentHarness::with_config(json!({"providers": {"openai": {
        "storage_backend": "main-storage"}}}))
    .await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;
    assert_eq!(h.row(id).await.storage_backend, "main-storage");
    assert_eq!(h.stores_of(chat).await[0].provider, "main-storage");
}

#[tokio::test]
async fn files_go_to_the_rag_provider_of_the_model_provider_entry() {
    let h = AttachmentHarness::with_config(json!({"providers": {
        "openai": {"rag_provider": "rag"},
        "rag": {
            "kind": "openai_responses",
            "host": "rag.provider.test",
            "upstream_alias": "rag-alias",
            "storage_kind": "openai"
        }
    }}))
    .await;
    let chat = h.seed_chat().await;
    let id = h.doc(chat, "a.pdf").await;

    for call in h.storage.calls() {
        assert!(
            call.path().starts_with("/rag-alias/v1/"),
            "{} goes through the rag provider's alias",
            call.path()
        );
    }
    assert_eq!(
        h.row(id).await.storage_backend,
        "rag",
        "the label of the provider that stores"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// provider failures
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_failed_provider_upload_is_503_with_retry_after_10_and_a_failed_row() {
    for reply in [
        StorageReply::error(500, PROVIDER_SECRET),
        StorageReply::error(429, PROVIDER_SECRET),
        StorageReply::Unavailable,
    ] {
        let h = AttachmentHarness::new().await;
        let chat = h.seed_chat().await;
        h.storage.push(StorageOp::UploadFile, reply.clone());

        let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
        let problem = assert_unavailable(&resp, "10");
        assert_eq!(problem["detail"], json!(GENERIC_503), "{reply:?}");
        assert!(!resp.raw.contains(PROVIDER_SECRET), "{}", resp.raw);
        assert!(!contains_provider_identifier(&resp.raw));

        let rows = h.rows_of(chat).await;
        assert_eq!(rows.len(), 1, "the row stays visible");
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].error_code.as_deref(), Some("upload_failed"));
        assert!(rows[0].provider_file_id.is_none());
        assert_eq!(
            h.storage.ops(),
            vec![StorageOp::UploadFile],
            "no store, no retry: {reply:?}"
        );

        // the failed row is readable
        let got = h.get(chat, rows[0].id).await;
        assert_eq!(got.status.as_u16(), 200, "{}", got.raw);
        let body = got.json();
        assert_eq!(body["status"], json!("failed"));
        assert_eq!(body["error_code"], json!("upload_failed"));
    }
}

#[tokio::test]
async fn a_failed_vector_store_creation_fails_the_upload_with_503() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage.push(
        StorageOp::CreateVectorStore,
        StorageReply::error(500, PROVIDER_SECRET),
    );

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    let problem = assert_unavailable(&resp, "10");
    assert_eq!(problem["detail"], json!(GENERIC_503));
    assert!(!resp.raw.contains(PROVIDER_SECRET));
    let rows = h.rows_of(chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "failed");
    assert!(rows[0].error_code.is_some());
    assert!(
        h.stores_of(chat).await.is_empty(),
        "no store row without a provider store"
    );
    assert!(h.storage.calls_to(StorageOp::AddVectorStoreFile).is_empty());
}

#[tokio::test]
async fn a_failed_add_to_the_vector_store_is_503_indexing_failed_and_deletes_the_file_once() {
    let h = AttachmentHarness::with(AttachmentOptions {
        attachment_cleanup: Cleanup::Recording,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.storage.push(
        StorageOp::AddVectorStoreFile,
        StorageReply::error(500, PROVIDER_SECRET),
    );
    // the best effort delete fails too: it is not retried
    h.storage.push(
        StorageOp::DeleteFile,
        StorageReply::error(500, PROVIDER_SECRET),
    );

    let resp = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    let problem = assert_unavailable(&resp, "10");
    assert_eq!(problem["detail"], json!(GENERIC_503));
    assert!(!resp.raw.contains(PROVIDER_SECRET));
    assert!(
        !resp.raw.contains("indexing_failed"),
        "the code is not in the response body"
    );

    let rows = h.rows_of(chat).await;
    assert_eq!(rows[0].status, "failed");
    assert_eq!(rows[0].error_code.as_deref(), Some("indexing_failed"));
    let provider_file = rows[0]
        .provider_file_id
        .clone()
        .expect("the file was uploaded");

    let deletes = h.storage.wait_calls(StorageOp::DeleteFile, 1).await;
    assert_eq!(deletes[0].last_segment(), provider_file);
    assert_eq!(
        deletes[0].path(),
        format!("/provider-alias/v1/files/{provider_file}")
    );
    h.storage.assert_count_stays(StorageOp::DeleteFile, 1).await;
    // no outbox message: the inline delete is the whole cleanup
    h.attachment_log.assert_count_stays(0).await;
}

#[tokio::test]
async fn a_failed_upload_leaves_the_chat_usable_for_a_new_upload() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage
        .push(StorageOp::UploadFile, StorageReply::error(500, "boom"));
    let first = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(first.status.as_u16(), 503);

    // a retry is a new upload with a new attachment and a new provider file
    let second = h.upload(chat, "a.pdf", PDF, PDF_BYTES).await;
    assert_eq!(second.status.as_u16(), 201, "{}", second.raw);
    let rows = h.rows_of(chat).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().filter(|r| r.status == "ready").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "failed").count(), 1);
    assert_eq!(h.storage.count(StorageOp::UploadFile), 2);
}

// ═════════════════════════════════════════════════════════════════════════════
// concurrency (rag.max_concurrent_uploads)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_upload_beyond_the_concurrency_limit_is_503_with_retry_after_5() {
    let h = AttachmentHarness::with_config(json!({"rag": {"max_concurrent_uploads": 1}})).await;
    let chat = h.seed_chat().await;
    h.storage.hold(StorageOp::UploadFile);

    let first = h.upload(chat, "first.pdf", PDF, PDF_BYTES);
    let second = async {
        h.storage.wait_paused(1).await;
        let resp = h.upload(chat, "second.pdf", PDF, PDF_BYTES).await;
        h.storage.release();
        resp
    };
    let (first, second) = tokio::join!(first, second);

    assert_eq!(first.status.as_u16(), 201, "{}", first.raw);
    assert_unavailable(&second, "5");
    assert_eq!(
        h.storage.count(StorageOp::UploadFile),
        1,
        "the rejected upload never reached the provider"
    );
    assert_eq!(h.live_rows(chat).await.len(), 1);
}

#[tokio::test]
async fn a_finished_upload_releases_its_slot_whether_it_succeeded_or_failed() {
    let h = AttachmentHarness::with_config(json!({"rag": {"max_concurrent_uploads": 1}})).await;
    let chat = h.seed_chat().await;
    h.storage
        .push(StorageOp::UploadFile, StorageReply::error(500, "boom"));
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        503
    );
    assert_eq!(
        h.upload(chat, "b.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
    assert_eq!(
        h.upload(chat, "c.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
    // a rejected request (validation) does not leak a slot either
    let bad = h.upload(chat, "x.zip", "application/zip", b"zip").await;
    assert_eq!(bad.status.as_u16(), 400);
    assert_eq!(
        h.upload(chat, "d.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        201
    );
}

#[tokio::test]
async fn a_new_message_can_not_yet_reference_an_upload_that_failed() {
    let h = AttachmentHarness::new().await;
    let chat = h.seed_chat().await;
    h.storage
        .push(StorageOp::UploadFile, StorageReply::error(500, "boom"));
    assert_eq!(
        h.upload(chat, "a.pdf", PDF, PDF_BYTES)
            .await
            .status
            .as_u16(),
        503
    );
    let failed = h.rows_of(chat).await[0].id;

    let resp = h.reject_with(chat, "see the file", &[failed]).await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "attachment", "invalid_attachment");
    h.oagw.assert_no_provider_call();
}
