#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;

use super::sse_response;
use crate::api::rest::dto::{MiniChatSseEvent, StreamMessageRequest};
use crate::domain::services::quota_service::{
    QuotaDecisionKind, QuotaPeriodKind, QuotaTierKind, QuotaWarningView,
};
use crate::domain::services::stream::StreamStart;
use crate::domain::services::stream::events::{
    Citation, CitationSource, DeltaKind, DoneData, StreamEvent, StreamStartedData, TextSpan,
    ThreadSummaryInfo, ToolPhase, UsageCounts,
};

/// `(event name, JSON data)` of every SSE frame in `body` (comments skipped).
fn frames(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .filter_map(|block| {
            let mut name = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    name = Some(v.to_owned());
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data.push_str(v);
                }
            }
            name.map(|n| (n, serde_json::from_str(&data).unwrap()))
        })
        .collect()
}

async fn render(events: Vec<StreamEvent>) -> (axum::http::HeaderMap, Vec<(String, Value)>) {
    let resp = sse_response(StreamStart::Replay(events));
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (headers, frames(std::str::from_utf8(&bytes).unwrap()))
}

#[tokio::test]
async fn events_render_with_contract_names_and_payloads() {
    let (rid, mid, att) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let events = vec![
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: rid,
            message_id: mid,
            is_new_turn: true,
            thread_summary_applied: Some(ThreadSummaryInfo { token_estimate: 42 }),
        }),
        StreamEvent::Ping,
        StreamEvent::Delta {
            kind: DeltaKind::Text,
            content: "Hel".to_owned(),
        },
        StreamEvent::Delta {
            kind: DeltaKind::Reasoning,
            content: "hmm".to_owned(),
        },
        StreamEvent::Tool {
            phase: ToolPhase::Done,
            name: "file_search".to_owned(),
            details: json!({"files_searched": 0}),
        },
        StreamEvent::Citations(vec![
            Citation {
                source: CitationSource::File,
                title: "doc.pdf".to_owned(),
                url: None,
                attachment_id: Some(att),
                snippet: String::new(),
                span: None,
            },
            Citation {
                source: CitationSource::Web,
                title: "Web".to_owned(),
                url: Some("https://example.com".to_owned()),
                attachment_id: None,
                snippet: "s".to_owned(),
                span: Some(TextSpan { start: 1, end: 4 }),
            },
        ]),
        StreamEvent::Done(DoneData {
            usage: UsageCounts {
                input_tokens: 12,
                output_tokens: 5,
            },
            effective_model: "s".to_owned(),
            selected_model: "b".to_owned(),
            quota_decision: QuotaDecisionKind::Downgrade,
            downgrade_from: Some("b".to_owned()),
            downgrade_reason: Some("premium_quota_exhausted".to_owned()),
            quota_warnings: Some(vec![QuotaWarningView {
                tier: QuotaTierKind::Premium,
                period: QuotaPeriodKind::Daily,
                remaining_percentage: 0,
                warning: true,
                exhausted: true,
                next_reset: Some(datetime!(2026-10-05 00:00:00 UTC)),
            }]),
        }),
    ];
    let (headers, frames) = render(events).await;
    assert_eq!(headers[CONTENT_TYPE], "text/event-stream");
    assert_eq!(headers[CACHE_CONTROL], "no-cache");
    let names: Vec<&str> = frames.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "stream_started",
            "ping",
            "delta",
            "delta",
            "tool",
            "citations",
            "done"
        ]
    );
    assert_eq!(
        frames[0].1,
        json!({"request_id": rid, "message_id": mid, "is_new_turn": true,
               "thread_summary_applied": {"token_estimate": 42}})
    );
    assert_eq!(frames[1].1, json!({}));
    assert_eq!(frames[2].1, json!({"type": "text", "content": "Hel"}));
    assert_eq!(frames[3].1, json!({"type": "reasoning", "content": "hmm"}));
    assert_eq!(
        frames[4].1,
        json!({"phase": "done", "name": "file_search", "details": {"files_searched": 0}})
    );
    assert_eq!(
        frames[5].1,
        json!({"items": [
            {"source": "file", "title": "doc.pdf", "attachment_id": att, "snippet": ""},
            {"source": "web", "title": "Web", "url": "https://example.com", "snippet": "s",
             "span": {"start": 1, "end": 4}}
        ]})
    );
    assert_eq!(
        frames[6].1,
        json!({
            "usage": {"input_tokens": 12, "output_tokens": 5},
            "effective_model": "s",
            "selected_model": "b",
            "quota_decision": "downgrade",
            "downgrade_from": "b",
            "downgrade_reason": "premium_quota_exhausted",
            "quota_warnings": [{"tier": "premium", "period": "daily", "remaining_percentage": 0,
                                "warning": true, "exhausted": true,
                                "next_reset": "2026-10-05T00:00:00Z"}]
        })
    );
}

