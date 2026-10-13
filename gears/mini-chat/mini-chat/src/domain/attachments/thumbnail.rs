//! Preview thumbnails of uploaded images (DESIGN 3.6 "File Upload", B.8
//! `thumbnail.*`): a WebP that fits the configured box, aspect ratio kept.
//!
//! Best effort: an image that is too large, too expensive to decode or not
//! decodable at all simply has no thumbnail (the upload is not affected).

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use super::NewThumbnail;
use crate::config::ThumbnailConfig;

/// The thumbnail of `data`, or `None` when none can be made within the limits
/// of `config`.
#[must_use]
pub(super) fn generate(data: &[u8], config: &ThumbnailConfig) -> Option<NewThumbnail> {
    let (width, height) = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if u64::from(width) * u64::from(height) > config.max_pixels {
        return None;
    }

    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(u64::try_from(config.max_decode_bytes).unwrap_or(u64::MAX));
    reader.limits(limits);
    let decoded = reader.decode().ok()?;

    // Never upscale: an image inside the box keeps its size.
    let fitted = if width > config.width || height > config.height {
        decoded.thumbnail(config.width, config.height)
    } else {
        decoded
    };
    let out_width = i32::try_from(fitted.width()).ok()?;
    let out_height = i32::try_from(fitted.height()).ok()?;

    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(fitted.to_rgba8())
        .write_to(&mut encoded, ImageFormat::WebP)
        .ok()?;
    let bytes = encoded.into_inner();
    (bytes.len() <= config.max_bytes).then_some(NewThumbnail {
        bytes,
        width: out_width,
        height: out_height,
    })
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod tests;
