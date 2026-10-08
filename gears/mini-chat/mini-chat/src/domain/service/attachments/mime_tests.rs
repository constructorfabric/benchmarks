#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn essence_strips_parameters_and_case() {
    assert_eq!(essence("Text/Plain; charset=UTF-8"), "text/plain");
    assert_eq!(essence(" application/pdf "), "application/pdf");
}

#[test]
fn octet_stream_is_inferred_from_extension() {
    let cases = vec![
        ("report.PDF", "application/pdf"),
        ("doc.docx", DOCX),
        ("deck.pptx", PPTX),
        ("sheet.xlsx", XLSX),
        ("img.png", "image/png"),
        ("photo.jpeg", "image/jpeg"),
        ("main.rs", "text/x-rust"),
        ("data.csv", "text/csv"),
        ("notes.md", "text/markdown"),
    ];
    for (name, want) in cases {
        assert_eq!(effective_content_type("application/octet-stream", name), want, "{name}");
    }
    assert_eq!(effective_content_type("application/octet-stream", "blob.xyz"), OCTET_STREAM);
    assert_eq!(effective_content_type("application/octet-stream", "noext"), OCTET_STREAM);
    // Non-octet types are kept as sent.
    assert_eq!(effective_content_type("image/png", "x.pdf"), "image/png");
}

#[test]
fn allow_list() {
    let supported = vec![
        ("application/pdf", "application/pdf"),
        (DOCX, DOCX),
        (PPTX, PPTX),
        (XLSX, XLSX),
        ("text/plain", "text/plain"),
        ("text/markdown", "text/markdown"),
        ("text/x-markdown", "text/markdown"),
        ("text/html", "text/html"),
        ("application/json", "application/json"),
        ("text/x-python", "text/x-python"),
        ("text/x-java", "text/x-java"),
        ("application/javascript", "application/javascript"),
        ("text/javascript", "text/javascript"),
        ("application/typescript", "application/typescript"),
        ("text/x-typescript", "text/x-typescript"),
        ("text/x-rust", "text/x-rust"),
        ("text/x-go", "text/x-go"),
        ("text/x-csharp", "text/x-csharp"),
        ("text/x-ruby", "text/x-ruby"),
        ("application/sql", "application/sql"),
        ("text/x-sql", "text/x-sql"),
        ("image/png", "image/png"),
        ("image/jpeg", "image/jpeg"),
        ("image/jpg", "image/jpeg"),
        ("image/webp", "image/webp"),
        ("image/gif", "image/gif"),
    ];
    for (ct, want) in supported {
        assert_eq!(check(ct, true), MimeCheck::Supported(want), "{ct}");
    }
    for ct in ["application/octet-stream", "image/bmp", "application/zip", "video/mp4", ""] {
        assert_eq!(check(ct, true), MimeCheck::Unsupported, "{ct}");
    }
}

#[test]
fn csv_is_text_plain_only_when_allowed() {
    assert_eq!(check("text/csv", true), MimeCheck::Supported("text/plain"));
    assert_eq!(check("text/csv", false), MimeCheck::Unsupported);
}

#[test]
fn kind_and_purposes() {
    assert_eq!(kind_of("image/gif"), AttachmentKind::Image);
    assert_eq!(kind_of("application/pdf"), AttachmentKind::Document);
    assert_eq!(purposes_of(XLSX), (false, true));
    assert_eq!(purposes_of("application/pdf"), (true, false));
    assert_eq!(purposes_of("image/png"), (false, false));
    assert_eq!(provider_extension("text/x-python"), "py");
    assert_eq!(provider_extension(XLSX), "xlsx");
}

#[test]
fn filename_defaults_and_truncation() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("  ")), "upload");
    assert_eq!(normalize_filename(Some("a.pdf")), "a.pdf");

    let long = format!("{}.pdf", "x".repeat(300));
    let got = normalize_filename(Some(&long));
    assert_eq!(got.chars().count(), 255);
    assert!(got.ends_with(".pdf"));
    assert!(got.starts_with("xxx"));

    let no_ext = "y".repeat(400);
    assert_eq!(normalize_filename(Some(&no_ext)).chars().count(), 255);

    // Multi-byte characters are counted as characters, not bytes.
    let multi = format!("{}.txt", "é".repeat(260));
    let got = normalize_filename(Some(&multi));
    assert_eq!(got.chars().count(), 255);
    assert!(got.ends_with(".txt"));

    let exactly = "z".repeat(255);
    assert_eq!(normalize_filename(Some(&exactly)), exactly);
}
