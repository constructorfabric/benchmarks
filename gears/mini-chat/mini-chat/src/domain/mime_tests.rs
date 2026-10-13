#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn octet_stream_inferred_from_extension() {
    assert_eq!(
        resolve_content_type(Some("application/octet-stream"), "report.pdf", false).unwrap(),
        "application/pdf"
    );
    assert_eq!(
        resolve_content_type(Some("application/octet-stream"), "Book.XLSX", false).unwrap(),
        XLSX_MIME
    );
}

#[test]
fn unknown_extension_rejected() {
    assert!(matches!(
        resolve_content_type(Some("application/octet-stream"), "a.xyz", false),
        Err(MimeError::Unsupported(_))
    ));
    assert!(matches!(
        resolve_content_type(Some("application/octet-stream"), "noext", false),
        Err(MimeError::Unsupported(_))
    ));
    assert!(matches!(
        resolve_content_type(Some("application/zip"), "a.zip", false),
        Err(MimeError::Unsupported(_))
    ));
}

#[test]
fn missing_content_type() {
    assert_eq!(
        resolve_content_type(None, "a.pdf", false),
        Err(MimeError::MissingContentType)
    );
}

#[test]
fn parameters_and_case_are_ignored() {
    assert_eq!(
        resolve_content_type(Some("Text/Plain; charset=utf-8"), "a.txt", false).unwrap(),
        "text/plain"
    );
}

#[test]
fn csv_remapped_to_text_plain_when_allowed() {
    assert_eq!(
        resolve_content_type(Some("text/csv"), "a.csv", true).unwrap(),
        "text/plain"
    );
    assert_eq!(
        resolve_content_type(Some("application/octet-stream"), "a.csv", true).unwrap(),
        "text/plain"
    );
}

#[test]
fn csv_rejected_when_disallowed() {
    assert!(matches!(
        resolve_content_type(Some("text/csv"), "a.csv", false),
        Err(MimeError::Unsupported(_))
    ));
    assert!(matches!(
        resolve_content_type(Some("application/octet-stream"), "a.csv", false),
        Err(MimeError::Unsupported(_))
    ));
}

#[test]
fn allowlist_accepts_spec_types() {
    for ct in [
        "application/pdf",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        XLSX_MIME,
        "text/plain",
        "text/markdown",
        "text/html",
        "application/json",
        "text/x-python",
        "text/x-java",
        "text/x-java-source",
        "text/javascript",
        "application/javascript",
        "application/typescript",
        "text/x-typescript",
        "text/x-rust",
        "text/x-go",
        "text/x-csharp",
        "text/x-ruby",
        "application/sql",
        "text/x-sql",
        "image/png",
        "image/jpeg",
        "image/webp",
        "image/gif",
    ] {
        assert_eq!(resolve_content_type(Some(ct), "f", false).unwrap(), ct);
    }
}

#[test]
fn extensions_map_to_allowlisted_types() {
    for ext in [
        "pdf", "docx", "pptx", "xlsx", "txt", "md", "html", "htm", "json", "py", "java", "js",
        "ts", "rs", "go", "cs", "rb", "sql", "png", "jpg", "jpeg", "webp", "gif",
    ] {
        let name = format!("f.{ext}");
        let ct = resolve_content_type(Some("application/octet-stream"), &name, false).unwrap();
        assert_ne!(extension_for(&ct), "bin", "{ext} -> {ct}");
    }
}

#[test]
fn xlsx_is_code_interpreter_only() {
    assert_eq!(purposes(XLSX_MIME), (false, true));
    assert_eq!(attachment_kind(XLSX_MIME), AttachmentKind::Document);
}

#[test]
fn documents_use_file_search() {
    assert_eq!(purposes("application/pdf"), (true, false));
    assert_eq!(purposes("text/plain"), (true, false));
}

#[test]
fn images_have_no_purpose() {
    assert_eq!(purposes("image/png"), (false, false));
    assert_eq!(attachment_kind("image/webp"), AttachmentKind::Image);
}

#[test]
fn extension_for_known_types() {
    assert_eq!(extension_for("application/pdf"), "pdf");
    assert_eq!(extension_for("image/jpeg"), "jpg");
    assert_eq!(extension_for(XLSX_MIME), "xlsx");
    assert_eq!(extension_for("text/plain"), "txt");
    assert_eq!(extension_for("application/x-unknown"), "bin");
}

#[test]
fn filename_default_and_truncation_keeps_extension() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("")), "upload");
    assert_eq!(normalize_filename(Some("  ")), "upload");
    assert_eq!(normalize_filename(Some("a.pdf")), "a.pdf");
    let long = format!("{}.pdf", "x".repeat(296));
    assert_eq!(long.chars().count(), 300);
    let n = normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(n.ends_with(".pdf"));
}

#[test]
fn filename_truncation_counts_chars_not_bytes() {
    let long = format!("{}.txt", "é".repeat(300));
    let n = normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(n.ends_with(".txt"));
}
