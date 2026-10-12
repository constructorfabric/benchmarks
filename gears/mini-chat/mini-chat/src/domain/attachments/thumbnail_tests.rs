//! Unit tests of the thumbnail rules.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn config(patch: &serde_json::Value) -> ThumbnailConfig {
    let mut base = serde_json::json!({});
    if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
        base.extend(patch.clone());
    }
    serde_json::from_value(base).unwrap()
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let buffer = image::ImageBuffer::from_fn(width, height, |x, y| {
        image::Rgb([
            u8::try_from(x % 256).unwrap(),
            u8::try_from(y % 256).unwrap(),
            9,
        ])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(buffer)
        .write_to(&mut out, ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[test]
fn a_large_image_is_scaled_into_the_box_keeping_its_ratio() {
    let thumbnail = generate(&png(400, 200), &config(&serde_json::json!({}))).unwrap();
    assert_eq!((thumbnail.width, thumbnail.height), (128, 64));
    assert_eq!(&thumbnail.bytes[..4], b"RIFF");
    assert_eq!(&thumbnail.bytes[8..12], b"WEBP");
}

#[test]
fn a_small_image_is_not_upscaled() {
    let thumbnail = generate(&png(40, 20), &config(&serde_json::json!({}))).unwrap();
    assert_eq!((thumbnail.width, thumbnail.height), (40, 20));
}

#[test]
fn an_image_over_a_limit_or_not_an_image_has_no_thumbnail() {
    assert!(
        generate(
            &png(40, 40),
            &config(&serde_json::json!({"max_pixels": 100}))
        )
        .is_none()
    );
    assert!(
        generate(
            &png(300, 300),
            &config(&serde_json::json!({"max_bytes": 1}))
        )
        .is_none()
    );
    assert!(generate(b"not an image", &config(&serde_json::json!({}))).is_none());
}
