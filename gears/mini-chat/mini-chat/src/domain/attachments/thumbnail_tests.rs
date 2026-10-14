//! Image thumbnail rules: WebP output fitted inside the configured box, no upscaling surprises,
//! the header pre-screening (pixels and estimated decoded size), the encoded-size bound, decode
//! failures and the not-kept skip. A skip never fails.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail`; definition of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-images`.
//!
//! The `image` crate (features `jpeg`, `png`, `gif`, `webp`) is a dependency of the crate and
//! present in `Cargo.lock`, so these tests decode and encode real images.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use image::ImageFormat;

use super::generate_thumbnail;
use crate::config::ThumbnailConfig;
use crate::test_support::attachments::{gif, jpeg, png, png_header_only, webp};

fn defaults() -> ThumbnailConfig {
    ThumbnailConfig::default()
}

fn decoded_dimensions(bytes: &[u8]) -> (u32, u32) {
    let image = image::load_from_memory_with_format(bytes, ImageFormat::WebP)
        .expect("the thumbnail is a decodable WebP");
    (image.width(), image.height())
}

#[test]
fn a_png_becomes_a_webp_that_fits_the_default_box_keeping_the_aspect_ratio() {
    // inst-th-resize, inst-th-encode, inst-th-return
    let thumbnail =
        generate_thumbnail(Some(&png(400, 200)), "image/png", &defaults()).expect("a thumbnail");
    assert_eq!(
        decoded_dimensions(&thumbnail.bytes),
        (thumbnail.width, thumbnail.height)
    );
    assert_eq!((thumbnail.width, thumbnail.height), (128, 64));
}

#[test]
fn a_portrait_image_is_fitted_by_its_height_without_cropping() {
    let thumbnail =
        generate_thumbnail(Some(&png(100, 400)), "image/png", &defaults()).expect("a thumbnail");
    assert_eq!((thumbnail.width, thumbnail.height), (32, 128));
}

#[test]
fn the_box_follows_the_configuration() {
    let config = ThumbnailConfig {
        width: 64,
        height: 32,
        ..defaults()
    };
    let thumbnail =
        generate_thumbnail(Some(&png(640, 640)), "image/png", &config).expect("a thumbnail");
    assert!(
        thumbnail.width <= 64 && thumbnail.height <= 32,
        "{thumbnail:?}"
    );
    assert_eq!(thumbnail.height, 32);
    assert_eq!(thumbnail.width, 32);
}

#[test]
fn jpeg_gif_and_webp_sources_are_thumbnailed_too() {
    for (name, bytes, mime) in [
        ("jpeg", jpeg(300, 150), "image/jpeg"),
        ("gif", gif(300, 150), "image/gif"),
        ("webp", webp(300, 150), "image/webp"),
    ] {
        let thumbnail = generate_thumbnail(Some(&bytes), mime, &defaults())
            .unwrap_or_else(|| panic!("{name}: no thumbnail"));
        assert_eq!((thumbnail.width, thumbnail.height), (128, 64), "{name}");
        assert_eq!(
            decoded_dimensions(&thumbnail.bytes),
            (128, 64),
            "{name}: the encoded dimensions"
        );
    }
}

#[test]
fn the_output_is_webp_whatever_the_source_format() {
    let thumbnail =
        generate_thumbnail(Some(&jpeg(200, 200)), "image/jpeg", &defaults()).expect("a thumbnail");
    assert_eq!(&thumbnail.bytes[0..4], b"RIFF");
    assert_eq!(&thumbnail.bytes[8..12], b"WEBP");
}

#[test]
fn an_image_that_was_not_kept_in_memory_is_skipped() {
    // inst-th-not-kept: larger than thumbnail.max_decode_bytes during the upload
    assert!(generate_thumbnail(None, "image/png", &defaults()).is_none());
}

#[test]
fn a_header_above_max_pixels_is_skipped_even_when_the_image_is_decodable() {
    // inst-th-prescreen: width * height above thumbnail.max_pixels
    let config = ThumbnailConfig {
        max_pixels: 100,
        ..defaults()
    };
    assert!(generate_thumbnail(Some(&png(20, 20)), "image/png", &config).is_none());
    assert!(generate_thumbnail(Some(&png(10, 10)), "image/png", &config).is_some());
}

#[test]
fn a_header_whose_decoded_size_exceeds_the_decode_cap_is_skipped() {
    // inst-th-prescreen: width * height * 4 above thumbnail.max_decode_bytes
    let config = ThumbnailConfig {
        max_decode_bytes: 1000,
        ..defaults()
    };
    assert!(generate_thumbnail(Some(&png(20, 20)), "image/png", &config).is_none()); // 1600
    assert!(generate_thumbnail(Some(&png(15, 15)), "image/png", &config).is_some()); // 900
}

#[test]
fn a_pixel_bomb_header_is_skipped() {
    // 40000 x 40000 = 1.6e9 pixels, far above the default 1e8
    assert!(
        generate_thumbnail(
            Some(&png_header_only(40_000, 40_000)),
            "image/png",
            &defaults()
        )
        .is_none()
    );
}

#[test]
fn an_encoded_thumbnail_above_max_bytes_is_skipped_without_a_retry() {
    // inst-th-encode: skipped, no retry at lower quality
    let config = ThumbnailConfig {
        max_bytes: 10,
        ..defaults()
    };
    assert!(generate_thumbnail(Some(&png(400, 200)), "image/png", &config).is_none());
}

#[test]
fn an_encoded_thumbnail_within_max_bytes_is_returned_and_bounded() {
    let config = defaults();
    let thumbnail =
        generate_thumbnail(Some(&png(400, 200)), "image/png", &config).expect("a thumbnail");
    assert!(thumbnail.bytes.len() <= config.max_bytes);
}

#[test]
fn malformed_bytes_are_skipped() {
    // inst-th-decode: any decode failure
    assert!(generate_thumbnail(Some(b"not an image at all"), "image/png", &defaults()).is_none());
    assert!(generate_thumbnail(Some(&[]), "image/png", &defaults()).is_none());
}

#[test]
fn a_truncated_image_is_skipped() {
    let mut bytes = png(200, 200);
    bytes.truncate(bytes.len().div_ceil(2));
    assert!(generate_thumbnail(Some(&bytes), "image/png", &defaults()).is_none());
}

#[test]
fn a_header_only_png_within_the_limits_fails_to_decode_and_is_skipped() {
    // the header passes the pre-screening, the pixel data is a stub: the decode fails
    assert!(generate_thumbnail(Some(&png_header_only(64, 64)), "image/png", &defaults()).is_none());
}
