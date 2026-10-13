#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::integer_division, clippy::cast_possible_truncation)]
//! Slice 4: WebP thumbnail generation
//! (`cpt-cf-mini-chat-algo-attachments-generate-thumbnail`).

use std::io::Cursor;
use std::time::Duration;

use image::codecs::gif::{GifEncoder, Repeat};
use image::{Delay, Frame, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use super::model::Thumbnail;
use super::thumbnail::generate_thumbnail;
use crate::config::ThumbnailConfig;

fn encode(img: &image::DynamicImage, format: ImageFormat) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, format).unwrap();
    out.into_inner()
}

/// A flat-colour PNG (small on the wire whatever its dimensions).
fn flat_png(width: u32, height: u32) -> Vec<u8> {
    encode(
        &image::DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([10, 120, 200]))),
        ImageFormat::Png,
    )
}

fn gradient_png(width: u32, height: u32) -> Vec<u8> {
    crate::attachment_test_env::png_bytes(width, height)
}

fn decode(thumbnail: &Thumbnail) -> image::DynamicImage {
    image::load_from_memory_with_format(&thumbnail.bytes, ImageFormat::WebP)
        .expect("the thumbnail is a WebP image")
}

async fn make(bytes: Vec<u8>) -> Option<Thumbnail> {
    generate_thumbnail(bytes, &ThumbnailConfig::default()).await
}

async fn dims(bytes: Vec<u8>) -> (u32, u32) {
    let thumbnail = make(bytes).await.expect("a thumbnail");
    (thumbnail.width, thumbnail.height)
}

// ---- sizing ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_landscape_image_is_fitted_inside_the_box() {
    assert_eq!(dims(gradient_png(300, 150)).await, (128, 64));
}

#[tokio::test]
async fn a_portrait_image_is_fitted_inside_the_box() {
    assert_eq!(dims(flat_png(100, 400)).await, (32, 128));
}

#[tokio::test]
async fn a_square_image_fills_the_box() {
    assert_eq!(dims(flat_png(500, 500)).await, (128, 128));
}

#[tokio::test]
async fn an_image_already_inside_the_box_is_not_enlarged() {
    assert_eq!(dims(flat_png(50, 30)).await, (50, 30));
    assert_eq!(dims(flat_png(128, 128)).await, (128, 128));
    assert_eq!(dims(flat_png(128, 20)).await, (128, 20));
}

#[tokio::test]
async fn each_side_is_rounded_and_at_least_one_pixel() {
    assert_eq!(
        dims(flat_png(200, 101)).await,
        (128, 65),
        "64.64 rounds to 65"
    );
    assert_eq!(dims(flat_png(3000, 10)).await, (128, 1));
    assert_eq!(dims(flat_png(10, 3000)).await, (1, 128));
}

#[tokio::test]
async fn the_box_comes_from_the_configuration() {
    let config = ThumbnailConfig {
        width: 64,
        height: 32,
        ..ThumbnailConfig::default()
    };

    let wide = generate_thumbnail(flat_png(300, 150), &config)
        .await
        .unwrap();
    let tall = generate_thumbnail(flat_png(300, 300), &config)
        .await
        .unwrap();

    assert_eq!((wide.width, wide.height), (64, 32));
    assert_eq!((tall.width, tall.height), (32, 32));
}

// ---- the result -----------------------------------------------------------------------------------

#[tokio::test]
async fn the_result_is_a_webp_image_of_the_reported_size_within_the_byte_limit() {
    let thumbnail = make(gradient_png(300, 150)).await.unwrap();

    assert_eq!(&thumbnail.bytes[..4], b"RIFF");
    assert_eq!(&thumbnail.bytes[8..12], b"WEBP");
    let decoded = decode(&thumbnail);
    assert_eq!((decoded.width(), decoded.height()), (128, 64));
    assert!(thumbnail.bytes.len() <= ThumbnailConfig::default().max_bytes);
}

#[tokio::test]
async fn jpeg_gif_and_webp_sources_are_supported() {
    let rgb = image::DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 150, Rgb([200, 30, 30])));
    let jpeg = encode(&rgb, ImageFormat::Jpeg);
    let gif = encode(&rgb, ImageFormat::Gif);
    let webp = encode(&rgb, ImageFormat::WebP);

    for (name, bytes) in [("jpeg", jpeg), ("gif", gif), ("webp", webp)] {
        assert_eq!(dims(bytes).await, (128, 64), "{name}");
    }
}

#[tokio::test]
async fn an_image_with_transparency_is_supported() {
    let rgba =
        image::DynamicImage::ImageRgba8(RgbaImage::from_pixel(200, 200, Rgba([1, 2, 3, 128])));

    assert_eq!(dims(encode(&rgba, ImageFormat::Png)).await, (128, 128));
}

#[tokio::test]
async fn an_animated_gif_yields_its_first_frame() {
    let mut out = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut out);
        encoder.set_repeat(Repeat::Infinite).unwrap();
        for color in [Rgba([255, 0, 0, 255]), Rgba([0, 0, 255, 255])] {
            let frame = Frame::from_parts(
                RgbaImage::from_pixel(40, 40, color),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            );
            encoder.encode_frame(frame).unwrap();
        }
    }

    let thumbnail = make(out).await.expect("a thumbnail of the first frame");

    let pixel = decode(&thumbnail).to_rgba8().get_pixel(5, 5).0;
    assert!(
        pixel[0] > 200 && pixel[2] < 60,
        "the first (red) frame, got {pixel:?}"
    );
}

