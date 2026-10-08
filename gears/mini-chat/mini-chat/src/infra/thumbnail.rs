//! Image preview thumbnails (WebP, fit inside WxH, bounded size; DESIGN §3.6
//! "Thumbnail generation details"). Best effort: any failure yields `None`.

use std::io::Cursor;

use image::ImageReader;
use image::codecs::webp::WebPEncoder;

use crate::config::ThumbnailConfig;

/// Encoded thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generates a WebP thumbnail, or `None` when skipped or failed.
#[must_use]
pub fn make_thumbnail(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height).to_rgba8();
    let (tw, th) = thumb.dimensions();
    let mut out = Vec::new();
    thumb
        .write_with_encoder(WebPEncoder::new_lossless(&mut out))
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
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn fits_inside_box_preserving_aspect() {
        let t = make_thumbnail(&png(400, 200), &ThumbnailConfig::default()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.bytes[0..4], b"RIFF");
        assert_eq!(&t.bytes[8..12], b"WEBP");
    }

    #[test]
    fn skips_garbage_and_oversized() {
        assert!(make_thumbnail(b"not an image", &ThumbnailConfig::default()).is_none());
        let cfg = ThumbnailConfig {
            max_pixels: 10,
            ..ThumbnailConfig::default()
        };
        assert!(make_thumbnail(&png(10, 10), &cfg).is_none());
        let cfg = ThumbnailConfig {
            max_bytes: 10,
            ..ThumbnailConfig::default()
        };
        assert!(make_thumbnail(&png(10, 10), &cfg).is_none());
    }
}
