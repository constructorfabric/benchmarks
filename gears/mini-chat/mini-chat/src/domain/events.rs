//! Stable SSE event contract (DESIGN §3.3 "SSE Event Definitions").

use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::domain::quota::PeriodStatus;

#[derive(Debug, Clone, PartialEq)]
pub struct CitationView {
    pub source: &'static str,
    pub title: String,
    pub url: Option<String>,
    pub attachment_id: Option<Uuid>,
    pub snippet: String,
    pub span: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DoneView {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub effective_model: String,
    pub selected_model: String,
    pub downgraded: bool,
    pub downgrade_reason: Option<String>,
    pub quota_warnings: Option<Vec<PeriodStatus>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    StreamStarted {
        request_id: Uuid,
        message_id: Uuid,
        is_new_turn: bool,
        thread_summary_tokens: Option<i64>,
    },
    Ping,
    Delta {
        kind: &'static str,
        content: String,
    },
    Tool {
        phase: &'static str,
        name: String,
        details: Value,
    },
    Citations(Vec<CitationView>),
    Done(DoneView),
    Error {
        code: String,
        message: String,
    },
}

fn ts(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl StreamEvent {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::StreamStarted { .. } => "stream_started",
            Self::Ping => "ping",
            Self::Delta { .. } => "delta",
            Self::Tool { .. } => "tool",
            Self::Citations(_) => "citations",
            Self::Done(_) => "done",
            Self::Error { .. } => "error",
        }
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Error { .. })
    }

    #[must_use]
    pub fn data(&self) -> Value {
        match self {
            Self::StreamStarted {
                request_id,
                message_id,
                is_new_turn,
                thread_summary_tokens,
            } => {
                let mut m = Map::new();
                m.insert("request_id".into(), json!(request_id));
                m.insert("message_id".into(), json!(message_id));
                m.insert("is_new_turn".into(), json!(is_new_turn));
                if let Some(t) = thread_summary_tokens {
                    m.insert("thread_summary_applied".into(), json!({"token_estimate": t}));
                }
                Value::Object(m)
            }
            Self::Ping => json!({}),
            Self::Delta { kind, content } => json!({"type": kind, "content": content}),
            Self::Tool { phase, name, details } => json!({"phase": phase, "name": name, "details": details}),
            Self::Citations(items) => {
                let items: Vec<Value> = items
                    .iter()
                    .map(|c| {
                        let mut m = Map::new();
                        m.insert("source".into(), json!(c.source));
                        m.insert("title".into(), json!(c.title));
                        if let Some(u) = &c.url {
                            m.insert("url".into(), json!(u));
                        }
                        if let Some(a) = c.attachment_id {
                            m.insert("attachment_id".into(), json!(a));
                        }
                        m.insert("snippet".into(), json!(c.snippet));
                        if let Some((s, e)) = c.span {
                            m.insert("span".into(), json!({"start": s, "end": e}));
                        }
                        Value::Object(m)
                    })
                    .collect();
                json!({"items": items})
            }
            Self::Done(d) => {
                let mut m = Map::new();
                m.insert(
                    "usage".into(),
                    json!({"input_tokens": d.input_tokens, "output_tokens": d.output_tokens}),
                );
                m.insert("effective_model".into(), json!(d.effective_model));
                m.insert("selected_model".into(), json!(d.selected_model));
                m.insert(
                    "quota_decision".into(),
                    json!(if d.downgraded { "downgrade" } else { "allow" }),
                );
                if d.downgraded {
                    m.insert("downgrade_from".into(), json!(d.selected_model));
                    if let Some(r) = &d.downgrade_reason {
                        m.insert("downgrade_reason".into(), json!(r));
                    }
                }
                if let Some(w) = &d.quota_warnings {
                    let arr: Vec<Value> = w
                        .iter()
                        .map(|p| {
                            let mut e = Map::new();
                            e.insert("tier".into(), json!(p.tier));
                            e.insert("period".into(), json!(p.period));
                            e.insert("remaining_percentage".into(), json!(p.remaining_percentage));
                            e.insert("warning".into(), json!(p.warning));
                            e.insert("exhausted".into(), json!(p.exhausted));
                            if p.warning || p.exhausted {
                                e.insert("next_reset".into(), json!(ts(p.next_reset)));
                            }
                            Value::Object(e)
                        })
                        .collect();
                    m.insert("quota_warnings".into(), Value::Array(arr));
                }
                Value::Object(m)
            }
            Self::Error { code, message } => json!({"code": code, "message": message}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_shape_allow() {
        let d = StreamEvent::Done(DoneView {
            input_tokens: 10,
            output_tokens: 5,
            effective_model: "m".into(),
            selected_model: "m".into(),
            downgraded: false,
            downgrade_reason: None,
            quota_warnings: None,
        });
        let v = d.data();
        assert_eq!(v["quota_decision"], "allow");
        assert!(v.get("downgrade_from").is_none());
        assert_eq!(v["usage"]["input_tokens"], 10);
        assert!(v.get("quota_warnings").is_none());
    }

    #[test]
    fn done_shape_downgrade() {
        let d = StreamEvent::Done(DoneView {
            input_tokens: 1,
            output_tokens: 1,
            effective_model: "s".into(),
            selected_model: "p".into(),
            downgraded: true,
            downgrade_reason: Some("premium_quota_exhausted".into()),
            quota_warnings: Some(vec![]),
        });
        let v = d.data();
        assert_eq!(v["downgrade_from"], "p");
        assert_eq!(v["downgrade_reason"], "premium_quota_exhausted");
    }

    #[test]
    fn started_shape() {
        let e = StreamEvent::StreamStarted {
            request_id: Uuid::nil(),
            message_id: Uuid::nil(),
            is_new_turn: true,
            thread_summary_tokens: Some(12),
        };
        let v = e.data();
        assert_eq!(v["is_new_turn"], true);
        assert_eq!(v["thread_summary_applied"]["token_estimate"], 12);
    }
}
