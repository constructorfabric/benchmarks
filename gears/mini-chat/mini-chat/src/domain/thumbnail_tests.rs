use std::io::Cursor;

use image::{ImageFormat, RgbImage};

use super::*;

fn encode(w: u32, h: u32, fmt: ImageFormat) -> Vec<u8> {
    let img = RgbImage::from_fn(w, h, |x, y| {
        image::Rgb([
            u8::try_from(x % 256).unwrap_or(u8::MAX),
            u8::try_from(y % 256).unwrap_or(u8::MAX),
            128,
        ])
    });
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, fmt).expect("encode");
    out.into_inner()
}

#[test]
fn fit_size_keeps_aspect_ratio() {
    assert_eq!(fit_size(400, 200, 128, 128), (128, 64));
    assert_eq!(fit_size(200, 400, 128, 128), (64, 128));
    assert_eq!(fit_size(50, 20, 128, 128), (50, 20));
    assert_eq!(fit_size(1000, 1, 128, 128), (128, 1));
}

#[test]
fn generates_webp_within_bounds() {
    let cfg = ThumbnailConfig::default();
    for fmt in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Gif] {
        let t = generate(&encode(300, 150, fmt), &cfg).expect("thumbnail");
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.data[0..4], b"RIFF");
        assert_eq!(&t.data[8..12], b"WEBP");
        assert!(t.data.len() <= cfg.max_bytes);
    }
}

#[test]
fn skips_oversized_or_invalid_input() {
    let cfg = ThumbnailConfig {
        max_pixels: 100,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&encode(20, 20, ImageFormat::Png), &cfg).is_none());
    let cfg = ThumbnailConfig {
        max_decode_bytes: 10,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&encode(8, 8, ImageFormat::Png), &cfg).is_none());
    assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
    let cfg = ThumbnailConfig {
        max_bytes: 10,
        ..ThumbnailConfig::default()
    };
    assert!(generate(&encode(64, 64, ImageFormat::Png), &cfg).is_none());
}
