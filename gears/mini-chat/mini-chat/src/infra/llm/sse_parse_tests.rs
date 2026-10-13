#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::format_push_string, clippy::non_ascii_literal)]
//! Slice 4: the incremental SSE parser.

use super::*;

fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
    let mut parser = SseParser::new();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(parser.push(chunk));
    }
    out.extend(parser.finish());
    out
}

fn event(name: Option<&str>, data: &str) -> SseEvent {
    SseEvent {
        event: name.map(str::to_owned),
        data: data.to_owned(),
        id: None,
    }
}

#[test]
fn one_event_with_a_name_and_data() {
    let events = parse_all(&[b"event: response.output_text.delta\ndata: {\"delta\":\"Hi\"}\n\n"]);
    assert_eq!(
        events,
        vec![event(
            Some("response.output_text.delta"),
            "{\"delta\":\"Hi\"}"
        )]
    );
}

#[test]
fn several_events_in_one_chunk() {
    let events = parse_all(&[b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\ndata: 3\n\n"]);
    assert_eq!(
        events,
        vec![
            event(Some("a"), "1"),
            event(Some("b"), "2"),
            event(None, "3")
        ]
    );
}

#[test]
fn an_event_is_yielded_as_soon_as_its_blank_line_arrives() {
    let mut parser = SseParser::new();
    assert!(parser.push(b"event: a\n").is_empty());
    assert!(parser.push(b"data: 1\n").is_empty());
    assert_eq!(parser.push(b"\n"), vec![event(Some("a"), "1")]);
    assert!(
        parser.push(b"data: 2\n").is_empty(),
        "the next one is not complete yet"
    );
}

#[test]
fn the_result_does_not_depend_on_how_the_body_is_chunked() {
    let body = "event: a\ndata: {\"t\":\"héllo wörld\"}\n\n: comment\ndata: line1\ndata: line2\n\nevent: z\ndata: [DONE]\n\n".as_bytes();
    let whole = parse_all(&[body]);
    assert_eq!(whole.len(), 3);
    for split in 1..body.len() {
        let (left, right) = body.split_at(split);
        assert_eq!(parse_all(&[left, right]), whole, "split at {split}");
    }
    let bytes: Vec<&[u8]> = body.chunks(1).collect();
    assert_eq!(parse_all(&bytes), whole, "byte by byte");
}

#[test]
fn a_multibyte_character_split_between_chunks_is_decoded() {
    let body = "data: caf\u{e9} \u{1f600}\n\n".as_bytes();
    // Split inside the two-byte and inside the four-byte sequences.
    for split in 1..body.len() {
        let (left, right) = body.split_at(split);
        assert_eq!(
            parse_all(&[left, right]),
            vec![event(None, "caf\u{e9} \u{1f600}")]
        );
    }
}

#[test]
fn crlf_and_cr_line_endings_are_line_breaks() {
    assert_eq!(
        parse_all(&[b"event: a\r\ndata: 1\r\n\r\n"]),
        vec![event(Some("a"), "1")]
    );
    assert_eq!(
        parse_all(&[b"event: a\rdata: 1\r\r"]),
        vec![event(Some("a"), "1")]
    );
}

#[test]
fn a_crlf_split_between_chunks_is_one_line_break() {
    // "data: a\r" | "\n\r\n": the CR ends the line, the LF belongs to it, the
    // second CRLF is the blank line: exactly one event.
    assert_eq!(
        parse_all(&[b"data: a\r", b"\n\r\n"]),
        vec![event(None, "a")]
    );
    assert_eq!(
        parse_all(&[b"data: a\r\n", b"\r", b"\n"]),
        vec![event(None, "a")]
    );
}

#[test]
fn comment_lines_are_ignored_and_do_not_end_an_event() {
    assert_eq!(
        parse_all(&[b": keep-alive\n\n: another\nevent: a\n: inside\ndata: 1\n\n"]),
        vec![event(Some("a"), "1")]
    );
}

#[test]
fn data_lines_are_joined_by_a_newline() {
    assert_eq!(
        parse_all(&[b"data: line1\ndata: line2\ndata:\ndata: line4\n\n"]),
        vec![event(None, "line1\nline2\n\nline4")]
    );
}

#[test]
fn only_one_leading_space_of_a_value_is_removed() {
    assert_eq!(parse_all(&[b"data:  two\n\n"]), vec![event(None, " two")]);
    assert_eq!(parse_all(&[b"data:none\n\n"]), vec![event(None, "none")]);
    assert_eq!(
        parse_all(&[b"event:x\ndata:y\n\n"]),
        vec![event(Some("x"), "y")]
    );
}

#[test]
fn a_value_may_contain_colons() {
    assert_eq!(
        parse_all(&[b"data: {\"a\":\"b:c\"}\n\n"]),
        vec![event(None, "{\"a\":\"b:c\"}")]
    );
}

#[test]
fn an_event_with_empty_data_is_still_an_event() {
    assert_eq!(parse_all(&[b"data:\n\n"]), vec![event(None, "")]);
}

#[test]
fn a_block_without_data_is_no_event() {
    assert!(parse_all(&[b"event: a\n\n"]).is_empty());
    assert!(parse_all(&[b"id: 7\n\n"]).is_empty());
    assert!(parse_all(&[b"\n\n\n"]).is_empty());
}

#[test]
fn the_event_name_does_not_leak_into_the_next_event() {
    assert_eq!(
        parse_all(&[b"event: a\ndata: 1\n\ndata: 2\n\n"]),
        vec![event(Some("a"), "1"), event(None, "2")]
    );
}

#[test]
fn the_id_is_captured_and_retry_and_unknown_fields_are_ignored() {
    let events = parse_all(&[b"id: 42\nretry: 3000\nfoo: bar\nevent: a\ndata: 1\n\n"]);
    assert_eq!(
        events,
        vec![SseEvent {
            event: Some("a".to_owned()),
            data: "1".to_owned(),
            id: Some("42".to_owned()),
        }]
    );
}

#[test]
fn a_leading_bom_is_dropped() {
    assert_eq!(
        parse_all(&[b"\xEF\xBB\xBFdata: 1\n\n"]),
        vec![event(None, "1")]
    );
    // Also when the BOM is split from the rest.
    assert_eq!(
        parse_all(&[b"\xEF\xBB", b"\xBFdata: 1\n\n"]),
        vec![event(None, "1")]
    );
}

#[test]
fn done_is_plain_data() {
    assert_eq!(
        parse_all(&[b"data: [DONE]\n\n"]),
        vec![event(None, "[DONE]")]
    );
}

#[test]
fn an_empty_chunk_changes_nothing() {
    let mut parser = SseParser::new();
    assert!(parser.push(b"").is_empty());
    assert!(parser.push(b"data: 1\n").is_empty());
    assert!(parser.push(b"").is_empty());
    assert_eq!(parser.push(b"\n"), vec![event(None, "1")]);
}

#[test]
fn a_large_event_is_parsed() {
    let payload = "x".repeat(1_000_000);
    let body = format!("data: {payload}\n\n");
    let chunks: Vec<&[u8]> = body.as_bytes().chunks(4096).collect();
    let events = parse_all(&chunks);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data.len(), 1_000_000);
}

#[test]
fn many_events_are_parsed_in_order() {
    let mut body = String::new();
    for n in 0..2000 {
        body.push_str(&format!("event: e\ndata: {n}\n\n"));
    }
    let events = parse_all(&[body.as_bytes()]);
    assert_eq!(events.len(), 2000);
    assert_eq!(events[1999].data, "1999");
}

#[test]
fn a_final_event_without_its_blank_line_is_yielded_at_the_end_of_the_body() {
    assert_eq!(
        parse_all(&[b"data: 1\n\ndata: last\n"]),
        vec![event(None, "1"), event(None, "last")]
    );
    assert_eq!(parse_all(&[b"data: last"]), vec![event(None, "last")]);
}

#[test]
fn an_empty_tail_yields_nothing_at_the_end() {
    assert_eq!(parse_all(&[b"data: 1\n\n\n\n"]).len(), 1);
    assert!(parse_all(&[b""]).is_empty());
    assert!(parse_all(&[b": only a comment\n"]).is_empty());
}

#[test]
fn invalid_utf8_is_replaced_not_fatal() {
    let events = parse_all(&[b"data: a\xFFb\n\n"]);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data, "a\u{fffd}b");
}

#[test]
fn the_parser_can_be_reused_after_the_end_of_a_body() {
    let mut parser = SseParser::new();
    let _ = parser.push(b"data: 1\n\n");
    assert!(parser.finish().is_empty());
    assert_eq!(parser.push(b"data: 2\n\n"), vec![event(None, "2")]);
}
