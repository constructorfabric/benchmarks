//! Image thumbnail generation (WebP, fit inside the configured box).

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// An encoded thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a WebP thumbnail; `None` when skipped or failed (best effort).
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let (w, h) = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height).to_rgba8();
    let (tw, th) = thumb.dimensions();
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(thumb).write_to(&mut out, ImageFormat::WebP).ok()?;
    let bytes = out.into_inner();
    if bytes.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail { bytes, width: tw, height: th })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ThumbnailConfig {
        ThumbnailConfig { width: 128, height: 128, max_bytes: 131_072, max_pixels: 100_000_000, max_decode_bytes: 33_554_432 }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([u8::try_from(x % 256).unwrap(), u8::try_from(y % 256).unwrap(), 128]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn fits_box_preserving_aspect() {
        let t = generate(&png(400, 200), &cfg()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.bytes[0..4], b"RIFF");
        assert_eq!(&t.bytes[8..12], b"WEBP");
    }

    #[test]
    fn garbage_is_skipped() {
        assert!(generate(b"not an image", &cfg()).is_none());
    }

    #[test]
    fn pixel_limit_skips() {
        let mut c = cfg();
        c.max_pixels = 100;
        assert!(generate(&png(20, 20), &c).is_none());
    }

    #[test]
    fn oversize_thumbnail_skipped() {
        let mut c = cfg();
        c.max_bytes = 10;
        assert!(generate(&png(50, 50), &c).is_none());
    }
}
