//! Static audit plugin client tests: the event is logged while `enabled`, accepted silently
//! otherwise, and the plugin always reports success to the audit handler.
//!
//! Slice S2: algorithm `static-audit-plugin`, steps `inst-static-audit-enabled`,
//! `inst-static-audit-log`, `inst-static-audit-silent` and `inst-static-audit-return`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::MiniChatAuditPluginClientV1;
use tracing_test::traced_test;
use uuid::Uuid;

use super::{StaticAuditConfig, StaticAuditService};
use crate::test_support::s2::all_audit_events;

fn service(enabled: bool) -> StaticAuditService {
    StaticAuditService::new(&StaticAuditConfig {
        enabled,
        ..StaticAuditConfig::default()
    })
}

#[traced_test]
#[tokio::test]
async fn an_enabled_plugin_logs_every_event_type_and_accepts_it() {
    let service = service(true);
    for index in 0..5_u128 {
        // a distinct chat id per event type tells the log lines apart
        let chat_id = Uuid::from_u128(0xE0E0_0000 + index);
        let event = all_audit_events(chat_id)
            .into_iter()
            .nth(usize::try_from(index).unwrap())
            .unwrap();
        service.emit(event).await.expect("accepted");
        assert!(
            logs_contain(&chat_id.to_string()),
            "event type #{index} was not logged"
        );
    }
}

#[traced_test]
#[tokio::test]
async fn a_disabled_plugin_accepts_the_event_without_logging_it() {
    let service = service(false);
    let chat_id = Uuid::from_u128(0xD15A_B1ED);
    for event in all_audit_events(chat_id) {
        service.emit(event).await.expect("accepted silently");
    }
    assert!(!logs_contain(&chat_id.to_string()));
}
