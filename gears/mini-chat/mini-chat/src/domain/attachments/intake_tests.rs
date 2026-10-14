//! Upload intake rules: MIME resolution (allowlist, extension inference, CSV remap), filename
//! normalization, kind classification, purpose routing, the effective per-file limit, the
//! provider-side filename and the storage provider resolution.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithms `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-intake` and
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-purpose-routing`; definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-intake` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;
use uuid::Uuid;

use super::{
    IMAGE_MIME_TYPES, XLSX_MIME, effective_limit_bytes, kind_of, normalize_filename,
    provider_filename, resolve_mime, resolve_secondary, resolve_storage, route_purposes,
};
use crate::config::{MiniChatConfig, RagConfig, StorageKind};
use crate::domain::attachments::AttachmentKind;
use crate::domain::error::{AttachmentError, DomainError};
use crate::domain::provider_registry::ProviderRegistry;
use crate::test_support::attachments::{
    ANTHROPIC_ALIAS, AZURE_API_VERSION, AZURE_BACKEND, AZURE_RAG_ALIAS, OPENAI_ALIAS,
    OPENAI_BACKEND, has_extension, providers,
};
use crate::test_support::{TENANT_A, TENANT_B};

const PDF: &str = "application/pdf";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";

fn unsupported(result: &Result<String, DomainError>) -> bool {
    matches!(
        result,
        Err(DomainError::Attachment(
            AttachmentError::UnsupportedContentType
        ))
    )
}

// ── MIME resolution ──────────────────────────────────────────────────────────

#[test]
fn every_allowlisted_type_resolves_to_itself() {
    // inst-intake-mime, inst-intake-unsupported: PDF, DOCX, PPTX, XLSX, plain text, Markdown,
    // HTML, JSON and the four images
    for mime in [
        PDF,
        DOCX,
        PPTX,
        XLSX_MIME,
        "text/plain",
        "text/markdown",
        "text/html",
        "application/json",
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
    ] {
        assert_eq!(
            resolve_mime(mime, "file", true).as_deref().ok(),
            Some(mime),
            "{mime}"
        );
    }
}

#[test]
fn content_type_parameters_and_case_are_ignored() {
    assert_eq!(
        resolve_mime("Application/PDF; charset=binary", "a.pdf", true).unwrap(),
        PDF
    );
    assert_eq!(
        resolve_mime("text/plain; charset=utf-8", "a.txt", true).unwrap(),
        "text/plain"
    );
    assert_eq!(
        resolve_mime("IMAGE/PNG", "a.png", true).unwrap(),
        "image/png"
    );
}

#[test]
fn octet_stream_is_inferred_from_the_filename_extension() {
    // inst-intake-mime: for example .pdf, .docx, .xlsx, .png
    let octet = "application/octet-stream";
    assert_eq!(resolve_mime(octet, "report.pdf", true).unwrap(), PDF);
    assert_eq!(resolve_mime(octet, "report.docx", true).unwrap(), DOCX);
    assert_eq!(resolve_mime(octet, "deck.pptx", true).unwrap(), PPTX);
    assert_eq!(resolve_mime(octet, "sheet.xlsx", true).unwrap(), XLSX_MIME);
    assert_eq!(resolve_mime(octet, "photo.png", true).unwrap(), "image/png");
    assert_eq!(
        resolve_mime(octet, "photo.jpg", true).unwrap(),
        "image/jpeg"
    );
    assert_eq!(
        resolve_mime(octet, "photo.jpeg", true).unwrap(),
        "image/jpeg"
    );
    assert_eq!(
        resolve_mime(octet, "photo.webp", true).unwrap(),
        "image/webp"
    );
    assert_eq!(resolve_mime(octet, "anim.gif", true).unwrap(), "image/gif");
    assert_eq!(
        resolve_mime(octet, "notes.txt", true).unwrap(),
        "text/plain"
    );
    assert_eq!(
        resolve_mime(octet, "notes.md", true).unwrap(),
        "text/markdown"
    );
    assert_eq!(resolve_mime(octet, "page.html", true).unwrap(), "text/html");
    assert_eq!(
        resolve_mime(octet, "data.json", true).unwrap(),
        "application/json"
    );
}

