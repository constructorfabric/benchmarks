//! Unit tests: MIME rules, filenames and thumbnails.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::mime::{self, AttachmentKind};
use super::test_support::png;
use super::thumbnail;
use crate::config::ThumbnailConfig;

fn thumb_cfg() -> ThumbnailConfig {
    ThumbnailConfig { width: 128, height: 128, max_bytes: 131_072, max_pixels: 100_000_000, max_decode_bytes: 33_554_432 }
}

#[test]
fn octet_stream_is_inferred_from_extension() {
    assert_eq!(mime::effective_type("application/octet-stream", "a.PDF"), mime::PDF);
    assert_eq!(mime::effective_type("application/octet-stream", "sheet.xlsx"), mime::XLSX);
    assert_eq!(mime::effective_type("application/octet-stream", "p.jpeg"), mime::JPEG);
    assert_eq!(mime::effective_type("application/octet-stream", "x.exe"), mime::OCTET_STREAM);
    assert_eq!(mime::effective_type("Text/Plain; charset=utf-8", "x.bin"), mime::TEXT);
}

#[test]
fn purposes_follow_the_media_type() {
    let pdf = mime::resolve(mime::PDF, false).unwrap();
    assert_eq!((pdf.kind, pdf.for_file_search, pdf.for_code_interpreter), (AttachmentKind::Document, true, false));
    let xlsx = mime::resolve(mime::XLSX, false).unwrap();
    assert_eq!((xlsx.kind, xlsx.for_file_search, xlsx.for_code_interpreter), (AttachmentKind::Document, false, true));
    let png_t = mime::resolve(mime::PNG, false).unwrap();
    assert_eq!((png_t.kind, png_t.for_file_search, png_t.for_code_interpreter), (AttachmentKind::Image, false, false));
    assert!(mime::resolve("application/x-msdownload", true).is_none());
    assert!(mime::resolve(mime::OCTET_STREAM, true).is_none());
}

#[test]
fn csv_only_when_allowed_and_stored_as_text() {
    assert!(mime::resolve(mime::CSV, false).is_none());
    assert_eq!(mime::resolve(mime::CSV, true).unwrap().content_type, mime::TEXT);
}

#[test]
fn filenames_default_and_truncate_keeping_extension() {
    assert_eq!(mime::normalize_filename(None), "upload");
    assert_eq!(mime::normalize_filename(Some("  ")), "upload");
    assert_eq!(mime::normalize_filename(Some("a.pdf")), "a.pdf");
    let long = format!("{}.pdf", "\u{e9}".repeat(400));
    let n = mime::normalize_filename(Some(&long));
    assert_eq!(n.chars().count(), 255);
    assert!(std::path::Path::new(&n).extension().is_some_and(|e| e == "pdf"));
    let no_ext = "x".repeat(300);
    assert_eq!(mime::normalize_filename(Some(&no_ext)).chars().count(), 255);
}

#[test]
fn provider_extension_matches_type() {
    assert_eq!(mime::provider_extension(mime::PDF), "pdf");
    assert_eq!(mime::provider_extension(mime::TEXT), "txt");
    assert_eq!(mime::provider_extension(mime::XLSX), "xlsx");
}

#[test]
fn thumbnail_fits_box_and_keeps_aspect() {
    let t = thumbnail::generate(&png(400, 200), &thumb_cfg()).expect("thumbnail");
    assert_eq!((t.width, t.height), (128, 64));
    let decoded = image::load_from_memory_with_format(&t.bytes, image::ImageFormat::WebP).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
}

#[test]
fn small_images_are_not_upscaled() {
    let t = thumbnail::generate(&png(40, 30), &thumb_cfg()).expect("thumbnail");
    assert_eq!((t.width, t.height), (40, 30));
}

#[test]
fn thumbnail_skipped_for_garbage_and_limits() {
    assert!(thumbnail::generate(b"not an image", &thumb_cfg()).is_none());
    let img = png(300, 300);
    let mut cfg = thumb_cfg();
    cfg.max_pixels = 1000;
    assert!(thumbnail::generate(&img, &cfg).is_none(), "header pixels above max_pixels");
    let mut cfg = thumb_cfg();
    cfg.max_decode_bytes = 300 * 300 * 4 - 1;
    assert!(thumbnail::generate(&img, &cfg).is_none(), "estimated decode size above the limit");
    let mut cfg = thumb_cfg();
    cfg.max_decode_bytes = img.len() - 1;
    assert!(thumbnail::generate(&img, &cfg).is_none(), "source larger than max_decode_bytes");
    let mut cfg = thumb_cfg();
    cfg.max_bytes = 16;
    assert!(thumbnail::generate(&img, &cfg).is_none(), "encoded thumbnail above max_bytes");
}
