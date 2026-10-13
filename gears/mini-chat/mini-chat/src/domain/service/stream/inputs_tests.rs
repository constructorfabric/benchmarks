#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: turn input loading
//! (`cpt-cf-mini-chat-algo-streaming-turns-load-turn-inputs`).

use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::ports::{
    HistoryRole, MessageBoundary, RecentQuery, ThreadSummaryInfo,
};
use crate::domain::service::stream::test_support::{
    CHAT, FakeTurns, TurnsCall, at, doc, history, image, sheet,
};
use crate::test_doubles::USER_TENANT;

const LIMITS: InputLimits = InputLimits {
    recent_messages_limit: 10,
    max_images_per_message: 4,
};

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT)
}

async fn load(turns: &FakeTurns, ids: &[Uuid]) -> Result<TurnInputs, DomainError> {
    load_turn_inputs(turns, &scope(), CHAT, ids, &LIMITS).await
}

fn boundary(secs: i64, n: u128) -> MessageBoundary {
    MessageBoundary {
        created_at: at(secs),
        id: Uuid::from_u128(n),
    }
}

#[tokio::test]
async fn without_attachment_ids_the_request_attachments_are_not_read() {
    let turns = FakeTurns::new();
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(inputs.image_count, 0);
    assert!(inputs.images.is_empty());
    assert_eq!(
        turns.count(|c| matches!(c, TurnsCall::RequestAttachments(_))),
        0
    );
}

#[tokio::test]
async fn only_image_rows_of_the_request_count_as_images() {
    let turns = FakeTurns::new();
    let (img, document) = (image(1), doc(1));
    turns.with(|t| t.request_attachments = vec![img.clone(), document.clone()]);
    // The third id is unknown, foreign or deleted: no row, so no image.
    let ids = [img.id, document.id, Uuid::from_u128(0xDEAD)];
    let inputs = load(&turns, &ids).await.unwrap();
    assert_eq!(inputs.image_count, 1);
    assert_eq!(
        inputs.images,
        vec![ImageAttachment {
            id: img.id,
            provider_file_id: img.provider_file_id.clone(),
            secondary_file_id: img.secondary_file_id.clone(),
        }]
    );
}

#[tokio::test]
async fn more_images_than_allowed_is_too_many_images_before_the_other_reads() {
    let turns = FakeTurns::new();
    let rows: Vec<_> = (1..=5).map(image).collect();
    let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    turns.with(|t| t.request_attachments = rows);
    let error = load(&turns, &ids).await.unwrap_err();
    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
    assert_eq!(
        turns.calls(),
        vec![TurnsCall::RequestAttachments(ids)],
        "no further read after the image check"
    );
}

#[tokio::test]
async fn exactly_the_maximum_number_of_images_passes() {
    let turns = FakeTurns::new();
    let rows: Vec<_> = (1..=4).map(image).collect();
    let ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    turns.with(|t| t.request_attachments = rows);
    assert_eq!(load(&turns, &ids).await.unwrap().image_count, 4);
}

#[tokio::test]
async fn the_ready_flags_come_from_the_ready_attachments() {
    let turns = FakeTurns::new();
    turns.with(|t| t.ready = vec![doc(1), sheet(1)]);
    let inputs = load(&turns, &[]).await.unwrap();
    assert!(inputs.has_ready_file_search_documents);
    assert!(inputs.has_ready_code_interpreter_files);
    assert_eq!(inputs.code_interpreter_file_ids, vec!["file-sheet1"]);

    let turns = FakeTurns::new();
    turns.with(|t| t.ready = vec![image(1)]);
    let inputs = load(&turns, &[]).await.unwrap();
    assert!(!inputs.has_ready_file_search_documents);
    assert!(!inputs.has_ready_code_interpreter_files);
}

#[tokio::test]
async fn a_code_interpreter_file_without_a_provider_file_id_does_not_count() {
    let turns = FakeTurns::new();
    let mut pending_copy = sheet(1);
    pending_copy.provider_file_id = None;
    turns.with(|t| t.ready = vec![pending_copy]);
    let inputs = load(&turns, &[]).await.unwrap();
    assert!(!inputs.has_ready_code_interpreter_files);
    assert!(inputs.code_interpreter_file_ids.is_empty());
}

#[tokio::test]
async fn code_interpreter_file_ids_follow_created_at_then_id() {
    let turns = FakeTurns::new();
    let (mut a, mut b, mut c) = (sheet(1), sheet(2), sheet(3));
    // b is the oldest; a and c share a time and are ordered by id.
    b.created_at = at(-300);
    a.created_at = at(-200);
    c.created_at = at(-200);
    // The repository does not promise an order.
    turns.with(|t| t.ready = vec![c.clone(), a.clone(), b.clone()]);
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(
        inputs.code_interpreter_file_ids,
        vec!["file-sheet2", "file-sheet1", "file-sheet3"]
    );
}

