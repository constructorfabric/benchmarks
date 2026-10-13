//! Image thumbnails: fit inside `width x height`, lossless WebP, size-bounded (DESIGN "Thumbnail generation").

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// A generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    /// WebP bytes.
    pub data: Vec<u8>,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// Generates a thumbnail; `None` when skipped (limits) or on any failure.
#[must_use]
pub fn generate(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if bytes.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let img = if img.width() > cfg.width || img.height() > cfg.height {
        img.resize(cfg.width, cfg.height, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .encode(rgba.as_raw(), rgba.width(), rgba.height(), image::ExtendedColorType::Rgba8)
        .ok()?;
    if out.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail { data: out, width: rgba.width(), height: rgba.height() })
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
