#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{SseFrame, SseReader};

fn frame(event: &str, data: &str) -> SseFrame {
    SseFrame {
        event: event.to_owned(),
        data: data.to_owned(),
    }
}

#[test]
fn yields_events_as_soon_as_complete() {
    let mut r = SseReader::new();
    assert!(r.push(b"event: a\ndata: {\"x\":1}\n").is_empty());
    assert_eq!(r.push(b"\n"), vec![frame("a", "{\"x\":1}")]);
    assert_eq!(
        r.push(b"data: one\ndata: two\n\nevent: b\ndata: 2\n\n"),
        vec![frame("", "one\ntwo"), frame("b", "2")]
    );
    assert_eq!(r.finish(), None);
}

#[test]
fn crlf_cr_and_split_chunks() {
    let mut r = SseReader::new();
    let mut out = Vec::new();
    for chunk in [
        &b"\xEF\xBB\xBFevent: a\r"[..],
        b"\ndata: h\xC3",
        b"\xA9\r\n\r",
        b"\nevent:b\rdata:x\r\r",
    ] {
        out.extend(r.push(chunk));
    }
    out.extend(r.finish());
    assert_eq!(out, vec![frame("a", "h\u{e9}"), frame("b", "x")]);
}

#[test]
fn comments_unknown_fields_and_eventless_blocks_are_skipped() {
    let mut r = SseReader::new();
    assert_eq!(
        r.push(b": keep-alive\n\nid: 7\nretry: 10\n\nfoo: bar\ndata: d\n\n"),
        vec![frame("", "d")]
    );
}

#[test]
fn trailing_event_without_blank_line_is_flushed() {
    let mut r = SseReader::new();
    assert!(r.push(b"event: response.completed\ndata: {}").is_empty());
    assert_eq!(r.finish(), Some(frame("response.completed", "{}")));
    assert_eq!(r.finish(), None);
}

#[test]
fn cr_then_two_lf_chunks_is_one_event() {
    let mut r = SseReader::new();
    let mut out = Vec::new();
    for chunk in [&b"data: a\r"[..], b"\n", b"\n"] {
        out.extend(r.push(chunk));
    }
    assert_eq!(out, vec![frame("", "a")]);
    assert_eq!(r.finish(), None);
}
