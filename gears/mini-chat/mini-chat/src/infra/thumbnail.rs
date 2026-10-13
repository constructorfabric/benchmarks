//! Image preview thumbnails (DESIGN section 3.6 "Thumbnail generation
//! details", B.8 `thumbnail.*`).
//!
//! [`WebpThumbnailer`] renders an uploaded image as a WebP thumbnail that
//! fits inside `thumbnail.width` x `thumbnail.height` with the aspect ratio
//! kept (no cropping; an image that already fits keeps its size). The
//! encoder is the lossless WebP encoder of the `image` crate.
//!
//! Generation is skipped ([`ThumbnailSkip`]) when:
//!
//! - the upload is larger than `thumbnail.max_decode_bytes`;
//! - the header dimensions give `width * height` above `thumbnail.max_pixels`
//!   or an estimated decoded size `width * height * 4` above
//!   `thumbnail.max_decode_bytes` (pre-screening only);
//! - decoding fails: unknown or malformed data, an unsupported sub-format, or
//!   a decoder allocation above `thumbnail.max_decode_bytes` (the decoder
//!   runs with that allocation limit, the security boundary against images
//!   that expand on decode);
//! - the encoded thumbnail is larger than `thumbnail.max_bytes` (no retry).
//!
//! A skipped thumbnail never fails the upload: the attachment becomes
//! `ready` without one.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageReader, Limits};
use tracing::debug;

use crate::config::ThumbnailConfig;
use crate::domain::ports::{StoredThumbnail, ThumbnailRenderer};

/// Bytes per decoded pixel assumed by the decode-size estimate (RGBA8).
const ESTIMATED_BYTES_PER_PIXEL: u64 = 4;

/// Why no thumbnail was produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThumbnailSkip {
    #[error("upload of {0} bytes exceeds thumbnail.max_decode_bytes")]
    UploadTooLarge(usize),
    #[error("{0} pixels exceed thumbnail.max_pixels")]
    TooManyPixels(u64),
    #[error("estimated decoded size {0} exceeds thumbnail.max_decode_bytes")]
    DecodeTooLarge(u64),
    #[error("image could not be decoded: {0}")]
    Undecodable(String),
    #[error("thumbnail could not be encoded: {0}")]
    Encode(String),
    #[error("encoded thumbnail of {0} bytes exceeds thumbnail.max_bytes")]
    EncodedTooLarge(usize),
}

/// The thumbnail limits of the gear configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailLimits {
    pub width: u32,
    pub height: u32,
    pub max_bytes: usize,
    pub max_pixels: u64,
    pub max_decode_bytes: usize,
}

impl ThumbnailLimits {
    /// The `thumbnail.*` configuration.
    #[must_use]
    pub const fn from_config(config: &ThumbnailConfig) -> Self {
        Self {
            width: config.width,
            height: config.height,
            max_bytes: config.max_bytes,
            max_pixels: config.max_pixels,
            max_decode_bytes: config.max_decode_bytes,
        }
    }
}

/// Size of a `width` x `height` image fitted inside `box_width` x
/// `box_height` with the aspect ratio kept; an image that already fits keeps
/// its size. Each side is at least 1.
#[must_use]
pub fn fit_within(width: u32, height: u32, box_width: u32, box_height: u32) -> (u32, u32) {
    if width <= box_width && height <= box_height {
        return (width.max(1), height.max(1));
    }
    let scale = f64::min(
        f64::from(box_width) / f64::from(width),
        f64::from(box_height) / f64::from(height),
    );
    let side = |value: u32, bound: u32| -> u32 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the scaled side is rounded and clamped to 1..=bound"
        )]
        let scaled = (f64::from(value) * scale)
            .round()
            .clamp(1.0, f64::from(bound)) as u32;
        scaled
    };
    (side(width, box_width), side(height, box_height))
}

fn decoder_limits(limits: &ThumbnailLimits) -> Limits {
    let mut decoder = Limits::default();
    decoder.max_alloc = Some(u64::try_from(limits.max_decode_bytes).unwrap_or(u64::MAX));
    decoder
}

