#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: reading the multipart request
//! (`cpt-cf-mini-chat-algo-attachments-read-multipart`): the reasons, the
//! discarded and unread parts, the headers of the `file` part and the streamed
//! content.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use bytes::Bytes;
use futures::stream;

use super::RawMultipart;
use crate::attachment_test_env::{
    PartSpec, file_part, multipart_body, multipart_content_type, text_part,
};
use crate::domain::error::MultipartIssue;
use crate::domain::service::attachments::classify::{classify_file, normalize_filename};
use crate::domain::service::attachments::model::ClassifyRules;
use crate::domain::service::attachments::source::{FilePartHeader, MultipartSource};

const B: &str = "TESTBOUNDARY";

fn rules() -> ClassifyRules {
    ClassifyRules {
        allow_csv_upload: true,
        disable_images: false,
        code_interpreter_available: true,
    }
}

fn source(parts: &[PartSpec]) -> RawMultipart {
    RawMultipart::new(
        Some(&multipart_content_type(B)),
        Body::from(multipart_body(B, parts)),
    )
}

fn raw(body: &[u8]) -> RawMultipart {
    RawMultipart::new(Some(&multipart_content_type(B)), Body::from(body.to_vec()))
}

async fn read_all(source: &mut RawMultipart) -> Result<(FilePartHeader, Vec<u8>), MultipartIssue> {
    let header = source.open_file().await?;
    let mut content = Vec::new();
    while let Some(chunk) = source.next_chunk().await? {
        content.extend_from_slice(&chunk);
    }
    Ok((header, content))
}

async fn open_err(mut source: RawMultipart) -> MultipartIssue {
    source.open_file().await.expect_err("an issue")
}

// ---- boundary ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_request_without_a_content_type_has_no_boundary() {
    let source = RawMultipart::new(
        None,
        Body::from(multipart_body(
            B,
            &[file_part("a.pdf", "application/pdf", b"x")],
        )),
    );

    assert_eq!(open_err(source).await, MultipartIssue::BoundaryRequired);
}

#[tokio::test]
async fn a_content_type_that_is_not_multipart_form_data_with_a_boundary_has_none() {
    for content_type in [
        "multipart/form-data",
        "multipart/form-data; charset=utf-8",
        "multipart/form-data; boundary=",
        "multipart/form-data; boundary=\"\"",
        "application/json",
        "application/json; boundary=TESTBOUNDARY",
        "multipart/mixed; boundary=TESTBOUNDARY",
        "text/plain",
        "",
    ] {
        let source = RawMultipart::new(
            Some(content_type),
            Body::from(multipart_body(
                B,
                &[file_part("a.pdf", "application/pdf", b"x")],
            )),
        );

        assert_eq!(
            open_err(source).await,
            MultipartIssue::BoundaryRequired,
            "`{content_type}`"
        );
    }
}

#[tokio::test]
async fn the_media_type_and_the_parameter_name_are_case_insensitive_and_the_boundary_may_be_quoted()
{
    for content_type in [
        "Multipart/Form-Data; Boundary=TESTBOUNDARY",
        "multipart/form-data; boundary=\"TESTBOUNDARY\"",
        "multipart/form-data;boundary=TESTBOUNDARY",
        "multipart/form-data; charset=utf-8; boundary=TESTBOUNDARY",
    ] {
        let mut source = RawMultipart::new(
            Some(content_type),
            Body::from(multipart_body(
                B,
                &[file_part("a.pdf", "application/pdf", b"data")],
            )),
        );

        let (_, content) = read_all(&mut source)
            .await
            .unwrap_or_else(|e| panic!("`{content_type}`: {e:?}"));

        assert_eq!(content, b"data", "`{content_type}`");
    }
}

#[tokio::test]
async fn nothing_of_the_body_is_read_before_the_part_is_asked_for() {
    let (body, polled) = crate::attachment_test_env::counting_body(vec![multipart_body(
        B,
        &[file_part("a.pdf", "application/pdf", b"x")],
    )]);
    let mut source = RawMultipart::new(Some(&multipart_content_type(B)), body);
    tokio::task::yield_now().await;

    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "constructing reads nothing"
    );
    drop(source.open_file());
    assert_eq!(
        polled.load(Ordering::SeqCst),
        0,
        "a dropped future reads nothing"
    );
    source.open_file().await.expect("the part is found");
    assert!(
        polled.load(Ordering::SeqCst) > 0,
        "asking for the part reads the body"
    );
}

#[tokio::test]
async fn a_missing_boundary_is_reported_without_reading_the_body() {
    let polled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polled);
    let body = Body::from_stream(stream::poll_fn(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        std::task::Poll::Ready(Some(Ok::<Bytes, std::io::Error>(Bytes::from_static(b"x"))))
    }));
    let mut source = RawMultipart::new(Some("application/json"), body);

    let issue = source.open_file().await.expect_err("an issue");

    assert_eq!(issue, MultipartIssue::BoundaryRequired);
    assert_eq!(polled.load(Ordering::SeqCst), 0);
}

