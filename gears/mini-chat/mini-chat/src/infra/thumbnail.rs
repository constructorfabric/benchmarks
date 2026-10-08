//! Image thumbnails: fit inside WxH (aspect preserved), WebP output,
//! bounded decode (DESIGN §3.6 "Thumbnail generation details").

use std::io::Cursor;

use image::{ImageFormat, ImageReader, Limits};

use crate::config::ThumbnailConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a thumbnail; `None` when skipped or failed (never an error).
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
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
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let thumb = img.thumbnail(cfg.width, cfg.height);
    let rgba = image::DynamicImage::ImageRgba8(thumb.to_rgba8());
    let mut out = Cursor::new(Vec::new());
    rgba.write_to(&mut out, ImageFormat::WebP).ok()?;
    let bytes = out.into_inner();
    if bytes.len() > cfg.max_bytes {
        return None;
    }
    Some(Thumbnail {
        bytes,
        width: rgba.width(),
        height: rgba.height(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn fits_and_keeps_aspect() {
        let t = generate(&png(400, 200), &ThumbnailConfig::default()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert!(t.bytes.starts_with(b"RIFF"));
    }

    #[test]
    fn invalid_data_gives_none() {
        assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
    }

    #[test]
    fn decode_cap_respected() {
        let cfg = ThumbnailConfig { max_decode_bytes: 100, ..ThumbnailConfig::default() };
        assert!(generate(&png(50, 50), &cfg).is_none());
    }
}