#[tokio::test]
async fn the_citation_map_has_every_ready_attachment_with_a_provider_file_id() {
    let turns = FakeTurns::new();
    let mut without_file = doc(3);
    without_file.provider_file_id = None;
    let (d1, i1) = (doc(1), image(1));
    turns.with(|t| t.ready = vec![d1.clone(), i1.clone(), without_file]);
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(inputs.citation_map.len(), 2);
    let target = &inputs.citation_map["file-doc1"];
    assert_eq!(target.attachment_id, d1.id);
    assert_eq!(target.filename, "doc1.pdf");
    assert_eq!(inputs.citation_map["file-img1"].attachment_id, i1.id);
}

#[tokio::test]
async fn the_vector_store_id_is_passed_through() {
    let turns = FakeTurns::new();
    turns.with(|t| t.vector_store = Some("vs_abc".to_owned()));
    assert_eq!(
        load(&turns, &[]).await.unwrap().vector_store_id.as_deref(),
        Some("vs_abc")
    );
    let turns = FakeTurns::new();
    assert_eq!(load(&turns, &[]).await.unwrap().vector_store_id, None);
}

#[tokio::test]
async fn without_a_boundary_no_recent_messages_are_read() {
    let turns = FakeTurns::new();
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(inputs.boundary, None);
    assert!(inputs.recent.is_empty());
    assert_eq!(turns.count(|c| matches!(c, TurnsCall::Recent(_))), 0);
}

#[tokio::test]
async fn a_zero_recent_limit_reads_no_recent_messages() {
    let turns = FakeTurns::new();
    turns.with(|t| t.boundary = Some(boundary(-10, 5)));
    let limits = InputLimits {
        recent_messages_limit: 0,
        ..LIMITS
    };
    let inputs = load_turn_inputs(&*turns, &scope(), CHAT, &[], &limits)
        .await
        .unwrap();
    assert!(inputs.recent.is_empty());
    assert_eq!(turns.count(|c| matches!(c, TurnsCall::Recent(_))), 0);
}

#[tokio::test]
async fn the_recent_query_is_bounded_by_the_boundary_and_the_limit() {
    let turns = FakeTurns::new();
    let b = boundary(-10, 5);
    turns.with(|t| {
        t.boundary = Some(b);
        t.recent = vec![
            history(1, HistoryRole::User, "q1", 1),
            history(2, HistoryRole::Assistant, "a1", 2),
        ];
    });
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(inputs.boundary, Some(b));
    assert_eq!(
        turns.calls().last(),
        Some(&TurnsCall::Recent(RecentQuery {
            boundary: b,
            frontier: None,
            limit: 10,
        }))
    );
    // Chronological order is kept as the repository returned it.
    let contents: Vec<_> = inputs.recent.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(contents, ["q1", "a1"]);
}

#[tokio::test]
async fn with_a_summary_the_recent_query_starts_after_its_frontier() {
    let turns = FakeTurns::new();
    let b = boundary(-10, 5);
    let frontier = boundary(-500, 2);
    let summary = ThreadSummaryInfo {
        summary_text: "earlier".to_owned(),
        frontier,
        token_estimate: 17,
    };
    turns.with(|t| {
        t.boundary = Some(b);
        t.summary = Some(summary.clone());
    });
    let inputs = load(&turns, &[]).await.unwrap();
    assert_eq!(inputs.summary, Some(summary));
    assert_eq!(
        turns.calls().last(),
        Some(&TurnsCall::Recent(RecentQuery {
            boundary: b,
            frontier: Some(frontier),
            limit: 10,
        }))
    );
}

#[tokio::test]
async fn the_reads_run_in_the_documented_order() {
    let turns = FakeTurns::new();
    turns.with(|t| t.boundary = Some(boundary(-10, 5)));
    let id = image(1).id;
    turns.with(|t| t.request_attachments = vec![image(1)]);
    load(&turns, &[id]).await.unwrap();
    let kinds: Vec<&str> = turns
        .calls()
        .iter()
        .map(|call| match call {
            TurnsCall::RequestAttachments(_) => "request_attachments",
            TurnsCall::ReadyAttachments => "ready",
            TurnsCall::VectorStore => "vector_store",
            TurnsCall::Boundary => "boundary",
            TurnsCall::Summary => "summary",
            TurnsCall::Recent(_) => "recent",
            other => panic!("unexpected call {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "request_attachments",
            "ready",
            "vector_store",
            "boundary",
            "summary",
            "recent"
        ]
    );
}

#[tokio::test]
async fn every_read_is_scoped_to_the_callers_scope() {
    let turns = FakeTurns::new();
    turns.with(|t| t.boundary = Some(boundary(-10, 5)));
    load(&turns, &[]).await.unwrap();
    let scopes = turns.with(|t| t.scopes.clone());
    assert!(!scopes.is_empty());
    assert!(scopes.iter().all(|used| *used == scope()));
}

#[tokio::test]
async fn a_repository_error_stops_the_loading() {
    let turns = FakeTurns::new();
    turns.with(|t| t.read_error = Some("db down".to_owned()));
    assert!(matches!(
        load(&turns, &[]).await,
        Err(DomainError::Database(_))
    ));
}
