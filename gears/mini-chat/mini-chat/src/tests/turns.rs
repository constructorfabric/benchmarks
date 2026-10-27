#![allow(clippy::wildcard_imports)]
//! Turn lifecycle and mutation tests: retry/edit/delete, turn status,
//! idempotency of mutations, attachment carry-forward.

use uuid::Uuid;

use super::*;
use crate::domain::stream_service::StreamSetup;
use crate::error::DomainError;

/// The latest turn of a chat.
async fn latest_turn(bed: &TestBed, chat_id: Uuid) -> crate::infra::storage::entity::chat_turns::Model {
    let conn = bed.ctx.db.conn().expect("conn");
    bed.ctx
        .repos
        .turns
        .latest(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.security.subject_tenant_id(), chat_id)
        .await
        .expect("latest")
        .expect("turn row")
}

#[tokio::test]
async fn turn_status_states() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let request_id = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(request_id)).await;

    let status = bed
        .services
        .turns
        .get_turn(&bed.bed.security, chat.id, request_id)
        .await
        .expect("status");
    assert_eq!(status.state, crate::api::rest::dto::TurnStatusState::Done);
    assert!(status.assistant_message_id.is_some());
    assert!(status.error_code.is_none());

    // Unknown turn -> 404.
    let err = bed
        .services
        .turns
        .get_turn(&bed.bed.security, chat.id, Uuid::new_v4())
        .await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));

    // A turn of another chat -> 404.
    let other = create_chat(&bed.bed, None, None).await;
    let err = bed
        .services
        .turns
        .get_turn(&bed.bed.security, other.id, request_id)
        .await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn retry_replaces_last_turn() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid1 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(rid1)).await;
    let turn1 = latest_turn(&bed.bed, chat.id).await;

    let setup = bed
        .services
        .turns
        .retry_turn(&bed.bed.security, chat.id, rid1)
        .await
        .expect("retry");
    let events = match setup {
        StreamSetup::Live { receiver, .. } => drain(receiver).await,
        StreamSetup::Replay(events) => events,
    };
    let started = started(&events);
    assert!(started.is_new_turn);
    assert_ne!(started.request_id, rid1, "retry gets a new request id");

    // The old turn is soft-deleted with replaced_by; the new turn completed.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let old = bed
        .bed
        .ctx
        .repos
        .turns
        .get_any(&conn, &toolkit_db::secure::AccessScope::allow_all(), bed.bed.security.subject_tenant_id(), chat.id, rid1)
        .await
        .expect("old")
        .expect("old row");
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(started.request_id));

    let new = latest_turn(&bed.bed, chat.id).await;
    assert_eq!(new.state, "completed");
    assert_eq!(new.request_id, started.request_id);

    // History shows one user + one assistant (the old ones are soft-deleted).
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].request_id, started.request_id);

    // The old request id replays no more: 409 conflict.
    let err = bed
        .services
        .stream
        .send_message(
            &bed.bed.security,
            chat.id,
            StreamMessageRequest {
                content: "hi".to_owned(),
                request_id: Some(rid1),
                attachment_ids: None,
                web_search: None,
            },
        )
        .await;
    assert!(matches!(err, Err(DomainError::RequestIdConflict)));

    // Each completed turn produced exactly one usage event.
    wait_until(|| bed.bed.policy.usage_events.lock().expect("lock").len() == 2, "usage events").await;
}

#[tokio::test]
async fn edit_replaces_content() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid1 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "original", Some(rid1)).await;

    let setup = bed
        .services
        .turns
        .edit_turn(&bed.bed.security, chat.id, rid1, "edited".to_owned())
        .await
        .expect("edit");
    let events = match setup {
        StreamSetup::Live { receiver, .. } => drain(receiver).await,
        StreamSetup::Replay(events) => events,
    };
    let started = started(&events);
    assert!(started.is_new_turn);

    // The provider saw the edited content.
    let reqs = bed.gateway.requests.lock().expect("lock");
    let last = reqs.last().expect("request");
    let input = last["input"].as_array().expect("input");
    let user_texts: Vec<_> = input
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"][0]["text"].as_str())
        .collect();
    assert!(user_texts.contains(&"edited"), "edited content sent: {user_texts:?}");

    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    assert_eq!(page.items[0].content, "edited");
}

