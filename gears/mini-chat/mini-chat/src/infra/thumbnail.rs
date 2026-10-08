//! Image thumbnails (WebP, fit inside the configured box, best effort).

use std::io::Cursor;

use image::ImageReader;
use image::codecs::webp::WebPEncoder;

use crate::config::ThumbnailConfig;

/// A generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a thumbnail; `None` when skipped or failed.
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = if img.width() > cfg.width || img.height() > cfg.height {
        img.thumbnail(cfg.width, cfg.height)
    } else {
        img
    };
    let rgba = thumb.to_rgba8();
    let (tw, th) = rgba.dimensions();
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .encode(rgba.as_raw(), tw, th, image::ExtendedColorType::Rgba8)
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
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 128])
        });
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn fits_inside_box_preserving_aspect() {
        let t = generate(&png(400, 200), &ThumbnailConfig::default()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.bytes[..4], b"RIFF");
        assert_eq!(&t.bytes[8..12], b"WEBP");
    }

    #[test]
    fn skips_garbage_and_oversized() {
        assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
        let cfg = ThumbnailConfig {
            max_pixels: 10,
            ..ThumbnailConfig::default()
        };
        assert!(generate(&png(10, 10), &cfg).is_none());
    }
}
