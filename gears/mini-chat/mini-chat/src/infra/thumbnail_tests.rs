#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

use super::make_thumbnail;
use crate::config::ThumbnailConfig;

/// PNG of `w` x `h` with a horizontal gradient (not trivially compressible).
fn png(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        let c = |v: u32| u8::try_from(v % 256).unwrap();
        Rgba([c(x), c(y), c(x + y), 255])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// Single-colour PNG: tiny on disk, huge when decoded.
fn flat_png(w: u32, h: u32) -> Vec<u8> {
    let img = RgbaImage::from_pixel(w, h, Rgba([10, 20, 30, 255]));
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[test]
fn fits_inside_preserving_aspect() {
    let cfg = ThumbnailConfig::default();
    let t = make_thumbnail(&png(400, 200), &cfg).expect("thumbnail");
    assert_eq!((t.width, t.height), (128, 64));
    assert_eq!(&t.webp[0..4], b"RIFF");
    assert_eq!(&t.webp[8..12], b"WEBP");
    let decoded = image::load_from_memory_with_format(&t.webp, ImageFormat::WebP).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 64));

    let tall = make_thumbnail(&png(100, 300), &cfg).expect("thumbnail");
    assert_eq!((tall.width, tall.height), (43, 128));
}

#[test]
fn small_image_is_not_upscaled() {
    let t = make_thumbnail(&png(40, 20), &ThumbnailConfig::default()).expect("thumbnail");
    assert_eq!((t.width, t.height), (40, 20));
}

#[test]
fn skips_over_decode_limit() {
    let bytes = png(64, 64);
    // Upload larger than max_decode_bytes.
    let cfg = ThumbnailConfig {
        max_decode_bytes: bytes.len() - 1,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&bytes, &cfg).is_none());

    // Header width * height * 4 above max_decode_bytes (a small file).
    let bomb = flat_png(4000, 4000);
    assert!(bomb.len() < 1_000_000);
    assert!(make_thumbnail(&bomb, &ThumbnailConfig::default()).is_none());
    assert!(
        make_thumbnail(
            &bomb,
            &ThumbnailConfig {
                max_decode_bytes: 100_000_000,
                ..ThumbnailConfig::default()
            }
        )
        .is_some()
    );

    // Header pixel count above max_pixels.
    let cfg = ThumbnailConfig {
        max_pixels: 64 * 64 - 1,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&bytes, &cfg).is_none());
}

#[test]
fn skips_when_encoded_over_max_bytes() {
    let cfg = ThumbnailConfig {
        max_bytes: 16,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&png(400, 200), &cfg).is_none());
}

#[test]
fn invalid_bytes_none() {
    let cfg = ThumbnailConfig::default();
    assert!(make_thumbnail(b"definitely not an image", &cfg).is_none());
    assert!(make_thumbnail(&[], &cfg).is_none());
    let mut truncated = png(64, 64);
    truncated.truncate(100);
    assert!(make_thumbnail(&truncated, &cfg).is_none());
}
