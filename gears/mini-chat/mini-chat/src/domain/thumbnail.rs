//! Image thumbnails: WebP, fit inside WxH, bounded size and decode limits.

use std::io::Cursor;

use crate::config::ThumbnailConfig;

/// A generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedThumbnail {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Generates a WebP thumbnail; returns `None` when generation is skipped or
/// fails (the attachment still becomes ready).
#[must_use]
pub fn generate(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<GeneratedThumbnail> {
    if bytes.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX) {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX));
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height).to_rgba8();
    let (tw, th) = thumb.dimensions();
    let mut out = Vec::new();
    let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
    encoder
        .encode(thumb.as_raw(), tw, th, image::ExtendedColorType::Rgba8)
        .ok()?;
    if out.len() > cfg.max_bytes {
        return None;
    }
    Some(GeneratedThumbnail { width: tw, height: th, data: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn fits_inside_and_preserves_aspect() {
        let t = generate(&png(512, 256), &ThumbnailConfig::default()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.data[0..4], b"RIFF");
        assert_eq!(&t.data[8..12], b"WEBP");
    }

    #[test]
    fn skips_garbage_and_oversized() {
        assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
        let cfg = ThumbnailConfig { max_pixels: 10, ..ThumbnailConfig::default() };
        assert!(generate(&png(64, 64), &cfg).is_none());
        let cfg = ThumbnailConfig { max_bytes: 10, ..ThumbnailConfig::default() };
        assert!(generate(&png(64, 64), &cfg).is_none());
    }
}
