use serde_json::json;

use super::*;

#[test]
fn parses_events_split_across_chunks() {
    let mut p = SseParser::default();
    let mut out = p.push(b"event: response.output_text.delta\nda");
    assert!(out.is_empty());
    out.extend(p.push(b"ta: {\"delta\":\"Hi\"}\n\nevent: x\r\ndata: a\r\ndata: b\r\n\r\n"));
    assert_eq!(
        out,
        vec![
            SseEvent {
                event: Some("response.output_text.delta".into()),
                data: "{\"delta\":\"Hi\"}".into(),
            },
            SseEvent {
                event: Some("x".into()),
                data: "a\nb".into(),
            },
        ]
    );
}

#[test]
fn utf8_split_inside_a_character_is_kept() {
    let mut p = SseParser::default();
    let bytes = "data: h\u{e9}llo\n\n".as_bytes();
    let (a, b) = bytes.split_at(8); // splits the two-byte 'é'
    assert!(p.push(a).is_empty());
    assert_eq!(p.push(b)[0].data, "h\u{e9}llo");
}

#[test]
fn comments_are_skipped_and_finish_flushes() {
    let mut p = SseParser::default();
    assert!(p.push(b": ping\n\n").is_empty());
    assert!(p.push(b"data: tail").is_empty());
    assert_eq!(
        p.finish(),
        Some(SseEvent {
            event: None,
            data: "tail".into()
        })
    );
    assert_eq!(p.finish(), None);
}

#[test]
fn name_falls_back_to_type_in_data() {
    let typed = json!({"type": "response.completed"}).to_string();
    let e = SseEvent {
        event: None,
        data: typed.clone(),
    };
    assert_eq!(e.name().as_deref(), Some("response.completed"));
    let e = SseEvent {
        event: Some("message".into()),
        data: typed,
    };
    assert_eq!(e.name().as_deref(), Some("response.completed"));
    let e = SseEvent {
        event: Some("error".into()),
        data: "{}".into(),
    };
    assert_eq!(e.name().as_deref(), Some("error"));
    let e = SseEvent {
        event: None,
        data: "not json".into(),
    };
    assert_eq!(e.name(), None);
}
