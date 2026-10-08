use super::{is_image, mime_from_extension, normalize_filename, resolve_mime};
use crate::domain::error::DomainError;

#[test]
fn mime_resolution() {
    assert_eq!(resolve_mime("application/pdf", "a.pdf", true).ok().as_deref(), Some("application/pdf"));
    assert_eq!(resolve_mime("application/octet-stream", "x.PNG", true).ok().as_deref(), Some("image/png"));
    assert_eq!(resolve_mime("text/csv; charset=utf-8", "x.csv", true).ok().as_deref(), Some("text/plain"));
    assert!(matches!(resolve_mime("text/csv", "x.csv", false), Err(DomainError::UnsupportedContentType(_))));
    assert!(matches!(resolve_mime("application/octet-stream", "x.zzz", true), Err(DomainError::UnsupportedContentType(_))));
    assert!(matches!(resolve_mime("video/mp4", "x.mp4", true), Err(DomainError::UnsupportedContentType(_))));
    assert!(is_image("image/gif"));
    assert_eq!(mime_from_extension("a.xlsx"), Some(super::XLSX));
}

#[test]
fn filename_defaults_and_truncation() {
    assert_eq!(normalize_filename(None), "upload");
    assert_eq!(normalize_filename(Some("  ")), "upload");
    let long = format!("{}.pdf", "a".repeat(300));
    let n = normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(std::path::Path::new(&n).extension().is_some_and(|e| e == "pdf"));
}
