//! SSE writer: serializes stream events, emits `ping` before the first
//! content event, synthesizes `stream_interrupted` when the provider task
//! ends without a terminal event, and cancels the turn when dropped.

use std::convert::Infallible;
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::Stream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;

use crate::domain::stream::{StreamEvent, StreamStart};
use crate::infra::llm::DeltaKind;

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// `(event name, JSON payload)` of a stream event.
#[must_use]
pub fn event_payload(ev: &StreamEvent) -> (&'static str, Value) {
    match ev {
        StreamEvent::StreamStarted {
            request_id,
            message_id,
            is_new_turn,
            thread_summary_applied,
        } => {
            let mut v = json!({
                "request_id": request_id,
                "message_id": message_id,
                "is_new_turn": is_new_turn,
            });
            if let Some(t) = thread_summary_applied {
                v["thread_summary_applied"] = json!({ "token_estimate": t });
            }
            ("stream_started", v)
        }
        StreamEvent::Ping => ("ping", json!({})),
        StreamEvent::Delta { kind, content } => (
            "delta",
            json!({
                "type": match kind { DeltaKind::Text => "text", DeltaKind::Reasoning => "reasoning" },
                "content": content,
            }),
        ),
        StreamEvent::Tool { phase, name, details } => {
            ("tool", json!({ "phase": phase, "name": name, "details": details }))
        }
        StreamEvent::Citations(items) => {
            let items: Vec<Value> = items
                .iter()
                .map(|c| {
                    let mut v = json!({ "source": c.source, "title": c.title, "snippet": c.snippet });
                    if let Some(u) = &c.url {
                        v["url"] = json!(u);
                    }
                    if let Some(a) = &c.attachment_id {
                        v["attachment_id"] = json!(a);
                    }
                    if let Some((s, e)) = c.span {
                        v["span"] = json!({ "start": s, "end": e });
                    }
                    v
                })
                .collect();
            ("citations", json!({ "items": items }))
        }
        StreamEvent::Done(d) => {
            let mut v = json!({
                "usage": { "input_tokens": d.input_tokens, "output_tokens": d.output_tokens },
                "effective_model": d.effective_model,
                "selected_model": d.selected_model,
                "quota_decision": if d.downgraded { "downgrade" } else { "allow" },
            });
            if let Some(f) = &d.downgrade_from {
                v["downgrade_from"] = json!(f);
            }
            if let Some(r) = &d.downgrade_reason {
                v["downgrade_reason"] = json!(r);
            }
            if let Some(w) = &d.quota_warnings {
                let items: Vec<Value> = w
                    .iter()
                    .map(|q| {
                        let mut e = json!({
                            "tier": q.tier,
                            "period": q.period,
                            "remaining_percentage": q.remaining_percentage,
                            "warning": q.warning,
                            "exhausted": q.exhausted,
                        });
                        if let Some(n) = q.next_reset {
                            e["next_reset"] = json!(rfc3339(n));
                        }
                        e
                    })
                    .collect();
                v["quota_warnings"] = Value::Array(items);
            }
            ("done", v)
        }
        StreamEvent::Error { code, message } => ("error", json!({ "code": code, "message": message })),
    }
}

fn to_sse(ev: &StreamEvent) -> Event {
    let (name, data) = event_payload(ev);
    Event::default().event(name).data(data.to_string())
}

struct LiveState {
    first: Option<StreamEvent>,
    rx: mpsc::Receiver<StreamEvent>,
    _guard: DropGuard,
    ping: Duration,
    content_started: bool,
    finished: bool,
}

fn live_stream(ls: crate::domain::stream::LiveStream) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    let st = LiveState {
        first: Some(ls.started),
        rx: ls.rx,
        _guard: ls.cancel_guard,
        ping: ls.ping_interval,
        content_started: false,
        finished: false,
    };
    futures::stream::unfold(st, |mut st| async move {
        if st.finished {
            return None;
        }
        if let Some(ev) = st.first.take() {
            return Some((Ok::<_, Infallible>(to_sse(&ev)), st));
        }
        let next = if st.content_started {
            st.rx.recv().await
        } else {
            match tokio::time::timeout(st.ping, st.rx.recv()).await {
                Err(_) => return Some((Ok(to_sse(&StreamEvent::Ping)), st)),
                Ok(v) => v,
            }
        };
        let ev = if let Some(ev) = next {
            ev
        } else {
            st.finished = true;
            StreamEvent::Error {
                code: "stream_interrupted".into(),
                message: "The response stream was interrupted".into(),
            }
        };
        if ev.is_content() {
            st.content_started = true;
        }
        if ev.is_terminal() {
            st.finished = true;
        }
        Some((Ok(to_sse(&ev)), st))
    })
}

/// SSE HTTP response for a stream start.
#[must_use]
pub fn sse_response(start: StreamStart) -> Response {
    match start {
        StreamStart::Replay(events) => {
            let s = futures::stream::iter(events.into_iter().map(|e| Ok::<_, Infallible>(to_sse(&e))));
            Sse::new(s)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
                .into_response()
        }
        StreamStart::Live(ls) => Sse::new(live_stream(ls))
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(30)))
            .into_response(),
    }
}
