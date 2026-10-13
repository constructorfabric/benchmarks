#![allow(clippy::case_sensitive_file_extension_comparisons)]

use super::*;

fn ok(ct: &str, name: &str) -> ResolvedMime {
    resolve(ct, name, true).unwrap_or_else(|e| panic!("{ct} / {name}: {e}"))
}

#[test]
fn documents_are_file_search() {
    for (ct, ext) in [
        ("application/pdf", "pdf"),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "docx",
        ),
        (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "pptx",
        ),
        ("text/plain", "txt"),
        ("text/markdown", "md"),
        ("text/html", "html"),
        ("application/json", "json"),
        ("text/x-python", "py"),
        ("text/x-java", "java"),
        ("text/javascript", "js"),
        ("application/typescript", "ts"),
        ("text/x-rust", "rs"),
        ("text/x-go", "go"),
        ("text/x-csharp", "cs"),
        ("text/x-ruby", "rb"),
        ("application/sql", "sql"),
    ] {
        let r = ok(ct, "f");
        assert_eq!(r.mime, ct);
        assert_eq!(r.kind, AttachmentKind::Document, "{ct}");
        assert!(r.for_file_search && !r.for_code_interpreter, "{ct}");
        assert_eq!(r.ext, ext, "{ct}");
    }
}

#[test]
fn xlsx_is_code_interpreter_only() {
    let r = ok(
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "a.xlsx",
    );
    assert_eq!(r.kind, AttachmentKind::Document);
    assert!(!r.for_file_search && r.for_code_interpreter);
    assert_eq!(r.ext, "xlsx");
}

#[test]
fn images_have_no_purpose() {
    for (ct, ext) in [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/webp", "webp"),
        ("image/gif", "gif"),
    ] {
        let r = ok(ct, "x");
        assert_eq!(r.kind, AttachmentKind::Image, "{ct}");
        assert!(!r.for_file_search && !r.for_code_interpreter, "{ct}");
        assert_eq!(r.ext, ext);
    }
}

#[test]
fn parameters_and_case_are_ignored() {
    assert_eq!(ok("Text/Plain; charset=UTF-8", "a.txt").mime, "text/plain");
}

#[test]
fn octet_stream_is_inferred_from_extension() {
    assert_eq!(
        ok("application/octet-stream", "report.PDF").mime,
        "application/pdf"
    );
    assert_eq!(
        ok("application/octet-stream", "sheet.xlsx").mime,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
    );
    assert_eq!(
        ok("application/octet-stream", "pic.jpeg").mime,
        "image/jpeg"
    );
    assert_eq!(
        resolve("application/octet-stream", "blob.bin", true),
        Err(DomainError::UnsupportedContentType)
    );
    assert_eq!(
        resolve("application/octet-stream", "noext", true),
        Err(DomainError::UnsupportedContentType)
    );
}

#[test]
fn csv_is_plain_text_only_when_allowed() {
    let r = ok("text/csv", "data.csv");
    assert_eq!(r.mime, "text/plain");
    assert_eq!(r.ext, "txt");
    assert!(r.for_file_search);
    assert_eq!(
        ok("application/octet-stream", "data.csv").mime,
        "text/plain"
    );
    assert_eq!(
        resolve("text/csv", "data.csv", false),
        Err(DomainError::UnsupportedContentType)
    );
}

#[test]
fn unsupported_types_are_rejected() {
    for ct in [
        "application/zip",
        "video/mp4",
        "image/svg+xml",
        "",
        "garbage",
    ] {
        assert_eq!(
            resolve(ct, "x.zip", true),
            Err(DomainError::UnsupportedContentType),
            "{ct}"
        );
    }
}

#[test]
fn missing_filename_is_upload() {
    assert_eq!(sanitize_filename(None), "upload");
    assert_eq!(sanitize_filename(Some("")), "upload");
    assert_eq!(sanitize_filename(Some("  ")), "upload");
    assert_eq!(sanitize_filename(Some("../")), "upload");
}

#[test]
fn path_separators_are_stripped() {
    assert_eq!(
        sanitize_filename(Some("../../etc/passwd.txt")),
        "passwd.txt"
    );
    assert_eq!(sanitize_filename(Some("C:\\Users\\me\\doc.pdf")), "doc.pdf");
    assert_eq!(sanitize_filename(Some("report.pdf")), "report.pdf");
}

#[test]
fn long_names_are_truncated_keeping_extension() {
    let long = format!("{}.pdf", "a".repeat(300));
    let s = sanitize_filename(Some(&long));
    assert_eq!(s.chars().count(), 255);
    assert!(s.ends_with(".pdf"), "{s}");
    assert_eq!(s, format!("{}.pdf", "a".repeat(251)));

    // Characters, not bytes.
    let wide = format!("{}.txt", "\u{436}".repeat(300));
    let s = sanitize_filename(Some(&wide));
    assert_eq!(s.chars().count(), 255);
    assert!(s.ends_with(".txt"));

    // Exactly 255 stays as is; no extension is cut plainly.
    let exact = "b".repeat(255);
    assert_eq!(sanitize_filename(Some(&exact)), exact);
    assert_eq!(sanitize_filename(Some(&"c".repeat(300))), "c".repeat(255));
}
