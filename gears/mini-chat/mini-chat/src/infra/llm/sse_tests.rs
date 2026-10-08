#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn frame(event: Option<&str>, data: &str) -> SseFrame {
    SseFrame {
        event: event.map(str::to_owned),
        data: data.to_owned(),
    }
}

fn feed(chunks: &[&[u8]]) -> Vec<SseFrame> {
    let mut parser = SseParser::default();
    chunks.iter().flat_map(|c| parser.push(c)).collect()
}

#[test]
fn parses_split_frames() {
    // A frame split inside the field name, the value, a multi-byte character
    // and the terminating blank line.
    let body =
        "event: response.output_text.delta\ndata: {\"delta\":\"h\u{e9}\"}\n\ndata: second\n\n";
    let bytes = body.as_bytes();
    let accent = body.find('\u{e9}').unwrap();
    let cuts = [3, 20, accent + 1, bytes.len() - 1];
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut prev = 0;
    for cut in cuts {
        chunks.push(&bytes[prev..cut]);
        prev = cut;
    }
    chunks.push(&bytes[prev..]);

    assert_eq!(
        feed(&chunks),
        vec![
            frame(
                Some("response.output_text.delta"),
                "{\"delta\":\"h\u{e9}\"}"
            ),
            frame(None, "second"),
        ]
    );
    // Same result byte by byte.
    let singles: Vec<&[u8]> = bytes.chunks(1).collect();
    assert_eq!(feed(&singles).len(), 2);
}

#[test]
fn multi_line_data() {
    let frames = feed(&[b"event: x\ndata: line1\ndata:line2\ndata\n\n"]);
    assert_eq!(frames, vec![frame(Some("x"), "line1\nline2\n")]);
}

#[test]
fn ignores_comments() {
    let frames = feed(&[b": keep-alive\n\n:another\ndata: a\n: mid-frame comment\ndata: b\n\n"]);
    // A comment-only block dispatches nothing; a comment inside a frame is skipped.
    assert_eq!(frames, vec![frame(None, "a\nb")]);
}

#[test]
fn crlf_and_cr_line_endings() {
    // CRLF split between chunks must not produce an extra empty line.
    let frames = feed(&[
        b"event: e1\r",
        b"\ndata: one\r\n\r",
        b"\nevent: e2\rdata: two\r\r",
    ]);
    assert_eq!(
        frames,
        vec![frame(Some("e1"), "one"), frame(Some("e2"), "two")]
    );
}

#[test]
fn event_field_resets_after_dispatch_and_ignores_id_retry() {
    let frames = feed(&[b"event: a\nid: 7\nretry: 100\ndata: 1\n\ndata: 2\n\nevent: dangling\n\n"]);
    assert_eq!(frames, vec![frame(Some("a"), "1"), frame(None, "2")]);
}

#[test]
fn incomplete_trailing_frame_is_not_dispatched() {
    let frames = feed(&[b"data: done\n\ndata: partial"]);
    assert_eq!(frames, vec![frame(None, "done")]);
}