// ---- screening and limits ---------------------------------------------------------------------------

#[tokio::test]
async fn bytes_that_are_not_an_image_give_no_thumbnail() {
    assert!(make(flat_png(40, 40)).await.is_some(), "a real image does");
    assert!(make(b"this is not an image".to_vec()).await.is_none());
    assert!(make(Vec::new()).await.is_none());
}

#[tokio::test]
async fn a_truncated_image_gives_no_thumbnail() {
    let png = gradient_png(300, 150);
    assert!(make(png.clone()).await.is_some(), "the whole image does");

    assert!(make(png[..png.len() / 2].to_vec()).await.is_none());
    assert!(make(png[..24].to_vec()).await.is_none());
}

#[tokio::test]
async fn a_source_above_max_decode_bytes_gives_no_thumbnail() {
    // A 1 x 1 PNG followed by ignored trailing bytes: the size screen is the only
    // one that can reject it (the decoded size is 4 bytes).
    let mut png = flat_png(1, 1);
    png.extend(std::iter::repeat_n(0_u8, 5_000));
    let at_limit = ThumbnailConfig {
        max_decode_bytes: png.len(),
        ..ThumbnailConfig::default()
    };
    let below = ThumbnailConfig {
        max_decode_bytes: png.len() - 1,
        ..ThumbnailConfig::default()
    };

    assert!(
        generate_thumbnail(png.clone(), &at_limit).await.is_some(),
        "the source size is compared with `>`"
    );
    assert!(generate_thumbnail(png, &below).await.is_none());
}

#[tokio::test]
async fn an_image_above_max_pixels_gives_no_thumbnail() {
    let png = flat_png(100, 100);
    let at_limit = ThumbnailConfig {
        max_pixels: 10_000,
        ..ThumbnailConfig::default()
    };
    let below = ThumbnailConfig {
        max_pixels: 9_999,
        ..ThumbnailConfig::default()
    };

    assert!(generate_thumbnail(png.clone(), &at_limit).await.is_some());
    assert!(generate_thumbnail(png, &below).await.is_none());
}

#[tokio::test]
async fn an_image_whose_decoded_size_exceeds_max_decode_bytes_gives_no_thumbnail() {
    let png = flat_png(100, 100);
    let at_limit = ThumbnailConfig {
        max_decode_bytes: 40_000,
        ..ThumbnailConfig::default()
    };
    let below = ThumbnailConfig {
        max_decode_bytes: 39_999,
        ..ThumbnailConfig::default()
    };

    assert!(
        generate_thumbnail(png.clone(), &at_limit).await.is_some(),
        "width x height x 4 = 40000 is not above the limit"
    );
    assert!(
        generate_thumbnail(png, &below).await.is_none(),
        "width x height x 4 = 40000 is above 39999 although the source is tiny"
    );
}

#[tokio::test]
async fn the_decoder_allocation_is_capped_at_max_decode_bytes() {
    // 16-bit RGBA decodes to 8 bytes per pixel: the header screen (x 4) passes
    // 40 000 <= 50 000, the decode itself needs 80 000.
    let deep = image::DynamicImage::ImageRgba16(image::ImageBuffer::from_pixel(
        100,
        100,
        Rgba([1000_u16, 2000, 3000, 65535]),
    ));
    let png = encode(&deep, ImageFormat::Png);
    let config = ThumbnailConfig {
        max_decode_bytes: 50_000,
        ..ThumbnailConfig::default()
    };
    assert!(png.len() < 50_000);

    assert!(generate_thumbnail(png.clone(), &config).await.is_none());
    assert!(
        generate_thumbnail(png, &ThumbnailConfig::default())
            .await
            .is_some(),
        "the same image is fine under the default cap"
    );
}

#[tokio::test]
async fn an_encoded_thumbnail_above_max_bytes_gives_no_thumbnail_and_is_not_retried() {
    let png = gradient_png(300, 150);
    let config = ThumbnailConfig {
        max_bytes: 10,
        ..ThumbnailConfig::default()
    };

    assert!(
        generate_thumbnail(png.clone(), &ThumbnailConfig::default())
            .await
            .is_some()
    );
    assert!(generate_thumbnail(png, &config).await.is_none());
}

// ---- threading ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_work_runs_on_a_blocking_thread_not_on_the_request_thread() {
    // A noisy image is slow to decode; the single-threaded runtime of this test
    // must keep polling other tasks while it is decoded.
    let img = RgbImage::from_fn(1500, 1500, |x, y| {
        let v = x
            .wrapping_mul(2_654_435_761)
            .wrapping_add(y.wrapping_mul(40_503));
        Rgb([(v >> 3) as u8, (v >> 11) as u8, (v >> 19) as u8])
    });
    let png = encode(&image::DynamicImage::ImageRgb8(img), ImageFormat::Png);
    let ticks = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(2)).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });

    let thumbnail = make(png).await;
    ticker.abort();

    assert!(thumbnail.is_some());
    assert!(
        ticks.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "the runtime thread stayed free while the image was decoded"
    );
}
