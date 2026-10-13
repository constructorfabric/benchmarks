#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the SSE wire format and the response of the send endpoint.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::to_bytes;
use futures::{Stream, StreamExt};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::preflight::{DowngradeReason, QuotaDecision};
use crate::domain::service::quota::status::QuotaWarningView;
use crate::domain::service::quota::types::{QuotaPeriod, QuotaTier};
use crate::domain::service::stream::types::{
    CitationItem, CitationSource, DeltaKind, DoneEvent, ErrorEvent, StreamErrorCode, StreamStarted,
    ToolName, ToolPhase,
};

fn data(event: &StreamEvent) -> Value {
    serde_json::from_str(&event_data(event)).expect("the data is JSON")
}

fn started(summary: Option<u32>) -> StreamEvent {
    StreamEvent::StreamStarted(StreamStarted {
        request_id: Uuid::from_u128(1),
        message_id: Uuid::from_u128(2),
        is_new_turn: true,
        thread_summary_applied: summary,
    })
}

fn done() -> DoneEvent {
    DoneEvent {
        usage: (120, 34),
        effective_model: "gpt-5.2".to_owned(),
        selected_model: "gpt-5.2".to_owned(),
        quota_decision: QuotaDecision::Allow,
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: Some(Vec::new()),
    }
}

// ---- the data of every event --------------------------------------------------------------------

#[test]
fn stream_started_carries_the_ids_and_the_new_turn_flag() {
    assert_eq!(
        event_data(&started(None)),
        format!(
            "{{\"request_id\":\"{}\",\"message_id\":\"{}\",\"is_new_turn\":true}}",
            Uuid::from_u128(1),
            Uuid::from_u128(2)
        ),
        "compact JSON, the optional key omitted"
    );
}

#[test]
fn stream_started_reports_a_kept_summary() {
    assert_eq!(
        data(&started(Some(42)))["thread_summary_applied"],
        json!({ "token_estimate": 42 })
    );
}

#[test]
fn a_replayed_stream_started_has_is_new_turn_false() {
    let event = StreamEvent::StreamStarted(StreamStarted {
        request_id: Uuid::from_u128(1),
        message_id: Uuid::from_u128(2),
        is_new_turn: false,
        thread_summary_applied: None,
    });
    assert_eq!(data(&event)["is_new_turn"], false);
}

#[test]
fn ping_is_an_empty_object() {
    assert_eq!(event_data(&StreamEvent::Ping), "{}");
}

#[test]
fn deltas_have_a_type_and_content() {
    assert_eq!(
        event_data(&StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "Hel".to_owned()
        }),
        "{\"type\":\"text\",\"content\":\"Hel\"}"
    );
    assert_eq!(
        data(&StreamEvent::Delta {
            kind: DeltaKind::Reasoning,
            content: "hmm".to_owned()
        }),
        json!({ "type": "reasoning", "content": "hmm" })
    );
}

#[test]
fn delta_content_is_escaped_so_that_the_data_stays_on_one_line() {
    let event = StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: "line1\nline2 \"quoted\" \u{e9}\u{1f600}\r\n".to_owned(),
    };
    let text = event_data(&event);
    assert!(!text.contains('\n') && !text.contains('\r'), "{text}");
    assert_eq!(
        data(&event)["content"],
        "line1\nline2 \"quoted\" \u{e9}\u{1f600}\r\n"
    );
}

#[test]
fn tool_events_carry_phase_name_and_details() {
    assert_eq!(
        event_data(&StreamEvent::Tool {
            phase: ToolPhase::Start,
            name: ToolName::WebSearch,
            details: json!({}),
        }),
        "{\"phase\":\"start\",\"name\":\"web_search\",\"details\":{}}"
    );
    assert_eq!(
        data(&StreamEvent::Tool {
            phase: ToolPhase::Done,
            name: ToolName::FileSearch,
            details: json!({ "files_searched": 3 }),
        }),
        json!({ "phase": "done", "name": "file_search", "details": { "files_searched": 3 } })
    );
    assert_eq!(
        data(&StreamEvent::Tool {
            phase: ToolPhase::Done,
            name: ToolName::CodeInterpreter,
            details: json!({ "output": "42" }),
        })["details"],
        json!({ "output": "42" })
    );
}

#[test]
fn every_tool_name_has_its_wire_value() {
    for (name, wire) in [
        (ToolName::FileSearch, "file_search"),
        (ToolName::WebSearch, "web_search"),
        (ToolName::CodeInterpreter, "code_interpreter"),
        (ToolName::FunctionCall, "function_call"),
        (ToolName::SearchKnowledge, "search_knowledge"),
        (ToolName::LoadFiles, "load_files"),
        (ToolName::UnknownTool, "unknown_tool"),
    ] {
        let event = StreamEvent::Tool {
            phase: ToolPhase::Start,
            name,
            details: json!({}),
        };
        assert_eq!(data(&event)["name"], wire);
    }
}

