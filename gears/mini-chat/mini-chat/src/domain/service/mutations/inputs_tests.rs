#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Mutation input loading: the carried attachments of the replaced user
//! message, the attachment part of the turn inputs, `TOO_MANY_IMAGES`, and
//! the absence of any history read before the commit.

use toolkit_security::AccessScope;
use uuid::Uuid;

use super::load_mutation_inputs;
use crate::domain::error::DomainError;
use crate::domain::service::mutations::test_support::{
    FakeMutations, MutationCall, TARGET_USER_MESSAGE,
};
use crate::domain::service::stream::inputs::InputLimits;
use crate::domain::service::stream::test_support::{CHAT, FakeTurns, TurnsCall, doc, image, sheet};
use crate::test_doubles::USER_TENANT;

const LIMITS: InputLimits = InputLimits {
    recent_messages_limit: 10,
    max_images_per_message: 4,
};

fn scope() -> AccessScope {
    AccessScope::for_tenant(USER_TENANT)
}

#[tokio::test]
async fn the_links_of_the_old_user_message_are_read_and_become_the_request_attachments() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    let (document, picture) = (doc(1), image(1));
    mutations.with(|inner| inner.carried = vec![document.id, picture.id]);
    turns.with(|inner| {
        inner.request_attachments = vec![document.clone(), picture.clone()];
        inner.ready = vec![document.clone(), picture.clone()];
    });

    let loaded = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap();

    assert_eq!(
        mutations.calls(),
        vec![MutationCall::Carried(CHAT, TARGET_USER_MESSAGE)]
    );
    assert_eq!(loaded.carried_ids, vec![document.id, picture.id]);
    assert_eq!(
        turns.calls()[0],
        TurnsCall::RequestAttachments(vec![document.id, picture.id])
    );
    assert_eq!(loaded.inputs.image_count, 1);
    assert_eq!(loaded.inputs.images.len(), 1);
    assert_eq!(loaded.inputs.images[0].id, picture.id);
    assert_eq!(
        loaded.inputs.images[0].provider_file_id.as_deref(),
        Some("file-img1")
    );
}

#[tokio::test]
async fn the_ready_flags_the_vector_store_and_the_citation_map_are_loaded() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    let (document, spreadsheet) = (doc(1), sheet(1));
    turns.with(|inner| {
        inner.ready = vec![document.clone(), spreadsheet.clone()];
        inner.vector_store = Some("vs_chat".to_owned());
    });

    let loaded = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap();

    assert!(loaded.inputs.has_ready_file_search_documents);
    assert!(loaded.inputs.has_ready_code_interpreter_files);
    assert_eq!(loaded.inputs.code_interpreter_file_ids, vec!["file-sheet1"]);
    assert_eq!(loaded.inputs.vector_store_id.as_deref(), Some("vs_chat"));
    assert!(loaded.inputs.citation_map.contains_key("file-doc1"));
}

#[tokio::test]
async fn without_links_there_is_no_attachment_lookup_and_no_image() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();

    let loaded = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap();

    assert!(loaded.carried_ids.is_empty());
    assert_eq!(loaded.inputs.image_count, 0);
    assert_eq!(
        turns.count(|call| matches!(call, TurnsCall::RequestAttachments(_))),
        0
    );
}

#[tokio::test]
async fn no_history_step_runs_before_the_commit() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    turns.with(|inner| {
        inner.summary = Some(crate::domain::service::stream::ports::ThreadSummaryInfo {
            summary_text: "s".to_owned(),
            frontier: crate::domain::service::stream::ports::MessageBoundary {
                created_at: crate::domain::service::stream::test_support::at(-5),
                id: Uuid::from_u128(1),
            },
            token_estimate: 3,
        });
    });

    let loaded = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap();

    assert_eq!(
        turns.count(|call| matches!(
            call,
            TurnsCall::Boundary | TurnsCall::Summary | TurnsCall::Recent(_)
        )),
        0
    );
    assert_eq!(loaded.inputs.boundary, None);
    assert_eq!(loaded.inputs.summary, None);
    assert!(loaded.inputs.recent.is_empty());
}

#[tokio::test]
async fn more_images_than_the_limit_are_too_many_images() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    let images: Vec<_> = (1..=5).map(image).collect();
    mutations.with(|inner| inner.carried = images.iter().map(|row| row.id).collect());
    turns.with(|inner| inner.request_attachments = images.clone());

    let error = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, DomainError::TooManyImages), "{error:?}");
}

#[tokio::test]
async fn the_images_at_the_limit_are_accepted() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    let images: Vec<_> = (1..=4).map(image).collect();
    mutations.with(|inner| inner.carried = images.iter().map(|row| row.id).collect());
    turns.with(|inner| inner.request_attachments = images.clone());

    let loaded = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap();

    assert_eq!(loaded.inputs.image_count, 4);
}

#[tokio::test]
async fn a_failed_link_read_is_a_database_error_and_nothing_else_is_read() {
    let mutations = FakeMutations::new();
    let turns = FakeTurns::new();
    mutations.with(|inner| inner.read_error = Some("down".to_owned()));

    let error = load_mutation_inputs(
        &*turns,
        &*mutations,
        &scope(),
        CHAT,
        TARGET_USER_MESSAGE,
        &LIMITS,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
    assert!(turns.calls().is_empty());
}
