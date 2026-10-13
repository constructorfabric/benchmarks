#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::case_sensitive_file_extension_comparisons)]
//! Slice 1: file classification and purpose routing
//! (`cpt-cf-mini-chat-algo-attachments-classify-file`), the file name rule
//! (`inst-filename` of `cpt-cf-mini-chat-algo-attachments-read-multipart`) and the
//! effective size limit (`inst-size-limits` of
//! `cpt-cf-mini-chat-algo-attachments-resolve-upload-context`).

use super::classify::{
    DEFAULT_FILENAME, FILENAME_MAX_CHARS, classify_file, normalize_filename, size_limit_bytes,
};
use super::model::{AttachmentKind, Classification, ClassifyRules};
use crate::domain::error::{DisabledFeature, DomainError};

const PDF: &str = "application/pdf";
const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const PPTX: &str = "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

const OPEN: ClassifyRules = ClassifyRules {
    allow_csv_upload: true,
    disable_images: false,
    code_interpreter_available: true,
};

fn classify(declared: &str, filename: &str) -> Result<Classification, DomainError> {
    classify_file(declared, filename, &OPEN)
}

fn ok(declared: &str, filename: &str) -> Classification {
    classify(declared, filename)
        .unwrap_or_else(|e| panic!("`{declared}` / `{filename}` must be accepted: {e:?}"))
}

/// One row of the MIME allowlist: `(declared values with the stored form
/// first, extensions for octet-stream, provider extension, kind)`.
type Row = (
    &'static [&'static str],
    &'static [&'static str],
    &'static str,
    AttachmentKind,
);

const DOC: AttachmentKind = AttachmentKind::Document;
const IMG: AttachmentKind = AttachmentKind::Image;

fn allowlist() -> Vec<Row> {
    vec![
        (&[PDF], &[".pdf"], "pdf", DOC),
        (&[DOCX], &[".docx"], "docx", DOC),
        (&[PPTX], &[".pptx"], "pptx", DOC),
        (&[XLSX], &[".xlsx"], "xlsx", DOC),
        (&["text/plain"], &[".txt"], "txt", DOC),
        (
            &["text/markdown", "text/x-markdown"],
            &[".md", ".markdown"],
            "md",
            DOC,
        ),
        (&["text/html"], &[".html", ".htm"], "html", DOC),
        (&["application/json"], &[".json"], "json", DOC),
        (
            &["text/x-python", "text/x-script.python"],
            &[".py"],
            "py",
            DOC,
        ),
        (
            &["text/x-java", "text/x-java-source"],
            &[".java"],
            "java",
            DOC,
        ),
        (
            &["text/javascript", "application/javascript"],
            &[".js", ".mjs"],
            "js",
            DOC,
        ),
        (
            &["application/typescript", "text/x-typescript"],
            &[".ts"],
            "ts",
            DOC,
        ),
        (&["text/x-rust"], &[".rs"], "rs", DOC),
        (&["text/x-golang", "text/x-go"], &[".go"], "go", DOC),
        (&["text/x-csharp"], &[".cs"], "cs", DOC),
        (&["text/x-ruby"], &[".rb"], "rb", DOC),
        (&["application/sql", "text/x-sql"], &[".sql"], "sql", DOC),
        (&["image/png"], &[".png"], "png", IMG),
        (&["image/jpeg", "image/jpg"], &[".jpg", ".jpeg"], "jpg", IMG),
        (&["image/webp"], &[".webp"], "webp", IMG),
        (&["image/gif"], &[".gif"], "gif", IMG),
    ]
}

fn purposes(stored: &str, kind: AttachmentKind) -> (bool, bool) {
    match (kind, stored) {
        (AttachmentKind::Image, _) => (false, false),
        (AttachmentKind::Document, XLSX) => (false, true),
        (AttachmentKind::Document, _) => (true, false),
    }
}

// ---- the allowlist ---------------------------------------------------------------------------------

