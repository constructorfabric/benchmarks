use super::*;
use toolkit_db::outbox::OutboxError;
use toolkit_db::secure::ScopeError;

use crate::domain::credits::CreditError;

fn assert_send_sync_static<T: Send + Sync + 'static>() {}

#[test]
fn domain_error_is_send_sync_static() {
    assert_send_sync_static::<DomainError>();
}

#[test]
fn unique_violation_scope_error_maps_to_unique_violation() {
    let db = sea_orm::DbErr::Query(sea_orm::RuntimeErr::Internal(
        "UNIQUE constraint failed: chats.id".to_owned(),
    ));
    let e = DomainError::from(ScopeError::Db(db));
    assert!(matches!(e, DomainError::UniqueViolation), "{e:?}");
}

#[test]
fn other_scope_errors_are_internal() {
    let e = DomainError::from(ScopeError::Denied("nope"));
    assert!(matches!(e, DomainError::Internal(_)), "{e:?}");
    let db = sea_orm::DbErr::Custom("boom".to_owned());
    let e = DomainError::from(ScopeError::Db(db));
    assert!(matches!(e, DomainError::Internal(_)), "{e:?}");
}

#[test]
fn outbox_payload_too_large_maps_without_chat_delete_flag() {
    let e = DomainError::from(OutboxError::PayloadTooLarge { size: 10, max: 5 });
    assert!(
        matches!(
            e,
            DomainError::OutboxPayloadTooLarge {
                during_chat_delete: false
            }
        ),
        "{e:?}"
    );
}

#[test]
fn other_outbox_errors_are_internal() {
    let e = DomainError::from(OutboxError::QueueNotRegistered("q".to_owned()));
    assert!(matches!(e, DomainError::Internal(_)), "{e:?}");
}

#[test]
fn db_error_is_internal() {
    let e = DomainError::from(toolkit_db::DbError::UnknownDsn("x".to_owned()));
    assert!(matches!(e, DomainError::Internal(_)), "{e:?}");
}

#[test]
fn credit_error_is_internal() {
    let e = DomainError::from(CreditError::ZeroMultiplier);
    assert!(matches!(e, DomainError::Internal(_)), "{e:?}");
}

#[test]
fn quota_scope_names() {
    assert_eq!(QuotaScope::Tokens.as_str(), "tokens");
    assert_eq!(QuotaScope::WebSearch.as_str(), "web_search");
    assert_eq!(QuotaScope::CodeInterpreter.as_str(), "code_interpreter");
}

#[test]
fn provider_resolution_is_an_sse_provider_error() {
    assert_eq!(
        DomainError::ProviderResolution("x".to_owned()).sse_code(),
        "provider_error"
    );
}

#[test]
fn sse_codes_are_always_documented_streaming_codes() {
    const DOCUMENTED: &[&str] = &[
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
        "finalization_failed",
        "stream_interrupted",
    ];
    for e in [
        DomainError::Internal("x".to_owned()),
        DomainError::PolicySnapshotGone("x".to_owned()),
        DomainError::UniqueViolation,
        DomainError::AuthzUnavailable,
        DomainError::OutboxPayloadTooLarge {
            during_chat_delete: false,
        },
        DomainError::StorageUnavailable,
        DomainError::Replay,
    ] {
        assert!(DOCUMENTED.contains(&e.sse_code()), "{e:?}");
    }
}

#[test]
fn display_never_contains_provider_detail_for_pre_stream_variants() {
    let e = DomainError::NotFound {
        resource: ResourceKind::Chat,
    };
    assert_eq!(e.to_string(), "chat not found");
}
