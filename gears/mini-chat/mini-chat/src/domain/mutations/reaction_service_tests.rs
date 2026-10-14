//! The reaction service over in-memory `SQLite`, the real stores and the PEP: the admission with
//! one PDP evaluation per call, the owner-scoped chat load, the target resolution (404 of the
//! message type, `ReactionTargetNotAssistant`), the upsert with the application clock and the
//! idempotent removal.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flows
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-set-reaction` (`inst-setr-admission`,
//! `inst-setr-refused`, `inst-setr-target`, `inst-setr-upsert`, `inst-setr-return-200`) and
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-remove-reaction` (`inst-delr-admission`,
//! `inst-delr-refused`, `inst-delr-target`, `inst-delr-remove`, `inst-delr-return-204`);
//! algorithms `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-target`
//! (`inst-rt-select`, `inst-rt-missing`, `inst-rt-not-assistant`, `inst-rt-return`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-upsert` (`inst-ru-upsert`,
//! `inst-ru-return`) and `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-reaction-removal`
//! (`inst-rr-delete`, `inst-rr-return`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-reactions`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::messages::Reaction;
use crate::test_support::chats::{a1, a2, b1};
use crate::test_support::mutations::{reaction_rows, seed, turn};
use crate::test_support::quota::default_now;
use crate::test_support::send::{
    MessageSeed, SendRigOptions, seed_msg, send_rig, send_rig_with, t,
};
use crate::test_support::{Pdp, USER_A1};

#[tokio::test]
async fn a_set_stores_the_reaction_with_the_clock_of_the_application() {
    // inst-setr-upsert, inst-setr-return-200
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    let stored = rig
        .reactions
        .set(&a1(), chat.id, assistant, Reaction::Like)
        .await
        .unwrap();
    assert_eq!(stored.message_id, assistant);
    assert_eq!(stored.reaction, Reaction::Like);
    assert_eq!(stored.created_at, default_now());
    let rows = reaction_rows(&rig.quota.db, assistant).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id, USER_A1);
}

#[tokio::test]
async fn a_set_and_a_remove_evaluate_the_pdp_once_with_their_action() {
    // inst-setr-admission, inst-delr-admission
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    rig.reactions
        .set(&a1(), chat.id, assistant, Reaction::Dislike)
        .await
        .unwrap();
    assert_eq!(rig.pdp.only_request().action.name, "set_reaction");

    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    rig.reactions
        .remove(&a1(), chat.id, seeded.assistant_message_id.unwrap())
        .await
        .unwrap();
    assert_eq!(rig.pdp.only_request().action.name, "delete_reaction");
}

#[tokio::test]
async fn a_user_or_a_system_message_is_not_a_target_for_either_method() {
    // inst-rt-not-assistant
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let system = seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("system", "s", t(3)).request(None),
    )
    .await;
    for message in [seeded.user_message_id, system] {
        let set = rig
            .reactions
            .set(&a1(), chat.id, message, Reaction::Like)
            .await;
        assert!(
            matches!(set, Err(DomainError::ReactionTargetNotAssistant)),
            "{set:?}"
        );
        let remove = rig.reactions.remove(&a1(), chat.id, message).await;
        assert!(
            matches!(remove, Err(DomainError::ReactionTargetNotAssistant)),
            "{remove:?}"
        );
    }
}

#[tokio::test]
async fn an_unknown_message_is_a_not_found_of_the_message_type() {
    // inst-rt-missing
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let set = rig
        .reactions
        .set(&a1(), chat.id, Uuid::new_v4(), Reaction::Like)
        .await;
    assert!(
        matches!(
            set,
            Err(DomainError::NotFound {
                resource: ResourceKind::Message,
                ..
            })
        ),
        "{set:?}"
    );
    let remove = rig.reactions.remove(&a1(), chat.id, Uuid::new_v4()).await;
    assert!(
        matches!(
            remove,
            Err(DomainError::NotFound {
                resource: ResourceKind::Message,
                ..
            })
        ),
        "{remove:?}"
    );
}

#[tokio::test]
async fn a_foreign_chat_is_a_not_found_of_the_chat_type_before_the_message_is_looked_up() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    for who in [a2(), b1()] {
        let set = rig
            .reactions
            .set(&who, chat.id, assistant, Reaction::Like)
            .await;
        assert!(
            matches!(
                set,
                Err(DomainError::NotFound {
                    resource: ResourceKind::Chat,
                    ..
                })
            ),
            "{set:?}"
        );
    }
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}

#[tokio::test]
async fn a_deny_and_an_unreachable_pdp_refuse_before_the_message_is_looked_up() {
    // inst-setr-refused, inst-delr-refused
    for (pdp, denied) in [(Pdp::Deny, true), (Pdp::Unreachable, false)] {
        let rig = send_rig_with(SendRigOptions::new().with_pdp(pdp)).await;
        let chat = rig.std_chat().await;
        let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
        let assistant = seeded.assistant_message_id.unwrap();
        let set = rig
            .reactions
            .set(&a1(), chat.id, assistant, Reaction::Like)
            .await;
        let remove = rig.reactions.remove(&a1(), chat.id, assistant).await;
        for result in [set.map(|_| ()), remove] {
            if denied {
                assert!(
                    matches!(result, Err(DomainError::AuthzDenied)),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(DomainError::AuthzUnavailable { .. })),
                    "{result:?}"
                );
            }
        }
        assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
    }
}

#[tokio::test]
async fn a_remove_is_idempotent_and_clears_the_reaction() {
    // inst-rr-delete, inst-rr-return
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seeded = seed(&rig.quota.db, &chat, turn("q")).await;
    let assistant = seeded.assistant_message_id.unwrap();
    rig.reactions
        .remove(&a1(), chat.id, assistant)
        .await
        .unwrap();
    rig.reactions
        .set(&a1(), chat.id, assistant, Reaction::Like)
        .await
        .unwrap();
    rig.reactions
        .remove(&a1(), chat.id, assistant)
        .await
        .unwrap();
    rig.reactions
        .remove(&a1(), chat.id, assistant)
        .await
        .unwrap();
    assert!(reaction_rows(&rig.quota.db, assistant).await.is_empty());
}