#[test]
fn every_declared_value_of_the_allowlist_is_stored_in_its_first_form() {
    for (declared, _, ext, kind) in allowlist() {
        let stored = declared[0];
        for value in declared {
            let got = ok(value, "file.bin");
            assert_eq!(got.content_type, stored, "{value}");
            assert_eq!(got.kind, kind, "{value}");
            assert_eq!(got.extension, ext, "{value}");
            let (search, interpreter) = purposes(stored, kind);
            assert_eq!(
                (got.for_file_search, got.for_code_interpreter),
                (search, interpreter),
                "{value}"
            );
        }
    }
}

#[test]
fn octet_stream_takes_the_type_of_the_file_name_extension() {
    for (declared, extensions, ext, _) in allowlist() {
        for extension in extensions {
            let got = ok("application/octet-stream", &format!("report{extension}"));
            assert_eq!(got.content_type, declared[0], "{extension}");
            assert_eq!(got.extension, ext, "{extension}");
        }
    }
}

#[test]
fn the_extension_is_compared_case_insensitively() {
    assert_eq!(
        ok("application/octet-stream", "REPORT.PDF").content_type,
        PDF
    );
    assert_eq!(
        ok("application/octet-stream", "Notes.DocX").content_type,
        DOCX
    );
    assert_eq!(
        ok("application/octet-stream", "Photo.JPEG").content_type,
        "image/jpeg"
    );
}

#[test]
fn only_the_last_extension_of_the_file_name_counts() {
    assert_eq!(
        ok("application/octet-stream", "archive.tar.pdf").content_type,
        PDF
    );
    assert!(matches!(
        classify("application/octet-stream", "report.pdf.zip"),
        Err(DomainError::UnsupportedContentType)
    ));
}

#[test]
fn octet_stream_with_an_unknown_or_missing_extension_is_unsupported() {
    assert!(classify("application/octet-stream", "known.pdf").is_ok());
    for name in [
        "blob.bin",
        "archive.zip",
        "noextension",
        "trailingdot.",
        "upload",
        "",
    ] {
        assert!(
            matches!(
                classify("application/octet-stream", name),
                Err(DomainError::UnsupportedContentType)
            ),
            "`{name}`"
        );
    }
}

#[test]
fn a_declared_type_other_than_octet_stream_ignores_the_extension() {
    assert_eq!(ok(PDF, "renamed.txt").content_type, PDF);
    assert!(matches!(
        classify("application/zip", "fake.pdf"),
        Err(DomainError::UnsupportedContentType)
    ));
}

#[test]
fn the_declared_type_is_lower_cased_stripped_of_parameters_and_trimmed() {
    for declared in [
        "Application/PDF",
        "application/pdf; charset=binary",
        "  application/pdf  ",
        "APPLICATION/PDF ;name=x",
        "application/pdf;",
    ] {
        assert_eq!(ok(declared, "a.pdf").content_type, PDF, "`{declared}`");
    }
    assert_eq!(
        ok("TEXT/Plain; charset=utf-8", "a.txt").content_type,
        "text/plain"
    );
    assert_eq!(
        ok(" Application/Octet-Stream ; x=y", "a.pdf").content_type,
        PDF
    );
}

#[test]
fn unsupported_types_are_rejected() {
    assert!(
        classify(PDF, "file.pdf").is_ok(),
        "a supported type is accepted"
    );
    for declared in [
        "application/zip",
        "application/x-msdownload",
        "application/msword",
        "application/vnd.ms-excel",
        "image/svg+xml",
        "image/bmp",
        "image/tiff",
        "video/mp4",
        "audio/mpeg",
        "text/xml",
        "multipart/form-data",
        "pdf",
        "",
    ] {
        assert!(
            matches!(
                classify(declared, "file.pdf"),
                Err(DomainError::UnsupportedContentType)
            ),
            "`{declared}`"
        );
    }
}

// ---- CSV -----------------------------------------------------------------------------------------

