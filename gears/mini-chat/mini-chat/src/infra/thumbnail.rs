//! Image preview thumbnails (D "Thumbnail generation details"): fit inside
//! `thumbnail.width` x `thumbnail.height`, WebP, best effort.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageReader, Limits};
use tracing::debug;

use crate::config::ThumbnailConfig;

/// Encoded WebP thumbnail and its dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub webp: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Thumbnail of an uploaded image; `None` when generation is skipped or
/// fails (the attachment still becomes ready).
///
/// Skipped when the input is larger than `max_decode_bytes`, when the header
/// dimensions exceed `max_pixels` or give an estimated RGBA size above
/// `max_decode_bytes`, or when the encoded WebP exceeds `max_bytes`. The
/// decoder's allocations are capped at `max_decode_bytes`. The image is
/// scaled down to fit inside `width` x `height` (aspect ratio kept, never
/// upscaled) and encoded as lossless WebP (the only WebP encoder of the
/// `image` crate).
#[must_use]
pub fn make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    let img = decode(bytes, cfg)?;
    let img = if img.width() > cfg.width || img.height() > cfg.height {
        img.thumbnail(cfg.width, cfg.height)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let mut webp = Vec::new();
    WebPEncoder::new_lossless(&mut webp)
        .encode(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| skipped::<()>("encode failed", &e))
        .ok()?;
    if webp.len() > cfg.max_bytes {
        return skipped("encoded size over max_bytes", &webp.len());
    }
    Some(Thumbnail {
        webp,
        width: rgba.width(),
        height: rgba.height(),
    })
}

/// Decode `bytes` within the size / pixel limits of `cfg`.
fn decode(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<DynamicImage> {
    if bytes.len() > cfg.max_decode_bytes {
        return skipped("input over max_decode_bytes", &bytes.len());
    }
    let reader = || {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .ok()
    };
    let (w, h) = reader()?.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    let decode_cap = u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > decode_cap {
        return skipped("dimensions over limits", &pixels);
    }
    let mut decoder = reader()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(decode_cap);
    decoder.limits(limits);
    decoder
        .decode()
        .map_err(|e| skipped::<()>("decode failed", &e))
        .ok()
}

fn skipped<T>(reason: &str, detail: &dyn std::fmt::Display) -> Option<T> {
    debug!(reason, detail = %detail, "thumbnail skipped");
    None
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod tests;
