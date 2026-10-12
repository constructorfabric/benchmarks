//! Image thumbnails (DESIGN "Thumbnail generation details").
//!
//! The thumbnail fits `width` x `height` with the aspect ratio preserved (never enlarged), is
//! encoded as WebP and is at most `max_bytes`. Generation is best effort: any skip or failure
//! yields `None` and the attachment still becomes `ready`.

use std::io::Cursor;

use bytes::Bytes;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, imageops::FilterType};
use tracing::debug;

use crate::config::ThumbnailConfig;
use crate::domain::message::Thumbnail;

/// Bytes per pixel of the decoded image the pre-screening assumes (RGBA).
const DECODED_BYTES_PER_PIXEL: u64 = 4;

/// The size of the thumbnail box: the target dimensions scaled to fit, never above the source.
fn fitted(source: (u32, u32), target: (u32, u32)) -> (u32, u32) {
    let (width, height) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (target_width, target_height) = (u64::from(target.0.max(1)), u64::from(target.1.max(1)));
    if width <= target_width && height <= target_height {
        return (source.0.max(1), source.1.max(1));
    }
    // The limiting side keeps the target size; the other side is scaled by the same ratio
    // (rounded to nearest, at least one pixel).
    let scaled = |side: u64, by: u64, over: u64| -> u32 {
        let value = (side * by + (over >> 1)).checked_div(over).unwrap_or(1);
        u32::try_from(value.max(1)).unwrap_or(u32::MAX)
    };
    if width * target_height >= height * target_width {
        (
            u32::try_from(target_width).unwrap_or(u32::MAX),
            scaled(height, target_width, width),
        )
    } else {
        (
            scaled(width, target_height, height),
            u32::try_from(target_height).unwrap_or(u32::MAX),
        )
    }
}

/// Decodes the image under the allocation cap and the header pre-screening.
fn decode(bytes: &[u8], config: &ThumbnailConfig) -> Option<DynamicImage> {
    let cap = u64::try_from(config.max_decode_bytes).unwrap_or(u64::MAX);
    let mut limits = Limits::no_limits();
    limits.max_alloc = Some(cap);
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(limits.clone());
    let decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    let pixels = u64::from(width) * u64::from(height);
    if pixels > config.max_pixels || pixels.saturating_mul(DECODED_BYTES_PER_PIXEL) > cap {
        debug!(
            width,
            height, "the image is above the thumbnail pixel limits"
        );
        return None;
    }
    limits.reserve(decoder.total_bytes()).ok()?;
    DynamicImage::from_decoder(decoder).ok()
}

/// Generates the WebP thumbnail of the image in `chunks`; `None` when the image is skipped or
/// cannot be processed.
#[must_use]
pub fn generate(chunks: &[Bytes], size: u64, config: &ThumbnailConfig) -> Option<Thumbnail> {
    if size > u64::try_from(config.max_decode_bytes).unwrap_or(u64::MAX) {
        debug!(size, "the image is above thumbnail.max_decode_bytes");
        return None;
    }
    let bytes: Vec<u8> = chunks
        .iter()
        .flat_map(|chunk| chunk.iter().copied())
        .collect();
    let image = decode(&bytes, config)?;
    let (width, height) = fitted(
        (image.width(), image.height()),
        (config.width, config.height),
    );
    let resized = if (width, height) == (image.width(), image.height()) {
        image
    } else {
        image.resize_exact(width, height, FilterType::Lanczos3)
    };
    // The WebP encoder takes 8-bit gray, gray-alpha, RGB and RGBA only.
    let encodable = if resized.color().has_alpha() {
        DynamicImage::ImageRgba8(resized.to_rgba8())
    } else {
        DynamicImage::ImageRgb8(resized.to_rgb8())
    };
    let mut encoded = Vec::new();
    encodable
        .write_to(&mut Cursor::new(&mut encoded), ImageFormat::WebP)
        .ok()?;
    if encoded.len() > config.max_bytes {
        debug!(
            bytes = encoded.len(),
            "the encoded thumbnail exceeds thumbnail.max_bytes"
        );
        return None;
    }
    Some(Thumbnail {
        width: encodable.width(),
        height: encodable.height(),
        data: encoded,
    })
}
