//! WebP thumbnail generation (`cpt-cf-mini-chat-algo-attachments-generate-thumbnail`).

use std::io::Cursor;

use bytes::Bytes;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};

use super::model::Thumbnail;
use crate::config::ThumbnailConfig;

// @cpt-dod:cpt-cf-mini-chat-dod-attachments-images:p1
/// Generates the thumbnail of an image, on a blocking worker thread.
pub async fn generate_thumbnail(
    bytes: impl Into<Bytes>,
    config: &ThumbnailConfig,
) -> Option<Thumbnail> {
    let bytes: Bytes = bytes.into();
    let config = config.clone();
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-blocking-thread
    tokio::task::spawn_blocking(move || build(&bytes, &config))
        .await
        .ok()
        .flatten()
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-blocking-thread
}

fn reader(bytes: &[u8]) -> Option<ImageReader<Cursor<&[u8]>>> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()
}

/// One side after scaling by `scale` in `(0, 1]`: never 0, never above `side`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn scaled_side(side: u32, scale: f64) -> u32 {
    ((f64::from(side) * scale).round() as u32).clamp(1, side.max(1))
}

fn build(bytes: &[u8], config: &ThumbnailConfig) -> Option<Thumbnail> {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-skip-large-source
    if bytes.len() > config.max_decode_bytes {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-skip-large-source
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-header-screening
    let (width, height) = reader(bytes)?.into_dimensions().ok()?;
    let pixels = u64::from(width) * u64::from(height);
    if pixels > config.max_pixels || u128::from(pixels) * 4 > config.max_decode_bytes as u128 {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-header-screening
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-decode
    let mut limits = Limits::default();
    limits.max_alloc = Some(config.max_decode_bytes as u64);
    let mut decoder = reader(bytes)?;
    decoder.limits(limits);
    let image = decoder.decode().ok()?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-decode

    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-resize
    let scale = (f64::from(config.width) / f64::from(width))
        .min(f64::from(config.height) / f64::from(height))
        .min(1.0);
    let (new_width, new_height) = (scaled_side(width, scale), scaled_side(height, scale));
    let image = if (new_width, new_height) == (width, height) {
        image
    } else {
        image.resize_exact(new_width, new_height, image::imageops::FilterType::Triangle)
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-resize
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-encode-webp
    let mut out = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image.to_rgba8())
        .write_to(&mut out, ImageFormat::WebP)
        .ok()?;
    let encoded = out.into_inner();
    if encoded.len() > config.max_bytes {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-encode-webp
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-return-thumbnail
    Some(Thumbnail {
        bytes: encoded,
        width: new_width,
        height: new_height,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-generate-thumbnail:p1:inst-return-thumbnail
}