#[test]
fn the_extension_inference_is_case_insensitive() {
    assert_eq!(
        resolve_mime("application/octet-stream", "REPORT.PDF", true).unwrap(),
        PDF
    );
}

#[test]
fn source_code_extensions_are_accepted_as_documents() {
    // the allowlist names Python, Java, JavaScript, TypeScript, Rust, Go, C#, Ruby and SQL
    for filename in [
        "a.py", "A.java", "a.js", "a.ts", "a.rs", "a.go", "a.cs", "a.rb", "a.sql",
    ] {
        let mime = resolve_mime("application/octet-stream", filename, true)
            .unwrap_or_else(|e| panic!("{filename}: {e:?}"));
        assert_eq!(kind_of(&mime), AttachmentKind::Document, "{filename}");
    }
}

#[test]
fn an_unknown_extension_keeps_octet_stream_and_is_unsupported() {
    assert!(unsupported(&resolve_mime(
        "application/octet-stream",
        "blob.bin",
        true
    )));
    assert!(unsupported(&resolve_mime(
        "application/octet-stream",
        "noextension",
        true
    )));
}

#[test]
fn the_declared_type_wins_over_the_extension() {
    assert_eq!(resolve_mime(PDF, "misnamed.png", true).unwrap(), PDF);
}

#[test]
fn types_outside_the_allowlist_are_unsupported() {
    // inst-intake-return-unsupported
    for mime in [
        "application/zip",
        "video/mp4",
        "application/x-msdownload",
        "image/svg+xml",
        "image/bmp",
        "application/msword",
        "",
    ] {
        assert!(unsupported(&resolve_mime(mime, "file.bin", true)), "{mime}");
    }
}

#[test]
fn csv_is_accepted_as_plain_text_while_csv_upload_is_allowed() {
    // inst-intake-csv-remap
    assert_eq!(
        resolve_mime("text/csv", "table.csv", true).unwrap(),
        "text/plain"
    );
    assert_eq!(
        resolve_mime("application/octet-stream", "table.csv", true).unwrap(),
        "text/plain"
    );
}

#[test]
fn csv_is_unsupported_while_csv_upload_is_disabled() {
    assert!(unsupported(&resolve_mime("text/csv", "table.csv", false)));
    assert!(unsupported(&resolve_mime(
        "application/octet-stream",
        "table.csv",
        false
    )));
}

// ── filename ─────────────────────────────────────────────────────────────────

#[test]
fn a_missing_filename_is_upload() {
    // inst-intake-filename
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("")), "upload");
}

#[test]
fn a_short_filename_is_kept() {
    assert_eq!(normalize_filename(Some("report.pdf")), "report.pdf");
}

#[test]
fn a_filename_of_exactly_255_characters_is_kept() {
    let name = format!("{}.pdf", "a".repeat(251));
    assert_eq!(name.chars().count(), 255);
    assert_eq!(normalize_filename(Some(&name)), name);
}

#[test]
fn a_long_filename_is_truncated_to_255_characters_keeping_the_extension() {
    let name = format!("{}.pdf", "a".repeat(400));
    let normalized = normalize_filename(Some(&name));
    assert_eq!(normalized.chars().count(), 255);
    assert!(has_extension(&normalized, "pdf"), "{normalized}");
    assert!(normalized.starts_with("aaaa"));
}

#[test]
fn truncation_counts_characters_not_bytes() {
    let name = format!("{}.txt", "\u{e9}".repeat(300));
    let normalized = normalize_filename(Some(&name));
    assert_eq!(normalized.chars().count(), 255);
    assert!(has_extension(&normalized, "txt"), "{normalized}");
}

// ── kind and purposes ────────────────────────────────────────────────────────

#[test]
fn the_four_image_types_are_images_and_everything_else_is_a_document() {
    // inst-intake-kind
    for mime in IMAGE_MIME_TYPES {
        assert_eq!(kind_of(mime), AttachmentKind::Image, "{mime}");
    }
    for mime in [PDF, DOCX, PPTX, XLSX_MIME, "text/plain", "application/json"] {
        assert_eq!(kind_of(mime), AttachmentKind::Document, "{mime}");
    }
}