fn reader<'a>(
    data: &'a [u8],
    limits: &ThumbnailLimits,
) -> Result<ImageReader<Cursor<&'a [u8]>>, ThumbnailSkip> {
    let mut reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|err| ThumbnailSkip::Undecodable(err.to_string()))?;
    reader.limits(decoder_limits(limits));
    Ok(reader)
}

/// Renders the WebP thumbnail of `data` under `limits`.
///
/// # Errors
///
/// [`ThumbnailSkip`] when the thumbnail is skipped (module docs).
pub fn render_webp(
    data: &[u8],
    limits: &ThumbnailLimits,
) -> Result<StoredThumbnail, ThumbnailSkip> {
    if data.len() > limits.max_decode_bytes {
        return Err(ThumbnailSkip::UploadTooLarge(data.len()));
    }
    let (width, height) = reader(data, limits)?
        .into_dimensions()
        .map_err(|err| ThumbnailSkip::Undecodable(err.to_string()))?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > limits.max_pixels {
        return Err(ThumbnailSkip::TooManyPixels(pixels));
    }
    let estimated = pixels.saturating_mul(ESTIMATED_BYTES_PER_PIXEL);
    if estimated > u64::try_from(limits.max_decode_bytes).unwrap_or(u64::MAX) {
        return Err(ThumbnailSkip::DecodeTooLarge(estimated));
    }
    let image = reader(data, limits)?
        .decode()
        .map_err(|err| ThumbnailSkip::Undecodable(err.to_string()))?;
    let (thumb_width, thumb_height) =
        fit_within(image.width(), image.height(), limits.width, limits.height);
    let resized = if (thumb_width, thumb_height) == (image.width(), image.height()) {
        image
    } else {
        image.resize_exact(thumb_width, thumb_height, FilterType::Triangle)
    };
    let encoded = encode_webp(&resized)?;
    if encoded.len() > limits.max_bytes {
        return Err(ThumbnailSkip::EncodedTooLarge(encoded.len()));
    }
    let width = i32::try_from(thumb_width).map_err(|err| ThumbnailSkip::Encode(err.to_string()))?;
    let height =
        i32::try_from(thumb_height).map_err(|err| ThumbnailSkip::Encode(err.to_string()))?;
    Ok(StoredThumbnail {
        data: encoded,
        width,
        height,
    })
}

/// Lossless WebP of `image` (RGBA when it has an alpha channel, else RGB).
fn encode_webp(image: &DynamicImage) -> Result<Vec<u8>, ThumbnailSkip> {
    let mut buffer = Vec::new();
    let webp = image::codecs::webp::WebPEncoder::new_lossless(&mut buffer);
    let result = if image.color().has_alpha() {
        let rgba = image.to_rgba8();
        webp.encode(
            rgba.as_raw(),
            rgba.width(),
            rgba.height(),
            ExtendedColorType::Rgba8,
        )
    } else {
        let rgb = image.to_rgb8();
        webp.encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            ExtendedColorType::Rgb8,
        )
    };
    result.map_err(|err| ThumbnailSkip::Encode(err.to_string()))?;
    Ok(buffer)
}

/// [`ThumbnailRenderer`] producing WebP thumbnails under the configured
/// limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebpThumbnailer {
    limits: ThumbnailLimits,
}

impl WebpThumbnailer {
    /// Renderer with `limits`.
    #[must_use]
    pub const fn new(limits: ThumbnailLimits) -> Self {
        Self { limits }
    }
}

impl ThumbnailRenderer for WebpThumbnailer {
    fn render(&self, data: &[u8]) -> Option<StoredThumbnail> {
        match render_webp(data, &self.limits) {
            Ok(thumbnail) => Some(thumbnail),
            Err(skip) => {
                debug!(target: "mini_chat.thumbnail", reason = %skip, "thumbnail skipped");
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod tests;