#[tokio::test]
async fn delete_last_turn_removes_it() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid1 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(rid1)).await;
    bed.services
        .turns
        .delete_turn(&bed.bed.security, chat.id, rid1)
        .await
        .expect("delete");
    let page = bed
        .services
        .chats
        .list_messages(&bed.bed.security, chat.id, &odata())
        .await
        .expect("list");
    assert!(page.items.is_empty());
    // Turn status of the deleted turn -> 404.
    let err = bed
        .services
        .turns
        .get_turn(&bed.bed.security, chat.id, rid1)
        .await;
    assert!(matches!(err, Err(DomainError::NotFound(_))));
    // Deleting again -> NOT_LATEST_TURN (no latest turn exists).
    let err = bed
        .services
        .turns
        .delete_turn(&bed.bed.security, chat.id, rid1)
        .await;
    assert!(matches!(err, Err(DomainError::NotLatestTurn)) | matches!(err, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn mutation_of_non_latest_turn_rejected() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid1 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "first", Some(rid1)).await;
    let rid2 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "second", Some(rid2)).await;

    let err = bed
        .services
        .turns
        .retry_turn(&bed.bed.security, chat.id, rid1)
        .await;
    assert!(matches!(err, Err(DomainError::NotLatestTurn)));
    let err = bed
        .services
        .turns
        .edit_turn(&bed.bed.security, chat.id, rid1, "x".to_owned())
        .await;
    assert!(matches!(err, Err(DomainError::NotLatestTurn)));
    let err = bed
        .services
        .turns
        .delete_turn(&bed.bed.security, chat.id, rid1)
        .await;
    assert!(matches!(err, Err(DomainError::NotLatestTurn)));
}

#[tokio::test]
async fn mutation_of_running_turn_rejected() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    // Insert a running turn directly.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let rid = Uuid::new_v4();
    bed.bed
        .ctx
        .repos
        .turns
        .insert(
            &conn,
            &toolkit_db::secure::AccessScope::allow_all(),
            crate::infra::storage::repo::turns::NewTurn {
                id: Uuid::new_v4(),
                tenant_id: bed.bed.security.subject_tenant_id(),
                chat_id: chat.id,
                request_id: rid,
                requester_user_id: bed.bed.security.subject_id(),
                web_search_enabled: false,
                reserve_tokens: 100,
                max_output_tokens_applied: 50,
                reserved_credits_micro: 100,
                policy_version_applied: 1,
                effective_model: "gpt-premium".to_owned(),
                minimal_generation_floor_applied: 50,
            },
        )
        .await
        .expect("insert");
    let err = bed
        .services
        .turns
        .retry_turn(&bed.bed.security, chat.id, rid)
        .await;
    assert!(matches!(err, Err(DomainError::TurnStateNotTerminal)));
}

#[tokio::test]
async fn mutation_requires_owner() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(rid)).await;
    let other = toolkit_security::SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(bed.bed.security.subject_tenant_id())
        .build()
        .expect("ctx");
    let err = bed
        .services
        .turns
        .delete_turn(&other, chat.id, rid)
        .await;
    // The chat is not visible to another user: 404 (hidden by the scope).
    assert!(matches!(err, Err(DomainError::NotFound(_))));
}

#[tokio::test]
async fn concurrent_mutations_resolve_deterministically() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    let rid1 = Uuid::new_v4();
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", Some(rid1)).await;

    // Rendezvous inside the preflight so both mutations pass the preview and
    // reach the mutation commit concurrently.
    bed.bed.policy.arm_snapshot_rendezvous(2);
    let (r1, r2) = tokio::join!(
        bed.services.turns.retry_turn(&bed.bed.security, chat.id, rid1),
        bed.services.turns.retry_turn(&bed.bed.security, chat.id, rid1),
    );
    let (winner, loser) = match (r1, r2) {
        (Ok(w), Err(e)) | (Err(e), Ok(w)) => (w, e),
        (Err(e1), Err(e2)) => panic!("both mutations failed: {e1} / {e2}"),
        (Ok(_), Ok(_)) => panic!("both mutations won the running-turn race"),
    };
    // The loser of the running-turn insert race gets GENERATION_IN_PROGRESS.
    assert!(matches!(loser, DomainError::GenerationInProgress));

    // The winner streams a normal new turn.
    let events = match winner {
        StreamSetup::Live { receiver, .. } => drain(receiver).await,
        StreamSetup::Replay(events) => events,
    };
    assert!(started(&events).is_new_turn);
    let new = latest_turn(&bed.bed, chat.id).await;
    assert_eq!(new.state, "completed");
    // The old turn was soft-deleted exactly once.
    let conn = bed.bed.ctx.db.conn().expect("conn");
    let old = bed
        .bed
        .ctx
        .repos
        .turns
        .get_any(
            &conn,
            &toolkit_db::secure::AccessScope::allow_all(),
            bed.bed.security.subject_tenant_id(),
            chat.id,
            rid1,
        )
        .await
        .expect("old")
        .expect("old row");
    assert!(old.deleted_at.is_some());
}
