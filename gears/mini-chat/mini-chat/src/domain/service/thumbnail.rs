//! Image thumbnail generation (OWNER: attachments).
//!
//! Best-effort: every failure (oversized input, header pre-check, decode error, allocation
//! limit, encoded size above `thumbnail.max_bytes`) yields `None` and the attachment still
//! becomes `ready` without a thumbnail.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// Content type of every generated thumbnail.
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// Encoded WebP thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

fn reader(data: &[u8]) -> Option<ImageReader<Cursor<&[u8]>>> {
    ImageReader::new(Cursor::new(data)).with_guessed_format().ok()
}

/// Generates a WebP thumbnail fitting inside `cfg.width` x `cfg.height` (aspect ratio kept,
/// never upscaled). CPU-bound: call from a blocking task.
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.is_empty() || data.len() > cfg.max_decode_bytes {
        return None;
    }
    // Header pre-screening (heuristic only; the decoder allocation cap is the real boundary).
    let (w, h) = reader(data)?.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0 || pixels > cfg.max_pixels {
        return None;
    }
    let max_decode = u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX);
    if pixels.saturating_mul(4) > max_decode {
        return None;
    }

    let mut r = reader(data)?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_decode);
    r.limits(limits);
    let img = r.decode().ok()?;

    let (tw, th) = (cfg.width.max(1), cfg.height.max(1));
    let resized = if img.width() <= tw && img.height() <= th {
        img
    } else {
        img.thumbnail(tw, th)
    };
    let rgba = DynamicImage::ImageRgba8(resized.to_rgba8());
    let mut out = Vec::new();
    rgba.write_with_encoder(WebPEncoder::new_lossless(&mut out)).ok()?;
    if out.is_empty() || out.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail {
        data: out,
        width: rgba.width(),
        height: rgba.height(),
    })
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
