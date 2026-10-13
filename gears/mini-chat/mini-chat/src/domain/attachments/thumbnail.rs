//! Best-effort WebP preview thumbnails of image attachments (DESIGN §3.6 "Thumbnail generation
//! details"). Every failure yields `None`; it never fails the upload.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// Content type of every stored thumbnail.
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// An encoded thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generates a WebP thumbnail fitting `cfg.width` x `cfg.height` (aspect preserved, no
/// cropping, no upscaling). Skips oversized sources (bytes, header pixels, estimated decoded
/// size), caps decoder allocations at `cfg.max_decode_bytes` and drops results above
/// `cfg.max_bytes`.
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.is_empty() || data.len() > cfg.max_decode_bytes {
        return None;
    }
    let (w, h) = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0 || pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let img = if img.width() > cfg.width || img.height() > cfg.height { img.thumbnail(cfg.width, cfg.height) } else { img };
    let rgba = img.to_rgba8();
    let (tw, th) = rgba.dimensions();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out).encode(rgba.as_raw(), tw, th, ExtendedColorType::Rgba8).ok()?;
    if out.is_empty() || out.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail { bytes: out, width: tw, height: th })
}
