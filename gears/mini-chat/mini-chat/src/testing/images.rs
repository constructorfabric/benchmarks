//! Generated test images.

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, RgbImage};

/// A `width × height` RGB gradient encoded as `format`.
///
/// # Panics
/// When the image cannot be encoded.
#[must_use]
#[allow(clippy::expect_used, clippy::cast_possible_truncation)]
pub fn encode(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let img = RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
    });
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(img)
        .write_to(&mut out, format)
        .expect("encode test image");
    out.into_inner()
}

/// A `width × height` PNG.
#[must_use]
pub fn png(width: u32, height: u32) -> Vec<u8> {
    encode(width, height, ImageFormat::Png)
}
