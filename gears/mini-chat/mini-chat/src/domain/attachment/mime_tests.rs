#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::mime::{
    DEFAULT_FILENAME, MAX_FILENAME_CHARS, XLSX_CONTENT_TYPE, file_extension, normalize_filename,
    resolve_content_type,
};
use super::model::AttachmentKind;

fn resolve(declared: &str, filename: &str) -> Option<(String, AttachmentKind, bool, bool)> {
    resolve_content_type(declared, filename, true).map(|resolved| {
        (
            resolved.content_type,
            resolved.kind,
            resolved.purposes.file_search,
            resolved.purposes.code_interpreter,
        )
    })
}

#[test]
fn documents_are_routed_to_file_search_and_xlsx_to_the_code_interpreter() {
    assert_eq!(
        resolve("application/pdf", "a.pdf"),
        Some((
            "application/pdf".to_owned(),
            AttachmentKind::Document,
            true,
            false
        ))
    );
    assert_eq!(
        resolve(XLSX_CONTENT_TYPE, "a.xlsx"),
        Some((
            XLSX_CONTENT_TYPE.to_owned(),
            AttachmentKind::Document,
            false,
            true
        ))
    );
    assert_eq!(
        resolve("text/x-python", "a.py"),
        Some((
            "text/x-python".to_owned(),
            AttachmentKind::Document,
            true,
            false
        ))
    );
}

#[test]
fn images_have_no_purposes() {
    for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        assert_eq!(
            resolve(mime, "x"),
            Some((mime.to_owned(), AttachmentKind::Image, false, false))
        );
    }
}

#[test]
fn the_declared_type_is_normalized_and_unknown_types_are_refused() {
    assert!(resolve("Application/PDF; charset=binary", "a").is_some());
    for mime in ["application/zip", "video/mp4", "image/svg+xml", ""] {
        assert!(resolve(mime, "a.zip").is_none(), "{mime}");
    }
}

#[test]
fn octet_stream_takes_the_type_of_the_extension() {
    let octet = "application/octet-stream";
    assert_eq!(resolve(octet, "Report.PDF").unwrap().0, "application/pdf");
    assert_eq!(
        resolve(octet, "photo.jpeg").unwrap().1,
        AttachmentKind::Image
    );
    assert_eq!(resolve(octet, "data.xlsx").unwrap().0, XLSX_CONTENT_TYPE);
    assert!(resolve(octet, "archive.bin").is_none());
    assert!(resolve(octet, "noextension").is_none());
    assert!(
        resolve(octet, ".pdf").is_none(),
        "a leading dot is no extension"
    );
}

#[test]
fn csv_is_stored_as_plain_text_when_allowed() {
    assert_eq!(resolve("text/csv", "a.csv").unwrap().0, "text/plain");
    assert_eq!(
        resolve("application/octet-stream", "a.csv").unwrap().0,
        "text/plain"
    );
    assert!(resolve_content_type("text/csv", "a.csv", false).is_none());
    assert!(resolve_content_type("application/octet-stream", "a.csv", false).is_none());
}

#[test]
fn a_missing_or_blank_filename_is_upload() {
    for raw in [None, Some(""), Some("   "), Some("\r\n")] {
        assert_eq!(normalize_filename(raw), DEFAULT_FILENAME);
    }
}

#[test]
fn paths_and_control_characters_are_stripped() {
    assert_eq!(normalize_filename(Some("C:\\docs\\a.pdf")), "a.pdf");
    assert_eq!(normalize_filename(Some("../../etc/passwd")), "passwd");
    assert_eq!(normalize_filename(Some("a\u{0}b\n.txt")), "ab.txt");
}

#[test]
fn long_filenames_are_cut_to_255_characters_keeping_the_extension() {
    let long = format!("{}.pdf", "a".repeat(300));
    let cut = normalize_filename(Some(&long));
    assert_eq!(cut.chars().count(), MAX_FILENAME_CHARS);
    assert_eq!(cut, format!("{}.pdf", "a".repeat(251)));

    let no_extension = "b".repeat(300);
    assert_eq!(normalize_filename(Some(&no_extension)), "b".repeat(255));

    let exact = format!("{}.pdf", "c".repeat(251));
    assert_eq!(normalize_filename(Some(&exact)), exact);

    let multibyte = "\u{e9}".repeat(300);
    assert_eq!(normalize_filename(Some(&multibyte)).chars().count(), 255);
}

#[test]
fn an_extension_as_long_as_the_limit_is_cut_with_the_name() {
    let name = format!("a.{}", "x".repeat(299));
    assert_eq!(normalize_filename(Some(&name)).chars().count(), 255);
}

#[test]
fn extensions_need_a_stem_and_a_suffix() {
    assert_eq!(file_extension("a.pdf"), Some("pdf"));
    assert_eq!(file_extension("a.tar.gz"), Some("gz"));
    assert_eq!(file_extension(".hidden"), None);
    assert_eq!(file_extension("trailing."), None);
    assert_eq!(file_extension("plain"), None);
}
