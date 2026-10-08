//! Image thumbnail generation (WebP, fit inside `WxH`, bounded decoding).

use std::io::Cursor;

use image::{ImageDecoder, ImageReader, Limits};

use crate::config::ThumbnailConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Generate a WebP thumbnail; `None` when skipped or on any failure
/// (thumbnail failures never fail the upload).
#[must_use]
pub fn generate(data: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    if data.len() > cfg.max_decode_bytes {
        return None;
    }
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(cfg.max_decode_bytes as u64);
    let mut decoder = reader.into_decoder().ok()?;
    let (w, h) = decoder.dimensions();
    let pixels = u64::from(w) * u64::from(h);
    if pixels == 0
        || pixels > cfg.max_pixels
        || pixels.saturating_mul(4) > cfg.max_decode_bytes as u64
    {
        return None;
    }
    decoder.set_limits(limits).ok()?;
    let img = image::DynamicImage::from_decoder(decoder).ok()?;
    // Fit inside the target box; never scale a smaller image up.
    let thumb = if w <= cfg.width && h <= cfg.height {
        img.to_rgba8()
    } else {
        img.thumbnail(cfg.width, cfg.height).to_rgba8()
    };
    let (tw, th) = thumb.dimensions();
    let mut out = Vec::new();
    let enc = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
    enc.encode(thumb.as_raw(), tw, th, image::ExtendedColorType::Rgba8)
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
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 200, 30, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn fits_inside_box_preserving_aspect() {
        let t = generate(&png(400, 200), &ThumbnailConfig::default()).unwrap();
        assert_eq!((t.width, t.height), (128, 64));
        assert_eq!(&t.bytes[0..4], b"RIFF");
    }

    #[test]
    fn invalid_data_is_skipped() {
        assert!(generate(b"not an image", &ThumbnailConfig::default()).is_none());
        let cfg = ThumbnailConfig {
            max_pixels: 10,
            ..ThumbnailConfig::default()
        };
        assert!(generate(&png(20, 20), &cfg).is_none());
    }
}
