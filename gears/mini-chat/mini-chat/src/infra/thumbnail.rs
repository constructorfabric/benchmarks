//! Image thumbnails (DESIGN section 3.6, "Thumbnail generation details";
//! B.8).
//!
//! Safeguards, in order: the upload itself must not exceed
//! `max_decode_bytes`; the header dimensions must give at most `max_pixels`
//! pixels and at most `max_decode_bytes` decoded RGBA bytes (a pre-screening
//! heuristic); the decoder runs with its allocations capped at
//! `max_decode_bytes`. The thumbnail fits inside `width` x `height` (aspect
//! ratio kept, never upscaled), is encoded as lossless WebP and is dropped
//! when the encoding exceeds `max_bytes`. Every failure is `None`.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// A lossless WebP thumbnail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thumbnail {
    pub webp: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Thumbnail of `bytes` fitted inside `cfg.width` x `cfg.height`, or `None`
/// when it is skipped or fails.
#[must_use]
pub fn make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if bytes.is_empty() || bytes.len() > cfg.max_decode_bytes {
        return None;
    }
    let (w, h) = reader(bytes)?.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    let max_decode = u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > max_decode {
        return None;
    }
    let mut decoder = reader(bytes)?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_decode);
    decoder.limits(limits);
    let img = decoder.decode().ok()?;
    let img = fit(img, cfg.width, cfg.height);
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    let mut webp = Vec::new();
    WebPEncoder::new_lossless(&mut webp)
        .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .ok()?;
    (webp.len() <= cfg.max_bytes).then_some(Thumbnail {
        webp,
        width,
        height,
    })
}

fn reader(bytes: &[u8]) -> Option<ImageReader<Cursor<&[u8]>>> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
}

/// Shrinks `img` to fit inside `max_w` x `max_h`; smaller images are kept.
fn fit(img: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    if img.width() <= max_w && img.height() <= max_h {
        img
    } else {
        img.thumbnail(max_w.max(1), max_h.max(1))
    }
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
