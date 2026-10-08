//! Image thumbnails (WebP, fit inside the configured box, best effort).

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// A generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a thumbnail; `None` when skipped or failed (the attachment still
/// becomes ready).
#[must_use]
pub fn make_thumbnail(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
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
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let img = if img.width() > cfg.width || img.height() > cfg.height {
        img.thumbnail(cfg.width.max(1), cfg.height.max(1))
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (tw, th) = (rgba.width(), rgba.height());
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut out, ImageFormat::WebP)
        .ok()?;
    let bytes = out.into_inner();
    if bytes.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail {
        bytes,
        width: tw,
        height: th,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ThumbnailConfig {
        ThumbnailConfig {
            width: 128,
            height: 128,
            max_bytes: 131_072,
            max_pixels: 100_000_000,
            max_decode_bytes: 33_554_432,
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap_or_default();
        out.into_inner()
    }

    #[test]
    fn fits_inside_box_and_keeps_aspect() {
        let t = make_thumbnail(&png(400, 200), &cfg()).expect("thumbnail");
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.bytes[0..4], b"RIFF");
        assert_eq!(&t.bytes[8..12], b"WEBP");
    }

    #[test]
    fn garbage_and_limits_skip() {
        assert!(make_thumbnail(b"not an image", &cfg()).is_none());
        let mut c = cfg();
        c.max_pixels = 10;
        assert!(make_thumbnail(&png(10, 10), &c).is_none());
        let mut c = cfg();
        c.max_bytes = 10;
        assert!(make_thumbnail(&png(10, 10), &c).is_none());
    }
}
