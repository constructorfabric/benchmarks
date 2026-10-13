//! Image preview thumbnails (DESIGN "Thumbnail generation details"): lossless WebP that fits in
//! `thumbnail.width × thumbnail.height`, generated best-effort during the upload.

use std::io::Cursor;

use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{ExtendedColorType, ImageReader, ImageResult, Limits};

use crate::config::ThumbnailConfig;

/// An encoded WebP thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The thumbnail of `bytes`, or `None` when it is skipped or fails (never an error: the image
/// stays usable without a preview).
///
/// Skipped when `bytes` exceed `max_decode_bytes`, when the header dimensions exceed
/// `max_pixels` or their RGBA size exceeds `max_decode_bytes`, and when the encoded WebP exceeds
/// `max_bytes`. The decoder's allocations are capped at `max_decode_bytes` (the header checks are
/// only a pre-screen). CPU-bound: run it on a blocking thread.
#[must_use]
pub fn make_thumbnail(bytes: &[u8], cfg: &ThumbnailConfig) -> Option<Thumbnail> {
    match try_make(bytes, cfg) {
        Ok(thumbnail) => thumbnail,
        Err(err) => {
            tracing::debug!(error = %err, "thumbnail generation failed");
            None
        }
    }
}

fn reader(bytes: &[u8]) -> ImageResult<ImageReader<Cursor<&[u8]>>> {
    Ok(ImageReader::new(Cursor::new(bytes)).with_guessed_format()?)
}

fn try_make(bytes: &[u8], cfg: &ThumbnailConfig) -> ImageResult<Option<Thumbnail>> {
    let max_decode = u64::try_from(cfg.max_decode_bytes).unwrap_or(u64::MAX);
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_decode {
        return Ok(None);
    }
    let (w, h) = reader(bytes)?.into_dimensions()?;
    let pixels = u64::from(w) * u64::from(h);
    if pixels > cfg.max_pixels || pixels.saturating_mul(4) > max_decode {
        return Ok(None);
    }

    let mut limits = Limits::default();
    limits.max_alloc = Some(max_decode);
    let mut decoder = reader(bytes)?;
    decoder.limits(limits);
    let img = decoder.decode()?;

    let (tw, th) = fit(img.width(), img.height(), cfg.width, cfg.height);
    let rgba = if (tw, th) == (img.width(), img.height()) {
        img.into_rgba8()
    } else {
        img.resize_exact(tw, th, FilterType::Triangle).into_rgba8()
    };
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out).encode(rgba.as_raw(), tw, th, ExtendedColorType::Rgba8)?;
    if out.len() > cfg.max_bytes {
        return Ok(None);
    }
    Ok(Some(Thumbnail {
        bytes: out,
        width: tw,
        height: th,
    }))
}

/// `w × h` scaled down to fit inside `max_w × max_h`, aspect preserved, never upscaled, at
/// least 1 × 1.
fn fit(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if w <= max_w && h <= max_h {
        return (w, h);
    }
    let (w64, h64) = (u64::from(w), u64::from(h));
    // Compare max_w / w with max_h / h without floating point.
    let (num, den) = if u64::from(max_w) * h64 <= u64::from(max_h) * w64 {
        (u64::from(max_w), w64)
    } else {
        (u64::from(max_h), h64)
    };
    #[allow(clippy::integer_division)] // rounded to the nearest pixel explicitly
    let scale = |v: u64| -> u32 {
        let scaled = (v * num + den / 2) / den;
        u32::try_from(scaled.max(1)).unwrap_or(u32::MAX)
    };
    (scale(w64), scale(h64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::attachments::png;

    fn cfg() -> ThumbnailConfig {
        ThumbnailConfig::default()
    }

    fn decoded_dimensions(t: &Thumbnail) -> (u32, u32) {
        let img = image::load_from_memory_with_format(&t.bytes, image::ImageFormat::WebP)
            .expect("valid webp");
        (img.width(), img.height())
    }

    #[test]
    fn fits_inside_the_box_preserving_aspect() {
        for ((w, h), want) in [
            ((300, 150), (128, 64)),
            ((100, 400), (32, 128)),
            ((256, 256), (128, 128)),
            ((1000, 1), (128, 1)),
        ] {
            let t = make_thumbnail(&png(w, h), &cfg()).unwrap_or_else(|| panic!("{w}x{h}"));
            assert_eq!((t.width, t.height), want, "{w}x{h}");
            assert_eq!(decoded_dimensions(&t), want, "{w}x{h}");
            assert_eq!(&t.bytes[..4], b"RIFF");
            assert_eq!(&t.bytes[8..12], b"WEBP");
        }
    }

    #[test]
    fn small_images_are_not_upscaled() {
        let t = make_thumbnail(&png(50, 20), &cfg()).expect("thumbnail");
        assert_eq!((t.width, t.height), (50, 20));
    }

    #[test]
    fn other_box_sizes_are_honoured() {
        let cfg = ThumbnailConfig {
            width: 64,
            height: 16,
            ..cfg()
        };
        let t = make_thumbnail(&png(300, 150), &cfg).expect("thumbnail");
        assert_eq!((t.width, t.height), (32, 16));
    }

    #[test]
    fn skipped_or_failed_thumbnails_are_none() {
        let img = png(20, 20);
        assert!(
            make_thumbnail(b"not an image", &cfg()).is_none(),
            "decode error"
        );
        let too_many_bytes = ThumbnailConfig {
            max_decode_bytes: img.len() - 1,
            ..cfg()
        };
        assert!(
            make_thumbnail(&img, &too_many_bytes).is_none(),
            "input size"
        );
        let too_many_pixels = ThumbnailConfig {
            max_pixels: 399,
            ..cfg()
        };
        assert!(make_thumbnail(&img, &too_many_pixels).is_none(), "pixels");
        let decoded_too_big = ThumbnailConfig {
            max_decode_bytes: 20 * 20 * 4 - 1,
            ..cfg()
        };
        assert!(img.len() < 20 * 20 * 4 - 1);
        assert!(
            make_thumbnail(&img, &decoded_too_big).is_none(),
            "decoded size"
        );
        let tiny_output = ThumbnailConfig {
            max_bytes: 8,
            ..cfg()
        };
        assert!(make_thumbnail(&img, &tiny_output).is_none(), "encoded size");
        assert!(make_thumbnail(&img, &cfg()).is_some(), "control");
    }
}
