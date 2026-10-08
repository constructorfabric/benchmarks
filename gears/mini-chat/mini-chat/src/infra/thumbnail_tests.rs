use super::*;
use crate::config::ThumbnailConfig;

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

#[test]
fn generates_webp_fitting_the_box_with_aspect_ratio() {
    let t = generate(&png(400, 200), &ThumbnailConfig::default()).unwrap();
    assert_eq!((t.width, t.height), (128, 64));
    assert_eq!(&t.data[0..4], b"RIFF");
    assert_eq!(&t.data[8..12], b"WEBP");
}

#[test]
fn small_images_are_not_upscaled() {
    let t = generate(&png(20, 10), &ThumbnailConfig::default()).unwrap();
    assert_eq!((t.width, t.height), (20, 10));
}

#[test]
fn skipped_when_over_limits_or_undecodable() {
    let cfg = ThumbnailConfig { max_pixels: 100, ..ThumbnailConfig::default() };
    assert!(generate(&png(20, 10), &cfg).is_none());
    let cfg = ThumbnailConfig { max_bytes: 10, ..ThumbnailConfig::default() };
    assert!(generate(&png(50, 50), &cfg).is_none());
    let cfg = ThumbnailConfig { max_decode_bytes: 100, ..ThumbnailConfig::default() };
    assert!(generate(&png(50, 50), &cfg).is_none());
    assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
}
