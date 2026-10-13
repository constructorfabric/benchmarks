#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Replacement history loading: the boundary excludes the new request, the
//! summary and the recent messages are read with it, and every read runs after
//! the replacement commit.

use toolkit_security::AccessScope;
use uuid::Uuid;

use super::load_replacement_history;
use crate::domain::error::DomainError;
use crate::domain::service::mutations::test_support::{FakeMutations, MutationCall};
use crate::domain::service::stream::ports::{
    HistoryRole, MessageBoundary, RecentQuery, ThreadSummaryInfo,
};
use crate::domain::service::stream::test_support::{CHAT, FakeTurns, TurnsCall, at, history};
use crate::test_doubles::USER_TENANT;

const NEW_REQUEST: Uuid = Uuid::from_u128(0xAAAA);

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT)
}

fn committed_store() -> std::sync::Arc<FakeMutations> {
    let store = FakeMutations::new();
    store.with(|inner| inner.replaced = true);
    store
}

fn boundary() -> MessageBoundary {
    MessageBoundary {
        created_at: at(-100),
        id: Uuid::from_u128(0x77),
    }
}

#[tokio::test]
async fn the_boundary_is_asked_for_with_the_new_request_id() {
    let store = committed_store();
    let turns = FakeTurns::new();

    load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 10)
        .await
        .unwrap();

    assert_eq!(
        store.calls(),
        vec![MutationCall::Boundary(CHAT, NEW_REQUEST)]
    );
}

#[tokio::test]
async fn the_summary_and_the_recent_messages_are_read_with_that_boundary() {
    let store = committed_store();
    let turns = FakeTurns::new();
    let summary = ThreadSummaryInfo {
        summary_text: "earlier".to_owned(),
        frontier: MessageBoundary {
            created_at: at(-200),
            id: Uuid::from_u128(0x11),
        },
        token_estimate: 9,
    };
    let recent = vec![
        history(1, HistoryRole::User, "q", 1),
        history(2, HistoryRole::Assistant, "a", 2),
    ];
    store.with(|inner| inner.boundary = Some(boundary()));
    turns.with(|inner| {
        inner.summary = Some(summary.clone());
        inner.recent.clone_from(&recent);
    });

    let loaded = load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 7)
        .await
        .unwrap();

    assert_eq!(loaded.boundary, Some(boundary()));
    assert_eq!(loaded.summary, Some(summary.clone()));
    assert_eq!(loaded.recent, recent);
    assert!(turns.calls().contains(&TurnsCall::Recent(RecentQuery {
        boundary: boundary(),
        frontier: Some(summary.frontier),
        limit: 7,
    })));
}

#[tokio::test]
async fn without_a_summary_the_recent_query_has_no_frontier() {
    let store = committed_store();
    let turns = FakeTurns::new();
    store.with(|inner| inner.boundary = Some(boundary()));

    load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 10)
        .await
        .unwrap();

    assert!(turns.calls().contains(&TurnsCall::Recent(RecentQuery {
        boundary: boundary(),
        frontier: None,
        limit: 10,
    })));
}

#[tokio::test]
async fn when_only_the_new_message_is_live_there_is_no_boundary_and_no_recent_read() {
    let store = committed_store();
    let turns = FakeTurns::new();
    turns.with(|inner| inner.recent = vec![history(1, HistoryRole::User, "never", 1)]);

    let loaded = load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 10)
        .await
        .unwrap();

    assert_eq!(loaded.boundary, None);
    assert!(loaded.recent.is_empty());
    assert_eq!(turns.count(|call| matches!(call, TurnsCall::Recent(_))), 0);
}

#[tokio::test]
async fn a_zero_limit_reads_no_recent_messages() {
    let store = committed_store();
    let turns = FakeTurns::new();
    store.with(|inner| inner.boundary = Some(boundary()));
    turns.with(|inner| inner.recent = vec![history(1, HistoryRole::User, "never", 1)]);

    let loaded = load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 0)
        .await
        .unwrap();

    assert!(loaded.recent.is_empty());
    assert_eq!(turns.count(|call| matches!(call, TurnsCall::Recent(_))), 0);
}

#[tokio::test]
async fn a_failed_boundary_read_is_returned_and_nothing_else_is_read() {
    let store = committed_store();
    let turns = FakeTurns::new();
    store.with(|inner| inner.history_error = Some("down".to_owned()));

    let error = load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 10)
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
    assert!(turns.calls().is_empty());
}

#[tokio::test]
async fn a_failed_recent_read_is_returned() {
    let store = committed_store();
    let turns = FakeTurns::new();
    store.with(|inner| inner.boundary = Some(boundary()));
    turns.with(|inner| inner.read_error = Some("down".to_owned()));

    let error = load_replacement_history(&*turns, &*store, &scope(), CHAT, NEW_REQUEST, 10)
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
}
