use super::*;

#[test]
fn images_are_classified_as_image_without_purposes() {
    for ct in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
        let r = classify(ct, true).unwrap();
        assert_eq!(r.kind, Kind::Image);
        assert!(!r.for_file_search && !r.for_code_interpreter);
    }
}

#[test]
fn xlsx_is_code_interpreter_only_and_documents_go_to_file_search() {
    let r = classify(XLSX, true).unwrap();
    assert_eq!(r.kind, Kind::Document);
    assert!(r.for_code_interpreter && !r.for_file_search);
    let r = classify("application/pdf", true).unwrap();
    assert!(r.for_file_search && !r.for_code_interpreter);
}

#[test]
fn csv_is_remapped_or_rejected() {
    assert_eq!(classify("text/csv", true).unwrap().content_type, "text/plain");
    assert!(classify("text/csv", false).is_none());
}

#[test]
fn unsupported_types_are_rejected() {
    assert!(classify("application/octet-stream", true).is_none());
    assert!(classify("video/mp4", true).is_none());
}

#[test]
fn content_type_parameters_and_case_are_ignored() {
    assert_eq!(classify("Text/Plain; charset=utf-8", true).unwrap().content_type, "text/plain");
}

#[test]
fn octet_stream_is_inferred_from_extension() {
    assert_eq!(effective_content_type("application/octet-stream", "report.PDF"), "application/pdf");
    assert_eq!(effective_content_type("application/octet-stream", "a.xlsx"), XLSX);
    assert_eq!(effective_content_type("application/octet-stream", "pic.jpg"), "image/jpeg");
    assert_eq!(effective_content_type("application/octet-stream", "blob.bin"), "application/octet-stream");
    assert_eq!(effective_content_type("text/plain", "x.pdf"), "text/plain");
}

#[test]
fn filename_defaults_and_truncation_keep_extension() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("  ")), "upload");
    let long = format!("{}.pdf", "a".repeat(300));
    let n = normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(std::path::Path::new(&n).extension().is_some_and(|ext| ext == "pdf"));
}
