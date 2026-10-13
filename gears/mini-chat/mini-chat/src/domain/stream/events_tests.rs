#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use super::*;
use crate::domain::quota_service::QuotaDecisionKind;

fn done() -> StreamEvent {
    StreamEvent::Done(Box::new(Done {
        usage: DoneUsage::default(),
        effective_model: "m".to_owned(),
        selected_model: "m".to_owned(),
        quota_decision: QuotaDecisionKind::Allow,
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: None,
    }))
}

#[test]
fn event_names_follow_the_sse_contract() {
    let started = StreamEvent::StreamStarted(StreamStarted {
        request_id: Uuid::nil(),
        message_id: Uuid::nil(),
        is_new_turn: true,
        thread_summary_applied: None,
    });
    let tool = StreamEvent::Tool(ToolActivity {
        phase: ToolPhase::Start,
        name: "web_search".to_owned(),
        details: serde_json::json!({}),
    });
    let events = [
        started,
        StreamEvent::text("hi"),
        tool,
        StreamEvent::Citations(Vec::new()),
        done(),
        StreamEvent::error(PROVIDER_ERROR, "boom"),
    ];

    let names: Vec<&str> = events.iter().map(StreamEvent::name).collect();
    assert_eq!(
        names,
        [
            "stream_started",
            "delta",
            "tool",
            "citations",
            "done",
            "error"
        ]
    );
    let terminal: Vec<bool> = events.iter().map(StreamEvent::is_terminal).collect();
    assert_eq!(terminal, [false, false, false, false, true, true]);
}

#[test]
fn wire_values_of_kinds_phases_and_sources() {
    assert_eq!(DeltaKind::Text.as_str(), "text");
    assert_eq!(DeltaKind::Reasoning.as_str(), "reasoning");
    assert_eq!(ToolPhase::Start.as_str(), "start");
    assert_eq!(ToolPhase::Done.as_str(), "done");
    assert_eq!(CitationSource::File.as_str(), "file");
    assert_eq!(CitationSource::Web.as_str(), "web");
}

#[test]
fn text_delta_and_stream_interrupted_constructors() {
    assert_eq!(
        StreamEvent::text("abc"),
        StreamEvent::Delta(Delta {
            kind: DeltaKind::Text,
            content: "abc".to_owned()
        })
    );
    let interrupted = StreamError::stream_interrupted();
    assert_eq!(interrupted.code, "stream_interrupted");
    assert!(!interrupted.message.is_empty());
}
