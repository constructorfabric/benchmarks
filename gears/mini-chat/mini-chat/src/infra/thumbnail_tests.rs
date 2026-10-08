#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::testing::images::{encode, png};

fn cfg() -> ThumbnailConfig {
    ThumbnailConfig::default()
}

fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
}

#[test]
fn landscape_png_fits_width_keeping_aspect_ratio() {
    let t = make_thumbnail(&png(400, 200), &cfg()).expect("thumbnail");
    assert_eq!((t.width, t.height), (128, 64));
    assert!(is_webp(&t.webp));
    let decoded = image::load_from_memory(&t.webp).expect("valid webp");
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
}

#[test]
fn portrait_image_fits_height() {
    let t = make_thumbnail(&png(100, 400), &cfg()).expect("thumbnail");
    assert_eq!((t.width, t.height), (32, 128));
}

#[test]
fn small_image_is_not_upscaled() {
    let t = make_thumbnail(&png(50, 40), &cfg()).expect("thumbnail");
    assert_eq!((t.width, t.height), (50, 40));
}

#[test]
fn custom_target_size() {
    let c = ThumbnailConfig {
        width: 64,
        height: 64,
        ..cfg()
    };
    let t = make_thumbnail(&png(400, 200), &c).expect("thumbnail");
    assert_eq!((t.width, t.height), (64, 32));
}

#[test]
fn jpeg_gif_and_webp_inputs_are_decoded() {
    for format in [
        image::ImageFormat::Jpeg,
        image::ImageFormat::Gif,
        image::ImageFormat::WebP,
    ] {
        let t = make_thumbnail(&encode(300, 300, format), &cfg())
            .unwrap_or_else(|| panic!("thumbnail of {format:?}"));
        assert_eq!((t.width, t.height), (128, 128), "{format:?}");
    }
}

#[test]
fn skipped_when_header_pixels_exceed_max_pixels() {
    let c = ThumbnailConfig {
        max_pixels: 400 * 200 - 1,
        ..cfg()
    };
    assert!(make_thumbnail(&png(400, 200), &c).is_none());
    let c = ThumbnailConfig {
        max_pixels: 400 * 200,
        ..cfg()
    };
    assert!(make_thumbnail(&png(400, 200), &c).is_some());
}

#[test]
fn skipped_when_estimated_decode_size_exceeds_max_decode_bytes() {
    let bytes = png(400, 200);
    let c = ThumbnailConfig {
        max_decode_bytes: 400 * 200 * 4 - 1,
        ..cfg()
    };
    assert!(bytes.len() < c.max_decode_bytes);
    assert!(make_thumbnail(&bytes, &c).is_none());
}

#[test]
fn skipped_when_input_is_larger_than_max_decode_bytes() {
    let bytes = png(10, 10);
    let c = ThumbnailConfig {
        max_decode_bytes: bytes.len() - 1,
        ..cfg()
    };
    assert!(make_thumbnail(&bytes, &c).is_none());
}

#[test]
fn skipped_when_webp_exceeds_max_bytes() {
    let c = ThumbnailConfig {
        max_bytes: 16,
        ..cfg()
    };
    assert!(make_thumbnail(&png(400, 200), &c).is_none());
}

#[test]
fn skipped_for_garbage_and_truncated_images() {
    assert!(make_thumbnail(b"definitely not an image", &cfg()).is_none());
    let bytes = png(400, 200);
    assert!(make_thumbnail(&bytes[..bytes.len() - 64], &cfg()).is_none());
}
