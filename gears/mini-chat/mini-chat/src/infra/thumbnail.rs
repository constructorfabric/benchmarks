//! Image thumbnails (DESIGN "Thumbnail generation details"): WebP, fit inside
//! `width x height` preserving aspect ratio, skipped on size limits, decoder
//! allocation capped at `max_decode_bytes`. Failures yield `None`.

use std::io::Cursor;

use image::{ImageDecoder, ImageReader, Limits};

use crate::config::ThumbnailConfig;

/// Generated thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Content type of every thumbnail.
pub const THUMBNAIL_CONTENT_TYPE: &str = "image/webp";

/// Generates a thumbnail, or `None` when skipped or failed.
#[must_use]
pub fn generate(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if bytes.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0 || pixels > cfg.max_pixels || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64 {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    decoder.set_limits(limits).ok()?;
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
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
    Some(Thumbnail {
        data: out,
        width: tw,
        height: th,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 255) as u8, (y % 255) as u8, 7]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode png");
        out
    }

    #[test]
    fn fits_inside_box_preserving_aspect_ratio() {
        let t = generate(&png(400, 200), &ThumbnailConfig::default()).expect("thumbnail");
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.data[..4], b"RIFF");
    }

    #[test]
    fn skipped_on_limits_and_garbage() {
        let cfg = ThumbnailConfig {
            max_pixels: 100,
            ..ThumbnailConfig::default()
        };
        assert!(generate(&png(20, 20), &cfg).is_none());
        assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
        let tiny = ThumbnailConfig {
            max_bytes: 10,
            ..ThumbnailConfig::default()
        };
        assert!(generate(&png(50, 50), &tiny).is_none());
    }
}