#[test]
fn citations_omit_absent_keys_and_never_carry_a_score() {
    let event = StreamEvent::Citations(vec![
        CitationItem {
            source: CitationSource::Web,
            title: "Page".to_owned(),
            snippet: "quoted".to_owned(),
            url: Some("https://e.org/p".to_owned()),
            attachment_id: None,
            span: Some((3, 9)),
        },
        CitationItem {
            source: CitationSource::File,
            title: "report.pdf".to_owned(),
            snippet: String::new(),
            url: None,
            attachment_id: Some(Uuid::from_u128(0x1001)),
            span: None,
        },
    ]);
    assert_eq!(
        data(&event),
        json!({ "items": [
            { "source": "web", "title": "Page", "snippet": "quoted", "url": "https://e.org/p", "span": { "start": 3, "end": 9 } },
            { "source": "file", "title": "report.pdf", "snippet": "", "attachment_id": Uuid::from_u128(0x1001).to_string() }
        ]})
    );
    assert!(!event_data(&event).contains("score"));
    assert!(!event_data(&event).contains("null"));
}

#[test]
fn done_has_the_usage_the_models_and_the_decision() {
    assert_eq!(
        data(&StreamEvent::Done(done())),
        json!({
            "usage": { "input_tokens": 120, "output_tokens": 34 },
            "effective_model": "gpt-5.2",
            "selected_model": "gpt-5.2",
            "quota_decision": "allow",
            "quota_warnings": []
        })
    );
}

#[test]
fn done_after_a_downgrade_names_the_origin_and_the_reason() {
    let event = StreamEvent::Done(DoneEvent {
        effective_model: "gpt-4.1".to_owned(),
        quota_decision: QuotaDecision::Downgrade,
        downgrade_from: Some("gpt-5.2".to_owned()),
        downgrade_reason: Some(DowngradeReason::PremiumQuotaExhausted),
        ..done()
    });
    let value = data(&event);
    assert_eq!(value["quota_decision"], "downgrade");
    assert_eq!(value["downgrade_from"], "gpt-5.2");
    assert_eq!(value["downgrade_reason"], "premium_quota_exhausted");
}

#[test]
fn every_downgrade_reason_has_its_wire_value() {
    for (reason, wire) in [
        (
            DowngradeReason::PremiumQuotaExhausted,
            "premium_quota_exhausted",
        ),
        (DowngradeReason::ForceStandardTier, "force_standard_tier"),
        (DowngradeReason::DisablePremiumTier, "disable_premium_tier"),
        (DowngradeReason::ModelDisabled, "model_disabled"),
    ] {
        let event = StreamEvent::Done(DoneEvent {
            quota_decision: QuotaDecision::Downgrade,
            downgrade_from: Some("a".to_owned()),
            downgrade_reason: Some(reason),
            ..done()
        });
        assert_eq!(data(&event)["downgrade_reason"], wire);
    }
}

#[test]
fn done_without_warnings_omits_the_key() {
    let event = StreamEvent::Done(DoneEvent {
        quota_warnings: None,
        ..done()
    });
    assert!(data(&event).get("quota_warnings").is_none());
}

#[test]
fn quota_warnings_carry_the_period_the_flags_and_the_next_reset() {
    let event = StreamEvent::Done(DoneEvent {
        quota_warnings: Some(vec![QuotaWarningView {
            tier: QuotaTier::Premium,
            period: QuotaPeriod::Daily,
            remaining_percentage: 9,
            warning: true,
            exhausted: false,
            next_reset: Some(OffsetDateTime::from_unix_timestamp(1_773_100_800).unwrap()),
        }]),
        ..done()
    });
    assert_eq!(
        data(&event)["quota_warnings"],
        json!([{
            "tier": "premium", "period": "daily", "remaining_percentage": 9,
            "warning": true, "exhausted": false, "next_reset": "2026-03-10T00:00:00Z"
        }])
    );
}

#[test]
fn done_never_exposes_identifiers_or_settlement_details() {
    let text = event_data(&StreamEvent::Done(done()));
    for forbidden in [
        "request_id",
        "message_id",
        "provider_response_id",
        "resp_",
        "credits",
        "settle",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
}

#[test]
fn error_has_a_code_and_a_message() {
    let event = StreamEvent::Error(ErrorEvent {
        code: StreamErrorCode::RateLimited,
        message: "Slow down. Retry after 7 seconds.".to_owned(),
    });
    assert_eq!(
        event_data(&event),
        "{\"code\":\"rate_limited\",\"message\":\"Slow down. Retry after 7 seconds.\"}"
    );
}

#[test]
fn every_streaming_error_code_has_its_wire_value() {
    for code in [
        StreamErrorCode::ProviderError,
        StreamErrorCode::ProviderTimeout,
        StreamErrorCode::RateLimited,
        StreamErrorCode::WebSearchCallsExceeded,
        StreamErrorCode::CodeInterpreterCallsExceeded,
        StreamErrorCode::AgenticIterationsExceeded,
        StreamErrorCode::UnexpectedToolUse,
        StreamErrorCode::MessagePersistenceFailed,
        StreamErrorCode::FinalizationFailed,
        StreamErrorCode::StreamInterrupted,
    ] {
        let event = StreamEvent::Error(ErrorEvent {
            code,
            message: "m".to_owned(),
        });
        assert_eq!(data(&event)["code"], code.as_str());
    }
}

// ---- the frame ----------------------------------------------------------------------------------

#[test]
fn a_frame_is_an_event_line_a_data_line_and_a_blank_line() {
    assert_eq!(
        format_frame(&StreamEvent::Ping),
        "event: ping\ndata: {}\n\n"
    );
    let frame = format_frame(&StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: "x".to_owned(),
    });
    assert_eq!(
        frame,
        "event: delta\ndata: {\"type\":\"text\",\"content\":\"x\"}\n\n"
    );
}

