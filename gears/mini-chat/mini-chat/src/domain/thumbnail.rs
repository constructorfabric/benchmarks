//! Image thumbnail generation (DESIGN §3.6 "File Upload", thumbnail details).

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// An encoded WebP thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Target size that fits inside `max_w` x `max_h`, keeping the aspect ratio (never upscales).
#[must_use]
pub fn fit_size(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w == 0 || h == 0 || (w <= max_w && h <= max_h) {
        return (w.max(1), h.max(1));
    }
    let scale = f64::min(
        f64::from(max_w) / f64::from(w),
        f64::from(max_h) / f64::from(h),
    );
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let tw = ((f64::from(w) * scale).round() as u32).clamp(1, max_w);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let th = ((f64::from(h) * scale).round() as u32).clamp(1, max_h);
    (tw, th)
}

/// Generates a WebP thumbnail; `None` when skipped or failed (best effort).
#[must_use]
pub fn generate(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if bytes.len() > cfg.max_decode_bytes {
        return None;
    }
    let (w, h) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let (tw, th) = fit_size(w, h, cfg.width.max(1), cfg.height.max(1));
    let small = if (tw, th) == (w, h) {
        img
    } else {
        img.resize_exact(tw, th, image::imageops::FilterType::Triangle)
    };
    let rgba = small.to_rgba8();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .encode(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
        .ok()?;
    if out.len() > cfg.max_bytes {
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
