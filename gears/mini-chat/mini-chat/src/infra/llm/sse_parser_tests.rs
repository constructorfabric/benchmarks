use super::{SseEvent, SseParser};

#[test]
fn parses_named_events_across_chunks() {
    let mut p = SseParser::new();
    assert!(p.feed(b"event: response.output_text.delta\nda").is_empty());
    let evs = p.feed(b"ta: {\"delta\":\"Hi\"}\n\n");
    assert_eq!(
        evs,
        vec![SseEvent {
            event: Some("response.output_text.delta".into()),
            data: "{\"delta\":\"Hi\"}".into()
        }]
    );
}

#[test]
fn handles_crlf_comments_and_multiline_data() {
    let mut p = SseParser::new();
    let evs = p.feed(b": keepalive\r\n\r\ndata: a\r\ndata: b\r\n\r\ndata: [DONE]\n\n");
    assert_eq!(evs.len(), 2);
    assert_eq!(evs[0].data, "a\nb");
    assert_eq!(evs[0].event, None);
    assert_eq!(evs[1].data, "[DONE]");
}

#[test]
fn finish_flushes_trailing_event() {
    let mut p = SseParser::new();
    assert!(p.feed(b"event: done\ndata: {}").is_empty());
    let ev = p.finish().unwrap_or_default();
    assert_eq!(ev.event.as_deref(), Some("done"));
    assert_eq!(ev.data, "{}");
}
