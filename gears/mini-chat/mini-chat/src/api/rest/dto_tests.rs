#![allow(clippy::unwrap_used)]

use serde_json::json;
use time::macros::datetime;
use uuid::Uuid;

use super::*;
use crate::domain::service::chats::ChatView;
use crate::domain::service::stream::{DoneInfo, QuotaWarning as W, StreamEvent};
use crate::infra::storage::entity::{chat, message};

fn chat_model(title: Option<&str>) -> chat::Model {
    chat::Model {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        user_id: Uuid::nil(),
        model: "m".into(),
        title: title.map(str::to_owned),
        is_temporary: false,
        created_at: datetime!(2025-06-15 10:30:00.123456001 UTC),
        updated_at: datetime!(2025-06-15 10:30:00 UTC),
        deleted_at: None,
    }
}

#[test]
fn chat_detail_omits_null_title_and_hides_identity() {
    let v = serde_json::to_value(ChatDetailDto::from(ChatView {
        chat: chat_model(None),
        message_count: 3,
    }))
    .unwrap();
    assert!(v.get("title").is_none());
    assert!(v.get("user_id").is_none() && v.get("tenant_id").is_none());
    assert_eq!(v["message_count"], 3);
    assert_eq!(v["created_at"], "2025-06-15T10:30:00.123456Z");
    assert_eq!(v["updated_at"], "2025-06-15T10:30:00.000000Z");
    let v = serde_json::to_value(ChatDetailDto::from(ChatView {
        chat: chat_model(Some("t")),
        message_count: 0,
    }))
    .unwrap();
    assert_eq!(v["title"], "t");
}

fn msg(role: &str, tokens: i64) -> message::Model {
    message::Model {
        id: Uuid::nil(),
        tenant_id: Uuid::nil(),
        chat_id: Uuid::nil(),
        request_id: Some(Uuid::nil()),
        role: role.into(),
        content: "c".into(),
        content_type: "text".into(),
        token_estimate: 0,
        provider_response_id: Some("resp_secret".into()),
        request_kind: "chat".into(),
        features_used: json!([]),
        input_tokens: tokens,
        output_tokens: tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
        model: Some("m".into()),
        is_compressed: false,
        created_at: datetime!(2025-06-15 10:30:00 UTC),
        deleted_at: None,
    }
}

#[test]
fn message_contract() {
    let view = |role, tokens, reaction: Option<&str>| MessageView {
        message: msg(role, tokens),
        request_id: Uuid::nil(),
        attachments: Vec::new(),
        my_reaction: reaction.map(str::to_owned),
    };
    let v = serde_json::to_value(MiniChatMessageDto::from(view("user", 0, None))).unwrap();
    assert_eq!(v["my_reaction"], serde_json::Value::Null);
    assert_eq!(v["attachments"], json!([]));
    assert!(v.get("model").is_none() && v.get("input_tokens").is_none());
    assert!(!v.to_string().contains("resp_secret"));
    let v =
        serde_json::to_value(MiniChatMessageDto::from(view("assistant", 5, Some("like")))).unwrap();
    assert_eq!(v["my_reaction"], "like");
    assert_eq!(v["model"], "m");
    assert_eq!(v["input_tokens"], 5);
    let v = serde_json::to_value(MiniChatMessageDto::from(view("assistant", 0, None))).unwrap();
    assert!(v.get("input_tokens").is_none() && v.get("output_tokens").is_none());
}

#[test]
fn sse_events_encode_contract() {
    let ev = |e: StreamEvent| {
        // `Event` has no public accessors; check the wire text via Debug.
        format!("{:?}", crate::api::rest::sse::to_sse(e))
    };
    let s = ev(StreamEvent::Started {
        request_id: Uuid::nil(),
        message_id: Uuid::nil(),
        is_new_turn: true,
        thread_summary_applied: None,
    });
    assert!(s.contains("stream_started") && !s.contains("thread_summary_applied"));
    let s = ev(StreamEvent::Delta {
        reasoning: true,
        content: "x".into(),
    });
    assert!(s.contains("reasoning"));
    let s = ev(StreamEvent::Done(DoneInfo {
        input_tokens: 1,
        output_tokens: 2,
        effective_model: "a".into(),
        selected_model: "b".into(),
        downgrade: true,
        downgrade_from: Some("b".into()),
        downgrade_reason: Some("premium_quota_exhausted".into()),
        quota_warnings: Some(vec![W {
            tier: "total",
            period: "daily",
            remaining_percentage: 10,
            warning: true,
            exhausted: false,
            next_reset: Some(datetime!(2025-06-16 00:00:00 UTC)),
        }]),
    }));
    assert!(
        s.contains("downgrade")
            && s.contains("premium_quota_exhausted")
            && s.contains("next_reset")
    );
}

#[test]
fn done_data_shapes() {
    let v = serde_json::to_value(DoneData::from(DoneInfo {
        input_tokens: 1,
        output_tokens: 2,
        effective_model: "a".into(),
        selected_model: "a".into(),
        downgrade: false,
        downgrade_from: None,
        downgrade_reason: None,
        quota_warnings: None,
    }))
    .unwrap();
    assert_eq!(
        v,
        json!({"usage": {"input_tokens": 1, "output_tokens": 2}, "effective_model": "a",
               "selected_model": "a", "quota_decision": "allow"})
    );
    let v = serde_json::to_value(PingData {}).unwrap();
    assert_eq!(v, json!({}));
    let v = serde_json::to_value(DeltaData {
        kind: DeltaKind::Text,
        content: "x".into(),
    })
    .unwrap();
    assert_eq!(v, json!({"type": "text", "content": "x"}));
}

#[test]
fn citation_dto_omits_absent_fields() {
    let v = serde_json::to_value(Citation::from(crate::domain::service::stream::Citation {
        web: false,
        title: "a.pdf".into(),
        snippet: String::new(),
        url: None,
        attachment_id: Some(Uuid::nil()),
        span: None,
    }))
    .unwrap();
    assert_eq!(
        v,
        json!({"source": "file", "title": "a.pdf", "attachment_id": Uuid::nil(), "snippet": ""})
    );
}

#[test]
fn odata_rewrite_quoted_uuid() {
    use toolkit_odata::ast::{CompareOperator, Expr, Value};
    let id = Uuid::new_v4();
    let e = Expr::Compare(
        Box::new(Expr::Identifier("id".into())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String(id.to_string()))),
    );
    match crate::api::rest::odata::rewrite(e) {
        Expr::Compare(_, _, r) => assert!(matches!(*r, Expr::Value(Value::Uuid(u)) if u == id)),
        _ => panic!("unexpected"),
    }
    let e = Expr::Compare(
        Box::new(Expr::Identifier("title".into())),
        CompareOperator::Eq,
        Box::new(Expr::Value(Value::String(id.to_string()))),
    );
    match crate::api::rest::odata::rewrite(e) {
        Expr::Compare(_, _, r) => assert!(matches!(*r, Expr::Value(Value::String(_)))),
        _ => panic!("unexpected"),
    }
}
