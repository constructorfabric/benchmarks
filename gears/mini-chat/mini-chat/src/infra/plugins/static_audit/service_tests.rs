#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the serving part of
//! `cpt-cf-mini-chat-algo-gear-foundation-static-audit-plugin`
//! (steps `inst-if-audit-enabled` .. `inst-return-audit-ok`).

use std::fmt::Debug;
use std::sync::{Arc, Mutex};

use mini_chat_sdk::{
    AuditPolicyDecisions, AuditQuotaDecision, AuditToolCalls, AuditUsage, MiniChatAuditEvent,
    MiniChatAuditPluginClientV1, TurnAuditEvent, TurnAuditEventType, TurnMutationAuditEvent,
};
use time::OffsetDateTime;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use uuid::Uuid;

use super::{StaticAuditConfig, StaticAuditService};

fn service(enabled: bool) -> StaticAuditService {
    StaticAuditService::new(StaticAuditConfig {
        enabled,
        vendor: "constructorfabric".to_owned(),
        priority: 100,
    })
}

fn event() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Mutation(TurnMutationAuditEvent::TurnDelete {
        tenant_id: Uuid::from_u128(0x7E),
        actor_user_id: Uuid::from_u128(0x55),
        chat_id: Uuid::from_u128(0xC1),
        request_id: Uuid::from_u128(0x91),
        timestamp: OffsetDateTime::UNIX_EPOCH,
    })
}

#[tokio::test]
async fn emit_succeeds_when_enabled() {
    let result = service(true).emit(event()).await;

    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn emit_succeeds_when_disabled() {
    let result = service(false).emit(event()).await;

    assert!(result.is_ok(), "{result:?}");
}

// ---- logged record ---------------------------------------------------------------

const PROMPT_SENTINEL: &str = "PROMPT-SENTINEL-do-not-log";
const RESPONSE_SENTINEL: &str = "RESPONSE-SENTINEL-do-not-log";
const ATTACHMENT_SENTINEL: &str = "ATTACHMENT-SENTINEL-do-not-log";

/// One captured tracing event rendered as `name=value` pairs.
#[derive(Debug, Clone)]
struct Captured {
    target: String,
    fields: Vec<(String, String)>,
}

impl Captured {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn rendered(&self) -> String {
        format!("{} {:?}", self.target, self.fields)
    }
}

struct FieldCollector(Vec<(String, String)>);

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }
}

/// Minimal subscriber that records every event, without extra crates.
#[derive(Clone, Default)]
struct CaptureSubscriber {
    events: Arc<Mutex<Vec<Captured>>>,
}

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut collector = FieldCollector(Vec::new());
        event.record(&mut collector);
        self.events.lock().unwrap().push(Captured {
            target: event.metadata().target().to_owned(),
            fields: collector.0,
        });
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

async fn capture(enabled: bool, event: MiniChatAuditEvent) -> Vec<Captured> {
    let subscriber = CaptureSubscriber::default();
    let events = Arc::clone(&subscriber.events);
    let service = service(enabled);
    let guard = tracing::subscriber::set_default(subscriber);
    service.emit(event).await.expect("emit");
    drop(guard);
    events.lock().unwrap().clone()
}

fn turn_event_with_content() -> MiniChatAuditEvent {
    MiniChatAuditEvent::Turn(TurnAuditEvent {
        event_type: TurnAuditEventType::TurnCompleted,
        tenant_id: Uuid::from_u128(0x7E),
        requester_user_id: Uuid::from_u128(0x55),
        chat_id: Uuid::from_u128(0xC1),
        turn_id: Uuid::from_u128(0xD1),
        request_id: Uuid::from_u128(0x91),
        selected_model: "gpt-5.2".to_owned(),
        effective_model: "gpt-5.2-mini".to_owned(),
        usage: AuditUsage {
            input_tokens: 10,
            output_tokens: 20,
        },
        latency_ms: 1500,
        tool_calls: AuditToolCalls {
            web_search: 1,
            file_search: 2,
        },
        policy_decisions: AuditPolicyDecisions {
            quota: AuditQuotaDecision {
                decision: "unknown".to_owned(),
            },
        },
        timestamp: OffsetDateTime::UNIX_EPOCH,
        prompt: PROMPT_SENTINEL.to_owned(),
        response: RESPONSE_SENTINEL.to_owned(),
        attachments: vec![serde_json::json!({ "name": ATTACHMENT_SENTINEL })],
        license: Some(serde_json::json!({ "key": ATTACHMENT_SENTINEL })),
        quota_scope: Some(serde_json::json!({ "scope": ATTACHMENT_SENTINEL })),
    })
}

#[tokio::test]
async fn enabled_emits_one_metadata_record_for_a_turn_event() {
    let records = capture(true, turn_event_with_content()).await;

    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.target, "mini_chat::audit");
    assert_eq!(record.field("event_type"), Some("turn_completed"));
    assert_eq!(
        record.field("tenant_id"),
        Some(Uuid::from_u128(0x7E).to_string().as_str())
    );
    assert_eq!(
        record.field("chat_id"),
        Some(Uuid::from_u128(0xC1).to_string().as_str())
    );
    assert_eq!(
        record.field("user_id"),
        Some(Uuid::from_u128(0x55).to_string().as_str())
    );
    assert_eq!(
        record.field("request_id"),
        Some(Uuid::from_u128(0x91).to_string().as_str())
    );
    assert_eq!(record.field("selected_model"), Some("Some(\"gpt-5.2\")"));
    assert_eq!(
        record.field("effective_model"),
        Some("Some(\"gpt-5.2-mini\")")
    );
    assert_eq!(record.field("latency_ms"), Some("Some(1500)"));
    assert_eq!(record.field("input_tokens"), Some("Some(10)"));
    assert_eq!(record.field("output_tokens"), Some("Some(20)"));
}

#[tokio::test]
async fn enabled_emits_one_record_for_a_mutation_event() {
    let records = capture(true, event()).await;

    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].field("event_type"), Some("turn_delete"));
    assert_eq!(
        records[0].field("user_id"),
        Some(Uuid::from_u128(0x55).to_string().as_str())
    );
}

#[tokio::test]
async fn disabled_emits_no_record() {
    let records = capture(false, turn_event_with_content()).await;

    assert!(records.is_empty(), "{records:?}");
}

#[tokio::test]
async fn content_never_appears_in_the_logged_record() {
    let records = capture(true, turn_event_with_content()).await;

    assert_eq!(records.len(), 1);
    let rendered = records[0].rendered();
    for sentinel in [PROMPT_SENTINEL, RESPONSE_SENTINEL, ATTACHMENT_SENTINEL] {
        assert!(
            !rendered.contains(sentinel),
            "{sentinel} leaked: {rendered}"
        );
    }
    for name in [
        "prompt",
        "response",
        "attachments",
        "license",
        "quota_scope",
    ] {
        assert!(
            records[0].field(name).is_none(),
            "`{name}` must not be logged"
        );
    }
}