#[test]
fn a_document_is_routed_to_file_search_only() {
    // inst-route-derive
    let purposes = route_purposes(PDF, AttachmentKind::Document, false, true).unwrap();
    assert!(purposes.for_file_search);
    assert!(!purposes.for_code_interpreter);
}

#[test]
fn an_xlsx_is_routed_to_code_interpreter_only() {
    let purposes = route_purposes(XLSX_MIME, AttachmentKind::Document, false, true).unwrap();
    assert!(!purposes.for_file_search);
    assert!(purposes.for_code_interpreter);
}

#[test]
fn an_image_has_no_purpose_flag() {
    for mime in IMAGE_MIME_TYPES {
        let purposes = route_purposes(mime, AttachmentKind::Image, false, true).unwrap();
        assert!(!purposes.for_file_search, "{mime}");
        assert!(!purposes.for_code_interpreter, "{mime}");
    }
}

#[test]
fn an_image_is_never_rejected_by_the_code_interpreter_gates() {
    // the drop applies to for_code_interpreter only; an image has no purpose to lose
    let purposes = route_purposes("image/png", AttachmentKind::Image, true, false).unwrap();
    assert!(!purposes.for_file_search && !purposes.for_code_interpreter);
}

fn ci_unavailable(result: &Result<super::Purposes, DomainError>) -> bool {
    matches!(
        result,
        Err(DomainError::Attachment(
            AttachmentError::CodeInterpreterUnavailable
        ))
    )
}

#[test]
fn an_xlsx_is_rejected_while_the_code_interpreter_kill_switch_is_on() {
    // inst-route-ci-unavailable, inst-route-no-purpose, inst-route-return-ci-unavailable
    assert!(ci_unavailable(&route_purposes(
        XLSX_MIME,
        AttachmentKind::Document,
        true,
        true
    )));
}

#[test]
fn an_xlsx_is_rejected_on_a_model_without_code_interpreter_support() {
    assert!(ci_unavailable(&route_purposes(
        XLSX_MIME,
        AttachmentKind::Document,
        false,
        false
    )));
    assert!(ci_unavailable(&route_purposes(
        XLSX_MIME,
        AttachmentKind::Document,
        true,
        false
    )));
}

#[test]
fn the_code_interpreter_gates_do_not_touch_a_file_search_document() {
    // inst-route-file-search-untouched: only the code interpreter is filtered at upload
    let purposes = route_purposes(PDF, AttachmentKind::Document, true, false).unwrap();
    assert!(purposes.for_file_search);
    assert!(!purposes.for_code_interpreter);
}

// ── effective per-file limit ─────────────────────────────────────────────────

#[test]
fn the_document_limit_defaults_to_25600_kib() {
    // inst-intake-limit: rag.uploaded_file_max_size_kb * 1024
    let rag = RagConfig::default();
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Document, &rag, 1000),
        25_600 * 1024
    );
}

#[test]
fn the_image_limit_defaults_to_5120_kib() {
    let rag = RagConfig::default();
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Image, &rag, 1000),
        5120 * 1024
    );
}

#[test]
fn the_model_limit_applies_when_it_is_lower() {
    // min(gear limit, model max_file_size_mb * 1048576)
    let rag = RagConfig::default();
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Document, &rag, 1),
        1_048_576
    );
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Image, &rag, 2),
        2 * 1_048_576
    );
}

#[test]
fn the_gear_limit_applies_when_it_is_lower() {
    let rag = RagConfig {
        uploaded_file_max_size_kb: 10,
        uploaded_image_max_size_kb: 20,
        ..RagConfig::default()
    };
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Document, &rag, 25),
        10 * 1024
    );
    assert_eq!(
        effective_limit_bytes(AttachmentKind::Image, &rag, 25),
        20 * 1024
    );
}

// ── provider-side filename ───────────────────────────────────────────────────