// ---- the file part ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_file_part_is_returned_with_its_headers_and_content() {
    let mut source = source(&[file_part(
        "report.pdf",
        "application/pdf",
        b"%PDF-1.7 hello",
    )]);

    let (header, content) = read_all(&mut source).await.unwrap();

    assert_eq!(header.filename.as_deref(), Some("report.pdf"));
    assert_eq!(
        classify_file(&header.content_type, "report.pdf", &rules())
            .unwrap()
            .content_type,
        "application/pdf"
    );
    assert_eq!(content, b"%PDF-1.7 hello");
}

#[tokio::test]
async fn parts_with_other_names_before_the_file_are_discarded() {
    let mut source = source(&[
        text_part("purpose", "assistants"),
        text_part("meta", "{\"a\":1}"),
        file_part("a.txt", "text/plain", b"hello"),
    ]);

    let (header, content) = read_all(&mut source).await.unwrap();

    assert_eq!(header.filename.as_deref(), Some("a.txt"));
    assert_eq!(content, b"hello");
}

#[tokio::test]
async fn the_part_name_is_compared_case_sensitively() {
    let upper = PartSpec {
        name: "File".to_owned(),
        ..file_part("a.txt", "text/plain", b"upper")
    };

    let mut only_upper = source(std::slice::from_ref(&upper));
    assert_eq!(
        open_err_ref(&mut only_upper).await,
        MultipartIssue::MissingFile
    );

    let mut both = source(&[upper, file_part("b.txt", "text/plain", b"lower")]);
    let (header, content) = read_all(&mut both).await.unwrap();
    assert_eq!(header.filename.as_deref(), Some("b.txt"));
    assert_eq!(content, b"lower");
}

async fn open_err_ref(source: &mut RawMultipart) -> MultipartIssue {
    source.open_file().await.expect_err("an issue")
}

#[tokio::test]
async fn only_the_first_file_part_is_read() {
    let mut source = source(&[
        file_part("first.txt", "text/plain", b"one"),
        file_part("second.pdf", "application/pdf", b"two"),
    ]);

    let (header, content) = read_all(&mut source).await.unwrap();

    assert_eq!(header.filename.as_deref(), Some("first.txt"));
    assert_eq!(content, b"one");
}

#[tokio::test]
async fn the_parts_after_the_file_are_never_read() {
    let mut body = multipart_body(B, &[file_part("a.txt", "text/plain", b"hello")]);
    // Replace the closing boundary by a second, garbage part that never ends.
    let closing = format!("--{B}--\r\n");
    body.truncate(body.len() - closing.len());
    body.extend_from_slice(
        format!("--{B}\r\nthis is not a header and the body stops he").as_bytes(),
    );
    let mut source = raw(&body);

    let (_, content) = read_all(&mut source)
        .await
        .expect("the file part is complete");

    assert_eq!(content, b"hello");
}

#[tokio::test]
async fn binary_content_with_line_breaks_and_boundary_like_text_is_preserved() {
    let mut content: Vec<u8> = (0..=255_u8).cycle().take(5000).collect();
    content.extend_from_slice(b"\r\n--TESTBOUND\r\n--\r\n\r\n");
    content.extend_from_slice(&[0, 13, 10, 13, 10, 45, 45]);
    let mut source = source(&[file_part("a.bin", "application/octet-stream", &content)]);

    let (_, read) = read_all(&mut source).await.unwrap();

    assert_eq!(read, content);
}

#[tokio::test]
async fn an_empty_file_has_no_chunks_and_is_not_an_error() {
    let mut source = source(&[file_part("empty.txt", "text/plain", b"")]);

    let (header, content) = read_all(&mut source).await.unwrap();

    assert_eq!(header.filename.as_deref(), Some("empty.txt"));
    assert!(content.is_empty());
}

#[tokio::test]
async fn a_large_file_is_delivered_in_chunks_and_reassembles() {
    let content: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
    let mut source = source(&[file_part("big.bin", "application/octet-stream", &content)]);
    source.open_file().await.unwrap();
    let mut chunks = 0;
    let mut read = Vec::new();

    while let Some(chunk) = source.next_chunk().await.unwrap() {
        chunks += 1;
        read.extend_from_slice(&chunk);
    }

    assert_eq!(read, content);
    assert!(chunks >= 1);
}

// ---- reasons ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_body_without_a_file_part_is_missing_the_file() {
    let mut no_parts = raw(format!("--{B}--\r\n").as_bytes());
    let mut other_only = source(&[text_part("purpose", "assistants"), text_part("note", "hi")]);

    assert_eq!(
        no_parts.open_file().await.unwrap_err(),
        MultipartIssue::MissingFile
    );
    assert_eq!(
        other_only.open_file().await.unwrap_err(),
        MultipartIssue::MissingFile
    );
}

#[tokio::test]
async fn a_file_part_without_a_content_type_is_rejected() {
    let mut part = file_part("a.pdf", "application/pdf", b"x");
    part.content_type = None;

    assert_eq!(
        open_err(source(&[part])).await,
        MultipartIssue::MissingContentType
    );
}

