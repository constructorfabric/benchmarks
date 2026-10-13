//! Unit tests of the upload allowlist and the file name rules.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn documents_images_and_spreadsheets_get_their_routing() {
    let pdf = classify("application/pdf", "a.pdf", true).unwrap();
    assert_eq!(pdf.kind, AttachmentKind::Document);
    assert!(pdf.for_file_search && !pdf.for_code_interpreter);

    let sheet = classify(XLSX, "a.xlsx", true).unwrap();
    assert_eq!(sheet.kind, AttachmentKind::Document);
    assert!(!sheet.for_file_search && sheet.for_code_interpreter);

    let image = classify("IMAGE/PNG", "a.png", true).unwrap();
    assert_eq!(image.kind, AttachmentKind::Image);
    assert!(!image.for_file_search && !image.for_code_interpreter);
    assert_eq!(image.content_type, "image/png");
}

#[test]
fn octet_stream_takes_the_type_of_the_extension() {
    assert_eq!(
        classify("application/octet-stream", "Report.PDF", true)
            .unwrap()
            .content_type,
        "application/pdf"
    );
    assert!(classify("application/octet-stream", "blob.bin", true).is_none());
    assert!(classify("application/octet-stream", "no_extension", true).is_none());
}

#[test]
fn csv_is_plain_text_only_while_allowed() {
    let csv = classify("text/csv", "d.csv", true).unwrap();
    assert_eq!(csv.content_type, "text/plain");
    assert_eq!(csv.extension, "csv");
    assert!(classify("text/csv", "d.csv", false).is_none());
    assert!(classify("application/octet-stream", "d.csv", false).is_none());
}

#[test]
fn rust_go_and_sql_source_is_a_plain_text_file_search_document() {
    for (content_type, filename) in [
        ("text/x-rust", "main.rs"),
        ("text/rust", "main.rs"),
        ("text/x-go", "main.go"),
        ("text/x-golang", "main.go"),
        ("application/sql", "schema.sql"),
        ("text/x-sql", "schema.sql"),
        ("TEXT/X-RUST", "main.rs"),
        ("application/octet-stream", "main.rs"),
        ("application/octet-stream", "MAIN.GO"),
        ("application/octet-stream", "schema.SQL"),
    ] {
        let kind = classify(content_type, filename, false).unwrap();
        assert_eq!(kind.content_type, "text/plain", "{content_type} {filename}");
        assert_eq!(kind.kind, AttachmentKind::Document, "{content_type}");
        assert!(kind.for_file_search && !kind.for_code_interpreter);
        assert_eq!(kind.extension, "txt");
    }
}

#[test]
fn unsupported_types_are_refused() {
    for content_type in [
        "application/zip",
        "image/svg+xml",
        "video/mp4",
        "image/tiff",
    ] {
        assert!(
            classify(content_type, "x", true).is_none(),
            "{content_type}"
        );
    }
}

#[test]
fn a_long_name_keeps_its_extension() {
    let name = format!("{}.pdf", "a".repeat(300));
    let cut = truncate_filename(&name, 255);
    assert_eq!(cut.chars().count(), 255);
    assert_eq!(std::path::Path::new(&cut).extension().unwrap(), "pdf");
    assert_eq!(truncate_filename("short.txt", 255), "short.txt");
    let no_extension = "b".repeat(300);
    assert_eq!(truncate_filename(&no_extension, 255).chars().count(), 255);
}
