#![allow(clippy::unwrap_used)]

use std::io::Cursor;

use super::generate;
use crate::config::ThumbnailConfig;

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 255) as u8, (y % 255) as u8, 7]));
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn fits_inside_box_preserving_aspect_ratio() {
    let t = generate(&png(400, 200), &ThumbnailConfig::default()).unwrap();
    assert_eq!((t.width, t.height), (128, 64));
    assert_eq!(&t.bytes[..4], b"RIFF");
    assert_eq!(&t.bytes[8..12], b"WEBP");
}

#[test]
fn limits_skip_generation() {
    let data = png(64, 64);
    let cfg = ThumbnailConfig { max_bytes: 10, ..ThumbnailConfig::default() };
    assert!(generate(&data, &cfg).is_none());
    let cfg = ThumbnailConfig { max_pixels: 100, ..ThumbnailConfig::default() };
    assert!(generate(&data, &cfg).is_none());
    let cfg = ThumbnailConfig { max_decode_bytes: 50, ..ThumbnailConfig::default() };
    assert!(generate(&data, &cfg).is_none());
    assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
}