#[test]
fn csv_is_stored_as_plain_text_when_csv_upload_is_allowed() {
    for (declared, name) in [
        ("text/csv", "data.csv"),
        ("application/octet-stream", "data.csv"),
    ] {
        let got = ok(declared, name);
        assert_eq!(got.content_type, "text/plain", "{declared}");
        assert_eq!(got.extension, "txt");
        assert_eq!(got.kind, AttachmentKind::Document);
        assert!(got.for_file_search && !got.for_code_interpreter);
    }
}

#[test]
fn csv_is_unsupported_when_csv_upload_is_disallowed() {
    let rules = ClassifyRules {
        allow_csv_upload: false,
        ..OPEN
    };

    for (declared, name) in [
        ("text/csv", "data.csv"),
        ("application/octet-stream", "data.csv"),
    ] {
        assert!(
            matches!(
                classify_file(declared, name, &rules),
                Err(DomainError::UnsupportedContentType)
            ),
            "{declared}"
        );
    }
    assert_eq!(
        classify_file("text/plain", "data.csv", &rules)
            .unwrap()
            .content_type,
        "text/plain",
        "plain text stays accepted"
    );
}

// ---- kind, purposes, kill switches ---------------------------------------------------------------------

#[test]
fn spreadsheets_are_code_interpreter_only_and_every_other_document_is_file_search_only() {
    let sheet = ok(XLSX, "a.xlsx");
    assert_eq!(sheet.kind, AttachmentKind::Document);
    assert!(sheet.for_code_interpreter);
    assert!(!sheet.for_file_search);

    for declared in [
        PDF,
        DOCX,
        PPTX,
        "text/plain",
        "application/json",
        "text/x-rust",
    ] {
        let got = ok(declared, "a.bin");
        assert!(
            got.for_file_search && !got.for_code_interpreter,
            "{declared}"
        );
    }
}

#[test]
fn images_have_neither_purpose() {
    for declared in [
        "image/png",
        "image/jpeg",
        "image/jpg",
        "image/webp",
        "image/gif",
    ] {
        let got = ok(declared, "a.bin");
        assert_eq!(got.kind, AttachmentKind::Image, "{declared}");
        assert!(
            !got.for_file_search && !got.for_code_interpreter,
            "{declared}"
        );
    }
}

#[test]
fn the_images_kill_switch_rejects_every_image_but_no_document() {
    let rules = ClassifyRules {
        disable_images: true,
        ..OPEN
    };

    for (declared, name) in [
        ("image/png", "a.png"),
        ("image/jpeg", "a.jpg"),
        ("image/jpg", "a.jpg"),
        ("image/webp", "a.webp"),
        ("image/gif", "a.gif"),
        ("application/octet-stream", "a.png"),
    ] {
        assert!(
            matches!(
                classify_file(declared, name, &rules),
                Err(DomainError::FeatureDisabled(DisabledFeature::Images))
            ),
            "{declared} {name}"
        );
    }
    assert!(classify_file(PDF, "a.pdf", &rules).is_ok());
    assert!(classify_file(XLSX, "a.xlsx", &rules).is_ok());
}

#[test]
fn an_unsupported_type_wins_over_the_images_kill_switch() {
    let rules = ClassifyRules {
        disable_images: true,
        ..OPEN
    };
    assert!(matches!(
        classify_file("image/png", "a.png", &rules),
        Err(DomainError::FeatureDisabled(DisabledFeature::Images))
    ));

    assert!(matches!(
        classify_file("image/bmp", "a.bmp", &rules),
        Err(DomainError::UnsupportedContentType)
    ));
}

#[test]
fn a_spreadsheet_without_the_code_interpreter_is_rejected() {
    let rules = ClassifyRules {
        code_interpreter_available: false,
        ..OPEN
    };

    for (declared, name) in [(XLSX, "a.xlsx"), ("application/octet-stream", "a.xlsx")] {
        assert!(
            matches!(
                classify_file(declared, name, &rules),
                Err(DomainError::CodeInterpreterUnavailable)
            ),
            "{declared}"
        );
    }
}