#[tokio::test]
async fn a_file_part_with_an_empty_content_type_is_rejected() {
    let body = format!(
        "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\nContent-Type: \r\n\r\nx\r\n--{B}--\r\n"
    );

    assert_eq!(
        open_err(raw(body.as_bytes())).await,
        MultipartIssue::MissingContentType
    );
}

#[tokio::test]
async fn a_body_truncated_inside_the_part_headers_is_a_multipart_error() {
    assert_reads_a_valid_body().await;
    let body = format!("--{B}\r\nContent-Disposition: form-data; name=\"file\"; filena");

    assert_eq!(
        open_err(raw(body.as_bytes())).await,
        MultipartIssue::MultipartError
    );
}

async fn assert_reads_a_valid_body() {
    let mut valid = source(&[file_part("a.pdf", "application/pdf", b"ok")]);
    let (_, content) = read_all(&mut valid).await.expect("a complete body is read");
    assert_eq!(content, b"ok");
}

#[tokio::test]
async fn a_body_truncated_inside_a_discarded_part_is_a_multipart_error() {
    assert_reads_a_valid_body().await;
    let body = format!("--{B}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nassist");

    assert_eq!(
        open_err(raw(body.as_bytes())).await,
        MultipartIssue::MultipartError
    );
}

#[tokio::test]
async fn a_body_truncated_inside_the_file_content_fails_the_content_read() {
    let body = format!(
        "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\nabcdef"
    );
    let mut source = raw(body.as_bytes());

    source.open_file().await.expect("the headers are complete");
    let mut outcome = Ok(());
    loop {
        match source.next_chunk().await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(issue) => {
                outcome = Err(issue);
                break;
            }
        }
    }

    assert_eq!(outcome, Err(MultipartIssue::MultipartError));
}

#[tokio::test]
async fn unparsable_part_headers_are_a_multipart_error() {
    assert_reads_a_valid_body().await;
    let body = format!("--{B}\r\nthis is not a header line\r\n\r\nx\r\n--{B}--\r\n");

    assert_eq!(
        open_err(raw(body.as_bytes())).await,
        MultipartIssue::MultipartError
    );
}

#[tokio::test]
async fn a_failing_body_stream_is_a_multipart_error() {
    let head = format!(
        "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.pdf\"\r\nContent-Type: application/pdf\r\n\r\nabc"
    );
    let body = Body::from_stream(stream::iter(vec![
        Ok(Bytes::from(head)),
        Err(std::io::Error::other("connection reset")),
    ]));
    let mut source = RawMultipart::new(Some(&multipart_content_type(B)), body);

    let mut last = match source.open_file().await {
        Err(issue) => Err(issue),
        Ok(_) => Ok(None),
    };
    for _ in 0..4 {
        if last.is_err() {
            break;
        }
        last = source.next_chunk().await;
    }

    assert_eq!(last, Err(MultipartIssue::MultipartError));
}

// ---- file name and content type ----------------------------------------------------------------------------

#[tokio::test]
async fn an_absent_or_empty_filename_becomes_upload() {
    let mut absent = file_part("x", "text/plain", b"x");
    absent.filename = None;
    let mut empty = file_part("x", "text/plain", b"x");
    empty.filename = Some(String::new());

    for part in [absent, empty] {
        let mut source = source(&[part]);

        let (header, _) = read_all(&mut source).await.unwrap();

        assert_eq!(normalize_filename(header.filename.as_deref()), "upload");
    }
}

#[tokio::test]
async fn a_filename_with_separators_and_spaces_is_kept_whole() {
    let mut source = source(&[file_part(
        "my report; v2 (final).pdf",
        "application/pdf",
        b"x",
    )]);

    let (header, _) = read_all(&mut source).await.unwrap();

    assert_eq!(
        header.filename.as_deref(),
        Some("my report; v2 (final).pdf")
    );
}

#[tokio::test]
async fn the_extended_filename_parameter_is_decoded() {
    let body = format!(
        "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf\r\nContent-Type: application/pdf\r\n\r\nx\r\n--{B}--\r\n"
    );
    let mut source = raw(body.as_bytes());

    let (header, _) = read_all(&mut source).await.unwrap();

    assert_eq!(header.filename.as_deref(), Some("r\u{e9}sum\u{e9}.pdf"));
}

#[tokio::test]
async fn the_content_type_is_the_declared_one_with_its_parameters() {
    let mut source = source(&[file_part("a.txt", "Text/Plain; charset=UTF-8", b"x")]);

    let (header, _) = read_all(&mut source).await.unwrap();

    assert_eq!(
        classify_file(&header.content_type, "a.txt", &rules())
            .unwrap()
            .content_type,
        "text/plain",
        "classification normalizes what the reader hands over"
    );
    assert!(
        header
            .content_type
            .to_ascii_lowercase()
            .starts_with("text/plain")
    );
}
