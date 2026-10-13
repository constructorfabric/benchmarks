#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Thumbnail tests: WebP output, fit-inside sizing, every image type and the
//! skip rules.

use std::io::Cursor;

use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use super::*;

fn limits() -> ThumbnailLimits {
    ThumbnailLimits::from_config(&ThumbnailConfig::default())
}

fn encoded(image: &DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    image.write_to(&mut out, format).unwrap();
    out.into_inner()
}

fn gradient(width: u32, height: u32) -> DynamicImage {
    DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        Rgb([
            u8::try_from(x % 256).unwrap(),
            u8::try_from(y % 256).unwrap(),
            128,
        ])
    }))
}

fn png(width: u32, height: u32) -> Vec<u8> {
    encoded(&gradient(width, height), ImageFormat::Png)
}

/// Decodes a WebP thumbnail and returns its size.
fn webp_size(data: &[u8]) -> (u32, u32) {
    assert_eq!(&data[..4], b"RIFF");
    assert_eq!(&data[8..12], b"WEBP");
    let image = image::load_from_memory_with_format(data, ImageFormat::WebP).unwrap();
    (image.width(), image.height())
}

#[test]
fn landscape_image_fits_the_box_as_webp() {
    let thumbnail = render_webp(&png(300, 150), &limits()).unwrap();

    assert_eq!((thumbnail.width, thumbnail.height), (128, 64));
    assert_eq!(webp_size(&thumbnail.data), (128, 64));
    assert!(thumbnail.data.len() <= limits().max_bytes);
}

#[test]
fn portrait_image_keeps_the_aspect_ratio() {
    let thumbnail = render_webp(&png(100, 400), &limits()).unwrap();

    assert_eq!((thumbnail.width, thumbnail.height), (32, 128));
    assert_eq!(webp_size(&thumbnail.data), (32, 128));
}

#[test]
fn small_image_keeps_its_size() {
    let thumbnail = render_webp(&png(40, 20), &limits()).unwrap();

    assert_eq!((thumbnail.width, thumbnail.height), (40, 20));
}

#[test]
fn configured_box_is_used() {
    let limits = ThumbnailLimits {
        width: 64,
        height: 32,
        ..limits()
    };
    let thumbnail = render_webp(&png(200, 200), &limits).unwrap();

    assert_eq!((thumbnail.width, thumbnail.height), (32, 32));
}

#[test]
fn every_supported_image_type_is_rendered() {
    let source = gradient(60, 30);
    for format in [
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::WebP,
    ] {
        let data = if format == ImageFormat::Gif {
            encoded(&DynamicImage::ImageRgba8(source.to_rgba8()), format)
        } else {
            encoded(&source, format)
        };
        let thumbnail = render_webp(&data, &limits()).unwrap();
        assert_eq!((thumbnail.width, thumbnail.height), (60, 30), "{format:?}");
        assert_eq!(webp_size(&thumbnail.data), (60, 30), "{format:?}");
    }
}

#[test]
fn transparent_image_is_rendered_with_alpha() {
    let image = DynamicImage::ImageRgba8(RgbaImage::from_pixel(300, 300, Rgba([10, 20, 30, 0])));
    let thumbnail = render_webp(&encoded(&image, ImageFormat::Png), &limits()).unwrap();

    let decoded = image::load_from_memory_with_format(&thumbnail.data, ImageFormat::WebP).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (128, 128));
    assert!(decoded.color().has_alpha());
}

#[test]
fn undecodable_data_is_skipped() {
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend_from_slice(&b"this is not a real png".repeat(10));

    assert!(matches!(
        render_webp(&data, &limits()),
        Err(ThumbnailSkip::Undecodable(_))
    ));
    assert!(matches!(
        render_webp(b"plain text", &limits()),
        Err(ThumbnailSkip::Undecodable(_))
    ));
    assert_eq!(WebpThumbnailer::new(limits()).render(&data), None);
}

#[test]
fn upload_above_max_decode_bytes_is_skipped() {
    let data = png(64, 64);
    let limits = ThumbnailLimits {
        max_decode_bytes: data.len() - 1,
        ..limits()
    };

    assert_eq!(
        render_webp(&data, &limits),
        Err(ThumbnailSkip::UploadTooLarge(data.len()))
    );
}

#[test]
fn too_many_pixels_are_skipped() {
    let limits = ThumbnailLimits {
        max_pixels: 100 * 100 - 1,
        ..limits()
    };

    assert_eq!(
        render_webp(&png(100, 100), &limits),
        Err(ThumbnailSkip::TooManyPixels(10_000))
    );
}

#[test]
fn estimated_decode_size_above_the_limit_is_skipped() {
    let data = png(200, 200);
    let limits = ThumbnailLimits {
        max_decode_bytes: 200 * 200 * 4 - 1,
        ..limits()
    };
    assert!(data.len() <= limits.max_decode_bytes);

    assert_eq!(
        render_webp(&data, &limits),
        Err(ThumbnailSkip::DecodeTooLarge(160_000))
    );
}

#[test]
fn oversized_encoded_thumbnail_is_skipped() {
    let limits = ThumbnailLimits {
        max_bytes: 16,
        ..limits()
    };

    assert!(matches!(
        render_webp(&png(300, 150), &limits),
        Err(ThumbnailSkip::EncodedTooLarge(_))
    ));
}

#[test]
fn renderer_returns_the_thumbnail() {
    let thumbnail = WebpThumbnailer::new(limits())
        .render(&png(256, 128))
        .unwrap();

    assert_eq!((thumbnail.width, thumbnail.height), (128, 64));
}

#[test]
fn fit_within_rounds_and_keeps_at_least_one_pixel() {
    assert_eq!(fit_within(300, 150, 128, 128), (128, 64));
    assert_eq!(fit_within(100, 400, 128, 128), (32, 128));
    assert_eq!(fit_within(1, 1000, 128, 128), (1, 128));
    assert_eq!(fit_within(1000, 3, 128, 128), (128, 1));
    assert_eq!(fit_within(128, 128, 128, 128), (128, 128));
    assert_eq!(fit_within(333, 100, 128, 128), (128, 38));
}
