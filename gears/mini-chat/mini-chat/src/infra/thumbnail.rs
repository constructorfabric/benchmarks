//! WebP preview thumbnails of image attachments (DESIGN §3.6 "Thumbnail
//! generation details", spec §13.4).
//!
//! Best effort: any limit hit or decode/encode failure yields `None` and the
//! attachment still becomes `ready` without a thumbnail.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{DynamicImage, ExtendedColorType, ImageReader, Limits};
use tracing::debug;

use crate::config::ThumbnailConfig;

/// An encoded WebP thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub webp: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Thumbnail of `bytes` fitted inside `cfg.width × cfg.height` (aspect ratio kept,
/// never upscaled), lossless WebP.
///
/// Skipped (`None`) when the input is larger than `max_decode_bytes`, when the
/// header dimensions exceed `max_pixels` or `w * h * 4 > max_decode_bytes`, when
/// decoding needs more than `max_decode_bytes`, on any decode/encode failure,
/// and when the WebP is larger than `max_bytes`.
#[must_use]
pub fn make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    match try_thumbnail(bytes, cfg) {
        Ok(t) => Some(t),
        Err(why) => {
            debug!(why, "thumbnail skipped");
            None
        }
    }
}

fn try_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Result<Thumbnail, String> {
    let max_decode = u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX);
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_decode {
        return Err("image larger than max_decode_bytes".to_owned());
    }

    let (w, h) = reader(bytes, max_decode)?
        .into_dimensions()
        .map_err(|e| format!("read header: {e}"))?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels {
        return Err("image exceeds max_pixels".to_owned());
    }
    if pixels.saturating_mul(4) > max_decode {
        return Err("decoded size exceeds max_decode_bytes".to_owned());
    }

    let img = reader(bytes, max_decode)?
        .decode()
        .map_err(|e| format!("decode: {e}"))?;
    let img = fit(img, cfg.width, cfg.height);
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();

    let mut webp = Vec::new();
    WebPEncoder::new_lossless(&mut webp)
        .encode(rgba.as_raw(), width, height, ExtendedColorType::Rgba8)
        .map_err(|e| format!("encode webp: {e}"))?;
    if webp.len() > cfg.max_bytes {
        return Err("thumbnail larger than max_bytes".to_owned());
    }
    Ok(Thumbnail {
        webp,
        width,
        height,
    })
}

/// Reader with the format guessed from the content and the allocation capped.
fn reader(bytes: &[u8], max_alloc: u64) -> Result<ImageReader<Cursor<&[u8]>>, String> {
    let mut r = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("guess format: {e}"))?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(max_alloc);
    r.limits(limits);
    Ok(r)
}

/// `img` scaled down to fit `max_w × max_h` (kept as is when it already fits).
fn fit(img: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    if img.width() <= max_w && img.height() <= max_h {
        img
    } else {
        img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
    }
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
