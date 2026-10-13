#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: the bounded file content read
//! (`cpt-cf-mini-chat-algo-attachments-read-file-content`).

use bytes::Bytes;

use super::source::read_file_content;
use super::test_support::VecSource;
use crate::domain::error::{DomainError, MultipartIssue};

fn chunks(parts: &[&[u8]]) -> Vec<Result<Bytes, MultipartIssue>> {
    parts
        .iter()
        .map(|p| Ok(Bytes::copy_from_slice(p)))
        .collect()
}

fn source(parts: &[&[u8]]) -> (VecSource, super::test_support::SourceProbe) {
    VecSource::new(
        Ok(super::source::FilePartHeader {
            content_type: "application/pdf".to_owned(),
            filename: None,
        }),
        chunks(parts),
    )
}

#[tokio::test]
async fn the_chunks_are_concatenated_in_order() {
    let (mut source, _) = source(&[b"abc", b"def", b"g"]);

    let bytes = read_file_content(&mut source, 100).await.unwrap();

    assert_eq!(&bytes[..], b"abcdefg");
}

#[tokio::test]
async fn a_file_of_exactly_the_limit_is_accepted() {
    let (mut source, _) = source(&[b"01234", b"56789"]);

    let bytes = read_file_content(&mut source, 10).await.unwrap();

    assert_eq!(bytes.len(), 10);
}

#[tokio::test]
async fn a_file_one_byte_over_the_limit_is_too_large() {
    let (mut source, _) = source(&[b"01234", b"567890"]);

    let result = read_file_content(&mut source, 10).await;

    assert!(
        matches!(result, Err(DomainError::FileTooLarge)),
        "{result:?}"
    );
}

#[tokio::test]
async fn the_read_stops_at_the_chunk_that_crosses_the_limit() {
    let parts: Vec<&[u8]> = vec![b"1234"; 50];
    let (mut source, probe) = source(&parts);

    let result = read_file_content(&mut source, 10).await;

    assert!(matches!(result, Err(DomainError::FileTooLarge)));
    assert_eq!(
        probe.pulled(),
        3,
        "4 + 4 + 4 crosses 10 at the third chunk; the rest of the body is never read"
    );
}

#[tokio::test]
async fn a_single_chunk_above_the_limit_is_too_large() {
    let big = vec![0_u8; 1 << 20];
    let (mut source, probe) = source(&[&big, b"x"]);

    let result = read_file_content(&mut source, 1024).await;

    assert!(matches!(result, Err(DomainError::FileTooLarge)));
    assert_eq!(probe.pulled(), 1);
}

#[tokio::test]
async fn an_empty_file_is_accepted() {
    let (mut source, _) = source(&[]);

    let bytes = read_file_content(&mut source, 10).await.unwrap();

    assert!(bytes.is_empty());
}

#[tokio::test]
async fn an_empty_chunk_does_not_count_and_does_not_end_the_file() {
    let (mut source, _) = source(&[b"", b"ab", b"", b"cd"]);

    let bytes = read_file_content(&mut source, 4).await.unwrap();

    assert_eq!(&bytes[..], b"abcd");
}

#[tokio::test]
async fn a_limit_of_zero_accepts_only_an_empty_file() {
    let (mut empty, _) = source(&[]);
    let (mut one, _) = source(&[b"x"]);

    assert!(read_file_content(&mut empty, 0).await.unwrap().is_empty());
    assert!(matches!(
        read_file_content(&mut one, 0).await,
        Err(DomainError::FileTooLarge)
    ));
}

#[tokio::test]
async fn a_failed_read_is_a_multipart_error() {
    let (mut healthy, _) = source(&[b"abc", b"def"]);
    assert_eq!(
        &read_file_content(&mut healthy, 100).await.unwrap()[..],
        b"abcdef"
    );
    let (mut source, _) = VecSource::new(
        Ok(super::source::FilePartHeader {
            content_type: "application/pdf".to_owned(),
            filename: None,
        }),
        vec![
            Ok(Bytes::from_static(b"abc")),
            Err(MultipartIssue::MultipartError),
        ],
    );

    let result = read_file_content(&mut source, 100).await;

    assert!(
        matches!(
            result,
            Err(DomainError::InvalidMultipart(
                MultipartIssue::MultipartError
            ))
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn reading_the_content_does_not_reopen_the_part() {
    let (mut source, probe) = source(&[b"abc"]);

    read_file_content(&mut source, 100).await.unwrap();

    assert!(!probe.opened(), "only the headers call open_file");
}

#[tokio::test]
async fn a_single_chunk_is_returned_without_a_copy() {
    let chunk = Bytes::from_static(b"abcdef");
    let pointer = chunk.as_ptr();
    let (mut source, _) = VecSource::new(
        Ok(super::source::FilePartHeader {
            content_type: "application/pdf".to_owned(),
            filename: None,
        }),
        vec![Ok(chunk)],
    );

    let bytes = read_file_content(&mut source, 100).await.unwrap();

    assert_eq!(&bytes[..], b"abcdef");
    assert_eq!(bytes.as_ptr(), pointer, "the same allocation");
}