#[test]
fn every_event_uses_its_contract_name() {
    for (event, name) in [
        (started(None), "stream_started"),
        (StreamEvent::Ping, "ping"),
        (StreamEvent::Done(done()), "done"),
        (
            StreamEvent::Error(ErrorEvent {
                code: StreamErrorCode::ProviderError,
                message: String::new(),
            }),
            "error",
        ),
        (StreamEvent::Citations(Vec::new()), "citations"),
    ] {
        assert!(format_frame(&event).starts_with(&format!("event: {name}\ndata: ")));
    }
}

// ---- the response -------------------------------------------------------------------------------

fn events() -> Vec<StreamEvent> {
    vec![
        started(None),
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "Hi".to_owned(),
        },
        StreamEvent::Done(done()),
    ]
}

#[tokio::test]
async fn the_response_is_a_200_event_stream_with_the_documented_headers() {
    let response = sse_response(futures::stream::iter(events()));
    assert_eq!(response.status(), 200);
    let headers = response.headers();
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"),
        "{:?}",
        headers["content-type"]
    );
    assert_eq!(headers["cache-control"], "no-cache");
    assert!(
        headers.get("connection").is_none(),
        "no Connection: keep-alive"
    );
    assert!(headers.get("content-encoding").is_none(), "no compression");
    assert!(
        headers.get("content-length").is_none(),
        "a stream has no length"
    );
}

#[tokio::test]
async fn the_body_is_the_frames_in_order_and_ends_with_the_stream() {
    let response = sse_response(futures::stream::iter(events()));
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let expected: String = events().iter().map(format_frame).collect();
    assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), expected);
}

/// A stream fed by a channel that reports its drop.
struct Fed {
    rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    dropped: Arc<AtomicBool>,
}

impl Stream for Fed {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for Fed {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

fn fed() -> (
    tokio::sync::mpsc::UnboundedSender<StreamEvent>,
    Arc<AtomicBool>,
    Fed,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let dropped = Arc::new(AtomicBool::new(false));
    (tx, Arc::clone(&dropped), Fed { rx, dropped })
}

#[tokio::test]
async fn each_event_is_flushed_as_soon_as_it_is_yielded() {
    let (tx, _dropped, stream) = fed();
    let mut body = sse_response(stream).into_body().into_data_stream();
    tx.send(started(None)).unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .expect("the first frame is flushed")
        .unwrap()
        .unwrap();
    assert_eq!(
        String::from_utf8(first.to_vec()).unwrap(),
        format_frame(&started(None))
    );
    // Nothing else is available: the body does not wait for more events to
    // emit what it has, nor emit what it has not.
    tx.send(StreamEvent::Ping).unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        String::from_utf8(second.to_vec()).unwrap(),
        "event: ping\ndata: {}\n\n"
    );
}

#[tokio::test]
async fn dropping_the_body_drops_the_event_stream() {
    let (_tx, dropped, stream) = fed();
    let response = sse_response(stream);
    assert!(!dropped.load(Ordering::SeqCst));
    drop(response);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the disconnect reaches the relay"
    );
}

#[tokio::test]
async fn dropping_a_started_body_drops_the_event_stream() {
    let (tx, dropped, stream) = fed();
    let mut body = sse_response(stream).into_body().into_data_stream();
    tx.send(started(None)).unwrap();
    body.next().await;
    drop(body);
    assert!(dropped.load(Ordering::SeqCst));
}

#[tokio::test(start_paused = true)]
async fn the_http_layer_adds_a_comment_keep_alive_every_thirty_seconds() {
    let (_tx, _dropped, stream) = fed();
    let mut body = sse_response(stream).into_body().into_data_stream();
    let before = tokio::time::Instant::now();
    let frame = body.next().await.unwrap().unwrap();
    assert_eq!(before.elapsed(), Duration::from_secs(30));
    let text = String::from_utf8(frame.to_vec()).unwrap();
    assert!(text.starts_with(':') && text.ends_with("\n\n"), "{text:?}");
    assert!(!text.contains("event:"), "a comment is no event");
}
