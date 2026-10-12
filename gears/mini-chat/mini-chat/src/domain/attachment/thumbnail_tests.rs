#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Cursor;

use bytes::Bytes;
use image::{DynamicImage, ImageFormat, RgbImage};

use super::thumbnail::generate;
use crate::config::ThumbnailConfig;

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([
            u8::try_from(x % 251).unwrap(),
            u8::try_from(y % 241).unwrap(),
            u8::try_from((x * 7 + y * 13) % 239).unwrap(),
        ])
    });
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(image)
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

fn thumbnail_of(bytes: &[u8], config: &ThumbnailConfig) -> Option<(u32, u32, Vec<u8>)> {
    let chunks = [Bytes::copy_from_slice(bytes)];
    generate(&chunks, u64::try_from(bytes.len()).unwrap(), config)
        .map(|thumbnail| (thumbnail.width, thumbnail.height, thumbnail.data))
}

#[test]
fn the_thumbnail_fits_the_box_and_keeps_the_aspect_ratio() {
    let config = ThumbnailConfig::default();
    assert_eq!(
        thumbnail_of(&png(640, 320), &config).map(|t| (t.0, t.1)),
        Some((128, 64))
    );
    assert_eq!(
        thumbnail_of(&png(150, 300), &config).map(|t| (t.0, t.1)),
        Some((64, 128))
    );
}

#[test]
fn a_small_image_is_not_enlarged() {
    let config = ThumbnailConfig::default();
    assert_eq!(
        thumbnail_of(&png(40, 20), &config).map(|t| (t.0, t.1)),
        Some((40, 20))
    );
}

#[test]
fn the_output_is_a_webp_container() {
    let (_, _, data) = thumbnail_of(&png(256, 256), &ThumbnailConfig::default()).unwrap();
    assert_eq!(&data[..4], b"RIFF");
    assert_eq!(&data[8..12], b"WEBP");
}

#[test]
fn the_configured_box_applies() {
    let config = ThumbnailConfig {
        width: 32,
        height: 32,
        ..ThumbnailConfig::default()
    };
    assert_eq!(
        thumbnail_of(&png(100, 50), &config).map(|t| (t.0, t.1)),
        Some((32, 16))
    );
}

#[test]
fn undecodable_bytes_and_every_limit_skip_the_thumbnail() {
    let image = png(64, 64);
    assert!(thumbnail_of(b"not an image", &ThumbnailConfig::default()).is_none());
    let tiny_output = ThumbnailConfig {
        max_bytes: 8,
        ..ThumbnailConfig::default()
    };
    assert!(thumbnail_of(&image, &tiny_output).is_none());
    let few_pixels = ThumbnailConfig {
        max_pixels: 100,
        ..ThumbnailConfig::default()
    };
    assert!(thumbnail_of(&image, &few_pixels).is_none());
    let small_cap = ThumbnailConfig {
        max_decode_bytes: 200,
        ..ThumbnailConfig::default()
    };
    assert!(thumbnail_of(&image, &small_cap).is_none());
}

#[test]
fn the_decoded_size_estimate_is_checked_against_the_decode_cap() {
    // 64 x 64 x 4 = 16384 bytes of pixels: above a cap that the file itself fits under.
    let image = png(64, 64);
    let config = ThumbnailConfig {
        max_decode_bytes: image.len() + 1,
        ..ThumbnailConfig::default()
    };
    assert!(image.len() + 1 < 64 * 64 * 4);
    assert!(thumbnail_of(&image, &config).is_none());
}
