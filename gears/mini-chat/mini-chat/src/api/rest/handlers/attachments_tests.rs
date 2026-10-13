#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;

use axum::body::Body;
use bytes::Bytes;
use futures::{StreamExt, stream};

use super::read_file_part;
use crate::domain::error::DomainError;

const BOUNDARY: &str = "XyZ123";
const CT: &str = "multipart/form-data; boundary=XyZ123";

fn part(name: &str, filename: Option<&str>, content_type: Option<&str>, data: &[u8]) -> Vec<u8> {
    let mut out = format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{name}\"");
    if let Some(f) = filename {
        write!(out, "; filename=\"{f}\"").unwrap();
    }
    out.push_str("\r\n");
    if let Some(ct) = content_type {
        write!(out, "Content-Type: {ct}\r\n").unwrap();
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(data);
    out.extend_from_slice(b"\r\n");
    out
}

fn body(parts: &[Vec<u8>]) -> Body {
    let mut all: Vec<u8> = parts.concat();
    all.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    Body::from(all)
}

fn limit(n: u64) -> impl Fn(Option<&str>, &str) -> u64 + Send {
    move |_, _| n
}

fn multipart(reason: &'static str, field: &'static str) -> DomainError {
    DomainError::Multipart { reason, field }
}

#[tokio::test]
async fn reads_the_file_part_after_other_fields() {
    let b = body(&[
        part("note", None, None, b"ignored"),
        part("file", Some("a.pdf"), Some("application/pdf"), b"%PDF-1.4"),
    ]);
    let f = read_file_part(Some(CT), b, limit(100)).await.unwrap();
    assert_eq!(f.filename.as_deref(), Some("a.pdf"));
    assert_eq!(f.content_type, "application/pdf");
    assert_eq!(&f.bytes[..], b"%PDF-1.4");
}

#[tokio::test]
async fn file_part_without_filename_has_none() {
    let b = body(&[part("file", None, Some("text/plain; charset=utf-8"), b"hi")]);
    let f = read_file_part(Some(CT), b, limit(100)).await.unwrap();
    assert_eq!(f.filename, None);
    assert_eq!(f.content_type, "text/plain; charset=utf-8");
}

#[tokio::test]
async fn limit_is_chosen_from_the_part_headers_and_enforced_while_reading() {
    let data = vec![b'x'; 64];
    let b = body(&[part("file", Some("a.png"), Some("image/png"), &data)]);
    let seen = std::sync::Mutex::new(None);
    let err = read_file_part(Some(CT), b, |name: Option<&str>, ct: &str| {
        *seen.lock().unwrap() = Some((name.map(str::to_owned), ct.to_owned()));
        63
    })
    .await
    .unwrap_err();
    assert_eq!(err, DomainError::FileTooLarge);
    assert_eq!(
        seen.into_inner().unwrap(),
        Some((Some("a.png".to_owned()), "image/png".to_owned()))
    );

    let b = body(&[part("file", Some("a.png"), Some("image/png"), &data)]);
    assert_eq!(
        read_file_part(Some(CT), b, limit(64))
            .await
            .unwrap()
            .bytes
            .len(),
        64
    );
}

#[tokio::test]
async fn oversize_stream_is_aborted_without_reading_the_rest() {
    // A body that never ends after the first chunk: the reader must stop at
    // the limit instead of waiting for more data.
    let head = part("file", Some("a.bin"), Some("application/pdf"), &[b'x'; 32]);
    let head = head[..head.len() - 2].to_vec(); // no part terminator
    let chunks =
        stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(head))]).chain(stream::pending());
    let err = read_file_part(Some(CT), Body::from_stream(chunks), limit(16))
        .await
        .unwrap_err();
    assert_eq!(err, DomainError::FileTooLarge);
}

#[tokio::test]
async fn multipart_errors() {
    let ok = || body(&[part("file", Some("a.pdf"), Some("application/pdf"), b"x")]);
    assert_eq!(
        read_file_part(None, ok(), limit(10)).await.unwrap_err(),
        multipart("BOUNDARY_REQUIRED", "content_type")
    );
    assert_eq!(
        read_file_part(Some("multipart/form-data"), ok(), limit(10))
            .await
            .unwrap_err(),
        multipart("BOUNDARY_REQUIRED", "content_type")
    );
    assert_eq!(
        read_file_part(
            Some(CT),
            body(&[part("other", None, None, b"x")]),
            limit(10)
        )
        .await
        .unwrap_err(),
        multipart("MISSING_FILE", "file")
    );
    assert_eq!(
        read_file_part(
            Some(CT),
            body(&[part("file", Some("a"), None, b"x")]),
            limit(10)
        )
        .await
        .unwrap_err(),
        multipart("MISSING_CONTENT_TYPE", "content_type")
    );
    assert_eq!(
        read_file_part(Some(CT), Body::from("garbage without boundary"), limit(10))
            .await
            .unwrap_err(),
        multipart("MULTIPART_ERROR", "multipart")
    );
}
