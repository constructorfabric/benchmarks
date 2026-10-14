//! The image thumbnail
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail`): header pre-screening, capped
//! decode, resize to fit, WebP encode with the size bound. A skip never fails the upload.

use crate::config::ThumbnailConfig;
use crate::domain::attachments::model::Thumbnail;

/// Generate the WebP thumbnail of `kept` (the image bytes kept in memory during the upload,
/// `None` when they exceeded `thumbnail.max_decode_bytes`); `None` means skipped. A skip never
/// fails the upload and never sets an `error_code`.
#[must_use]
pub fn generate_thumbnail(
    kept: Option<&[u8]>,
    _mime: &str,
    config: &ThumbnailConfig,
) -> Option<Thumbnail> {
    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat, ImageReader, Limits};

    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-not-kept
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-return-not-kept
    let bytes = kept?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-return-not-kept
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-not-kept
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-prescreen
    // the header dimensions are screened before any pixel is decoded
    let decode_cap = u64::try_from(config.max_decode_bytes).unwrap_or(u64::MAX);
    let (width, height) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > config.max_pixels || pixels.saturating_mul(4) > decode_cap {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-prescreen
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-decode
    // any decode failure (malformed data, unsupported sub-format, the allocation cap) is a skip
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(decode_cap);
    reader.limits(limits);
    let decoded = reader.decode().ok()?;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-decode
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-resize
    // fit inside the box keeping the aspect ratio, without cropping
    let small = DynamicImage::ImageRgba8(decoded.thumbnail(config.width, config.height).to_rgba8());
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-resize
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-encode
    let mut out = Cursor::new(Vec::new());
    small.write_to(&mut out, ImageFormat::WebP).ok()?;
    let encoded = out.into_inner();
    // above the size bound: skipped, with no retry at a lower quality
    if encoded.len() > config.max_bytes {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-encode
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-return
    Some(Thumbnail {
        bytes: encoded,
        width: small.width(),
        height: small.height(),
    })
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-thumbnail:p1:inst-th-return
}

#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
