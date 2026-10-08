//! Image thumbnails: fit inside W×H, lossless WebP, bounded decode.

use std::io::Cursor;

use image::{ImageDecoder, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// Generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a thumbnail; `None` on any failure or limit (best effort).
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0 || pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height).to_rgba8();
    let (tw, th) = (thumb.width(), thumb.height());
    let mut out = Vec::new();
    image::codecs::webp::WebPEncoder::new_lossless(&mut out)
        .encode(thumb.as_raw(), tw, th, image::ExtendedColorType::Rgba8)
        .ok()?;
    if out.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail {
        bytes: out,
        width: tw,
        height: th,
    })
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod tests;