#[test]
fn an_unavailable_code_interpreter_does_not_touch_other_uploads() {
    let rules = ClassifyRules {
        code_interpreter_available: false,
        ..OPEN
    };

    let pdf = classify_file(PDF, "a.pdf", &rules).unwrap();
    assert!(pdf.for_file_search && !pdf.for_code_interpreter);
    assert!(classify_file("image/png", "a.png", &rules).is_ok());
    assert!(classify_file("text/plain", "a.txt", &rules).is_ok());
}

// ---- file name ------------------------------------------------------------------------------------------

#[test]
fn an_absent_or_empty_file_name_becomes_upload() {
    assert_eq!(DEFAULT_FILENAME, "upload");
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("")), "upload");
}

#[test]
fn a_file_name_up_to_255_characters_is_kept() {
    assert_eq!(normalize_filename(Some("report.pdf")), "report.pdf");
    let exact = format!("{}.pdf", "a".repeat(251));
    assert_eq!(exact.chars().count(), FILENAME_MAX_CHARS);
    assert_eq!(normalize_filename(Some(&exact)), exact);
    assert_eq!(
        normalize_filename(Some(&format!("{exact}x")))
            .chars()
            .count(),
        255
    );
}

#[test]
fn a_longer_file_name_is_cut_to_255_characters_keeping_the_extension() {
    let long = format!("{}.pdf", "a".repeat(300));

    let got = normalize_filename(Some(&long));

    assert_eq!(got.chars().count(), 255);
    assert!(got.ends_with(".pdf"), "{got}");
    assert!(got.starts_with("aaaa"));
}

#[test]
fn the_cut_counts_unicode_scalar_values_not_bytes() {
    let long = format!("{}.txt", "\u{e9}".repeat(300));

    let got = normalize_filename(Some(&long));

    assert_eq!(got.chars().count(), 255);
    assert!(got.ends_with(".txt"));
    assert!(got.starts_with("\u{e9}\u{e9}"));
    let emoji = "\u{1f600}".repeat(300);
    assert_eq!(normalize_filename(Some(&emoji)).chars().count(), 255);
}

#[test]
fn a_long_name_without_an_extension_is_cut_plainly() {
    let long = "b".repeat(400);

    assert_eq!(normalize_filename(Some(&long)), "b".repeat(255));
}

#[test]
fn a_leading_dot_is_not_an_extension_separator() {
    let long = format!(".{}", "c".repeat(400));

    let got = normalize_filename(Some(&long));

    assert_eq!(got.chars().count(), 255);
    assert!(got.starts_with(".ccc"));
}

#[test]
fn the_extension_is_the_text_from_the_last_dot() {
    let long = format!("{}.tar.gz", "d".repeat(300));

    let got = normalize_filename(Some(&long));

    assert_eq!(got.chars().count(), 255);
    assert!(got.ends_with(".gz"));
}

// ---- size limits -----------------------------------------------------------------------------------------

#[test]
fn the_size_limit_is_the_configured_kib_in_bytes() {
    assert_eq!(size_limit_bytes(25_600, 0), 26_214_400);
    assert_eq!(size_limit_bytes(5_120, 0), 5_242_880);
    assert_eq!(size_limit_bytes(10, 0), 10_240);
}

#[test]
fn a_smaller_model_limit_lowers_the_size_limit() {
    assert_eq!(size_limit_bytes(25_600, 1), 1_048_576);
    assert_eq!(size_limit_bytes(25_600, 10), 10_485_760);
    assert_eq!(size_limit_bytes(5_120, 2), 2_097_152);
}

#[test]
fn a_larger_model_limit_never_raises_the_size_limit() {
    assert_eq!(size_limit_bytes(25_600, 100), 26_214_400);
    assert_eq!(size_limit_bytes(5_120, 25), 5_242_880);
}

#[test]
fn a_model_limit_of_zero_imposes_no_cap() {
    assert_eq!(size_limit_bytes(1_024, 0), 1_048_576);
}

#[test]
fn the_size_limit_does_not_overflow_for_large_configuration_values() {
    assert_eq!(size_limit_bytes(u32::MAX, 0), u64::from(u32::MAX) * 1024);
}