#[tokio::test]
async fn optional_fields_are_omitted() {
    let (rid, mid) = (Uuid::new_v4(), Uuid::new_v4());
    let (_, frames) = render(vec![
        StreamEvent::StreamStarted(StreamStartedData {
            request_id: rid,
            message_id: mid,
            is_new_turn: false,
            thread_summary_applied: None,
        }),
        StreamEvent::Done(DoneData {
            usage: UsageCounts::default(),
            effective_model: "b".to_owned(),
            selected_model: "b".to_owned(),
            quota_decision: QuotaDecisionKind::Allow,
            downgrade_from: None,
            downgrade_reason: None,
            quota_warnings: None,
        }),
    ])
    .await;
    assert_eq!(
        frames[0].1,
        json!({"request_id": rid, "message_id": mid, "is_new_turn": false})
    );
    assert_eq!(
        frames[1].1,
        json!({"usage": {"input_tokens": 0, "output_tokens": 0}, "effective_model": "b",
               "selected_model": "b", "quota_decision": "allow"})
    );
}

#[tokio::test]
async fn error_event_is_code_and_message() {
    let (_, frames) = render(vec![StreamEvent::error("provider_error", "boom")]).await;
    assert_eq!(
        frames,
        vec![(
            "error".to_owned(),
            json!({"code": "provider_error", "message": "boom"})
        )]
    );
}

#[test]
fn stream_message_request_defaults() {
    let req: StreamMessageRequest = serde_json::from_value(json!({"content": "hi"})).unwrap();
    assert_eq!(req.content, "hi");
    assert_eq!(req.request_id, None);
    assert!(req.attachment_ids.is_empty());
    assert!(req.web_search.is_none());
    let req: StreamMessageRequest = serde_json::from_value(json!({
        "content": "hi", "request_id": null, "attachment_ids": [], "web_search": {"enabled": true}
    }))
    .unwrap();
    assert!(req.web_search.unwrap().enabled);
}

#[test]
fn sse_event_schema_is_a_oneof_of_named_events() {
    let schema =
        serde_json::to_value(<MiniChatSseEvent as utoipa::PartialSchema>::schema()).unwrap();
    let variants = schema["oneOf"].as_array().expect("oneOf");
    let mut seen = Vec::new();
    for v in variants {
        let event = v["properties"]["event"]["enum"][0]
            .as_str()
            .unwrap()
            .to_owned();
        let data_ref = v["properties"]["data"]["$ref"].as_str().unwrap().to_owned();
        let mut required: Vec<&str> = v["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r.as_str().unwrap())
            .collect();
        required.sort_unstable();
        assert_eq!(required, ["data", "event"], "{v}");
        seen.push((event, data_ref));
    }
    let expect = [
        ("stream_started", "StreamStartedData"),
        ("ping", "PingData"),
        ("delta", "DeltaData"),
        ("tool", "ToolData"),
        ("citations", "CitationsData"),
        ("done", "DoneData"),
        ("error", "ErrorData"),
    ];
    let expect: Vec<(String, String)> = expect
        .iter()
        .map(|(e, d)| ((*e).to_owned(), format!("#/components/schemas/{d}")))
        .collect();
    assert_eq!(seen, expect);
}
