#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;

use image::{ImageFormat, Rgba, RgbaImage};

use super::*;

/// Encodes a `w` x `h` gradient image in `format`.
pub(crate) fn encode_image(w: u32, h: u32, format: ImageFormat) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
    });
    let mut out = Cursor::new(Vec::new());
    let dynimg = image::DynamicImage::ImageRgba8(img);
    if format == ImageFormat::Jpeg {
        image::DynamicImage::ImageRgb8(dynimg.to_rgb8())
            .write_to(&mut out, format)
            .unwrap();
    } else {
        dynimg.write_to(&mut out, format).unwrap();
    }
    out.into_inner()
}

fn is_webp(data: &[u8]) -> bool {
    data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

#[test]
fn png_is_resized_to_fit_keeping_aspect_ratio() {
    let png = encode_image(300, 150, ImageFormat::Png);
    let t = generate(&png, &ThumbnailConfig::default()).expect("thumbnail");
    assert_eq!((t.width, t.height), (128, 64));
    assert!(is_webp(&t.data));
    let decoded = image::load_from_memory(&t.data).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
}

#[test]
fn portrait_jpeg_fits_height() {
    let jpg = encode_image(100, 400, ImageFormat::Jpeg);
    let t = generate(&jpg, &ThumbnailConfig::default()).expect("thumbnail");
    assert_eq!((t.width, t.height), (32, 128));
}

#[test]
fn custom_box_and_no_upscale() {
    let png = encode_image(40, 20, ImageFormat::Png);
    let t = generate(&png, &ThumbnailConfig::default()).expect("thumbnail");
    assert_eq!((t.width, t.height), (40, 20));

    let cfg = ThumbnailConfig {
        width: 10,
        height: 10,
        ..ThumbnailConfig::default()
    };
    let t = generate(&png, &cfg).expect("thumbnail");
    assert_eq!((t.width, t.height), (10, 5));
}

#[test]
fn gif_and_webp_sources_are_supported() {
    for f in [ImageFormat::Gif, ImageFormat::WebP] {
        let data = encode_image(64, 64, f);
        assert!(generate(&data, &ThumbnailConfig::default()).is_some(), "{f:?}");
    }
}

#[test]
fn safeguards_skip_generation() {
    let png = encode_image(300, 150, ImageFormat::Png);
    // Input larger than max_decode_bytes.
    let cfg = ThumbnailConfig {
        max_decode_bytes: png.len() - 1,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&png, &cfg).is_none());
    // Header pixel count above max_pixels.
    let cfg = ThumbnailConfig {
        max_pixels: 300 * 150 - 1,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&png, &cfg).is_none());
    // Estimated decoded size (w*h*4) above max_decode_bytes.
    let cfg = ThumbnailConfig {
        max_decode_bytes: 300 * 150 * 4 - 1,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&png, &cfg).is_none());
    // Encoded thumbnail above max_bytes.
    let cfg = ThumbnailConfig {
        max_bytes: 16,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&png, &cfg).is_none());
}

#[test]
fn garbage_and_empty_input_yield_none() {
    let cfg = ThumbnailConfig::default();
    assert!(generate(b"", &cfg).is_none());
    assert!(generate(b"definitely not an image", &cfg).is_none());
    // Valid PNG signature, truncated body.
    let png = encode_image(64, 64, ImageFormat::Png);
    assert!(generate(&png[..40], &cfg).is_none());
}