#[test]
fn the_provider_filename_is_chat_id_attachment_id_and_the_extension() {
    // inst-pfu-build: {chat_id}_{attachment_id}.{ext}
    let chat = Uuid::new_v4();
    let attachment = Uuid::new_v4();
    assert_eq!(
        provider_filename(chat, attachment, "Quarterly report.pdf"),
        format!("{chat}_{attachment}.pdf")
    );
    assert_eq!(
        provider_filename(chat, attachment, "photo.png"),
        format!("{chat}_{attachment}.png")
    );
}

#[test]
fn the_provider_filename_never_carries_the_original_name() {
    let name = provider_filename(Uuid::new_v4(), Uuid::new_v4(), "secret-plans.pdf");
    assert!(!name.contains("secret"), "{name}");
}

// ── storage provider resolution ──────────────────────────────────────────────

#[test]
fn the_storage_is_the_model_provider_itself_without_a_rag_provider() {
    // inst-intake-storage
    let registry = providers();
    let target = resolve_storage(&registry, "azure_openai", TENANT_A).unwrap();
    assert_eq!(target.provider_id, "azure_openai");
    assert_eq!(target.upstream_alias, OPENAI_ALIAS);
    assert_eq!(target.storage_kind, StorageKind::Openai);
    assert_eq!(target.storage_backend, OPENAI_BACKEND);
    assert_eq!(target.api_version, None);
}

#[test]
fn the_storage_follows_the_rag_provider_of_the_model_provider() {
    let registry = providers();
    let target = resolve_storage(&registry, "anthropic", TENANT_A).unwrap();
    assert_eq!(target.provider_id, "azure_openai");
    assert_eq!(target.upstream_alias, OPENAI_ALIAS);
    assert_ne!(target.upstream_alias, ANTHROPIC_ALIAS);
}

#[test]
fn an_azure_storage_carries_its_api_version_and_its_backend_label() {
    let registry = providers();
    let target = resolve_storage(&registry, "azure_rag", TENANT_A).unwrap();
    assert_eq!(target.storage_kind, StorageKind::Azure);
    assert_eq!(target.upstream_alias, AZURE_RAG_ALIAS);
    assert_eq!(target.api_version.as_deref(), Some(AZURE_API_VERSION));
    assert_eq!(target.storage_backend, AZURE_BACKEND);
}

#[test]
fn the_storage_alias_follows_the_tenant_override() {
    let tenant_key = TENANT_A.hyphenated().to_string();
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": {
            "azure_openai": {
                "kind": "openai_responses", "host": "azure.example.com",
                "storage_kind": "openai",
                "tenant_overrides": { tenant_key: { "host": "tenant-a.example.com" } }
            }
        }
    }))
    .expect("valid config");
    let registry = ProviderRegistry::build(&config.providers);
    let own = resolve_storage(&registry, "azure_openai", TENANT_A).unwrap();
    assert_eq!(own.upstream_alias, "tenant-a.example.com");
    let other = resolve_storage(&registry, "azure_openai", TENANT_B).unwrap();
    assert_eq!(other.upstream_alias, "azure.example.com");
}

#[test]
fn an_unknown_provider_has_no_storage() {
    // inst-intake-storage-missing: 500 internal
    let registry = providers();
    let result = resolve_storage(&registry, "ghost", TENANT_A);
    assert!(
        matches!(result, Err(DomainError::Internal { .. })),
        "{result:?}"
    );
}

#[test]
fn only_an_anthropic_messages_provider_has_a_secondary_target() {
    // inst-sec-skip: the chat model's provider kind must be anthropic_messages
    let registry = providers();
    let anthropic = resolve_secondary(&registry, "anthropic", TENANT_A).expect("anthropic target");
    assert_eq!(anthropic.upstream_alias, ANTHROPIC_ALIAS);
    assert!(resolve_secondary(&registry, "azure_openai", TENANT_A).is_none());
    assert!(resolve_secondary(&registry, "azure_rag", TENANT_A).is_none());
    assert!(resolve_secondary(&registry, "ghost", TENANT_A).is_none());
}
