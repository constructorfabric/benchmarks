use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

use super::*;

fn encoded(w: u32, h: u32, format: ImageFormat) -> Vec<u8> {
    let img = RgbaImage::from_fn(w, h, |x, y| {
        let byte = |v: u32| u8::try_from(v % 256).unwrap();
        Rgba([byte(x), byte(y), byte(x + y), 255])
    });
    let img = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(img).to_rgb8())
    } else {
        DynamicImage::ImageRgba8(img)
    };
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

fn decode_webp(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory_with_format(bytes, ImageFormat::WebP).expect("valid WebP")
}

#[test]
fn landscape_fits_width_keeping_aspect_ratio() {
    let t = make_thumbnail(
        &encoded(300, 200, ImageFormat::Png),
        &ThumbnailConfig::default(),
    )
    .expect("thumbnail");
    assert_eq!((t.width, t.height), (128, 85));
    let img = decode_webp(&t.webp);
    assert_eq!((img.width(), img.height()), (128, 85));
}

#[test]
fn portrait_fits_height() {
    let t = make_thumbnail(
        &encoded(100, 400, ImageFormat::Jpeg),
        &ThumbnailConfig::default(),
    )
    .expect("thumbnail");
    assert_eq!((t.width, t.height), (32, 128));
}

#[test]
fn configured_box_is_used() {
    let cfg = ThumbnailConfig {
        width: 64,
        height: 32,
        ..ThumbnailConfig::default()
    };
    let t = make_thumbnail(&encoded(200, 200, ImageFormat::Gif), &cfg).expect("thumbnail");
    assert_eq!((t.width, t.height), (32, 32));
}

#[test]
fn small_image_is_not_upscaled() {
    let t = make_thumbnail(
        &encoded(40, 20, ImageFormat::WebP),
        &ThumbnailConfig::default(),
    )
    .expect("thumbnail");
    assert_eq!((t.width, t.height), (40, 20));
    assert_eq!(decode_webp(&t.webp).width(), 40);
}

#[test]
fn undecodable_bytes_give_none() {
    assert!(make_thumbnail(b"not an image", &ThumbnailConfig::default()).is_none());
}

#[test]
fn encoded_size_over_max_bytes_is_skipped() {
    let cfg = ThumbnailConfig {
        max_bytes: 10,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&encoded(300, 200, ImageFormat::Png), &cfg).is_none());
}

#[test]
fn input_over_max_decode_bytes_is_skipped() {
    let png = encoded(300, 200, ImageFormat::Png);
    let cfg = ThumbnailConfig {
        max_decode_bytes: png.len() - 1,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&png, &cfg).is_none());
}

#[test]
fn header_pixel_count_over_max_pixels_is_skipped() {
    let cfg = ThumbnailConfig {
        max_pixels: 300 * 200 - 1,
        ..ThumbnailConfig::default()
    };
    assert!(make_thumbnail(&encoded(300, 200, ImageFormat::Png), &cfg).is_none());
}

#[test]
fn estimated_decoded_size_over_max_decode_bytes_is_skipped() {
    let png = encoded(300, 200, ImageFormat::Png);
    // Bytes fit, but 300 * 200 * 4 does not.
    let cfg = ThumbnailConfig {
        max_decode_bytes: 300 * 200 * 4 - 1,
        ..ThumbnailConfig::default()
    };
    assert!(png.len() < cfg.max_decode_bytes);
    assert!(make_thumbnail(&png, &cfg).is_none());
}
