#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder};
use toolkit_db::secure::SecureEntityExt;
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::ports::{
    MessageKey, MessageRole, ThreadSummaryCommit, ThreadSummaryCommitOutcome, ThreadSummaryStore,
    ThreadSummaryTask,
};
use crate::domain::summary_service::system_usage_event;
use crate::infra::db::entities::{messages, thread_summaries};
use crate::infra::db::repo::tests::{
    MessageSeed, TENANT_A, TestStore, USER_A1, at, seed_chat, seed_message,
};

struct Seeded {
    chat_id: Uuid,
    /// `(key, id)` of the live messages, chronological.
    keys: Vec<MessageKey>,
    deleted: Uuid,
}

/// A chat with six live messages (1 s apart) and one deleted message
/// between the second and the third.
async fn seeded(store: &TestStore) -> Seeded {
    let chat_id = seed_chat(store, TENANT_A, USER_A1, None).await;
    let mut keys = Vec::new();
    for i in 0..6 {
        let role = if i % 2 == 0 {
            MessageRole::User
        } else {
            MessageRole::Assistant
        };
        let seed = MessageSeed::new(role, i * 1000);
        let created_at = seed.created_at;
        let id = seed_message(store, chat_id, TENANT_A, seed).await;
        keys.push(MessageKey { created_at, id });
    }
    let mut seed = MessageSeed::new(MessageRole::User, 1500);
    seed.deleted = true;
    let deleted = seed_message(store, chat_id, TENANT_A, seed).await;
    Seeded {
        chat_id,
        keys,
        deleted,
    }
}

fn scope() -> AccessScope {
    AccessScope::for_tenant(TENANT_A)
}

fn commit(
    chat_id: Uuid,
    base: Option<MessageKey>,
    target: MessageKey,
    text: &str,
) -> ThreadSummaryCommit {
    let task = ThreadSummaryTask {
        tenant_id: TENANT_A,
        chat_id,
        system_request_id: Uuid::new_v4(),
        base_frontier: base,
        frozen_target: target,
    };
    let now = at(10_000);
    ThreadSummaryCommit {
        tenant_id: TENANT_A,
        chat_id,
        summary_id: Uuid::new_v4(),
        base_frontier: base,
        target,
        summary_text: text.to_owned(),
        token_estimate: 7,
        usage_event: system_usage_event(&task, "gpt-4.1-mini", None, now),
        at: now,
    }
}

async fn compressed(store: &TestStore, chat_id: Uuid) -> Vec<(Uuid, bool)> {
    messages::Entity::find()
        .filter(messages::Column::ChatId.eq(chat_id))
        .order_by_asc(messages::Column::CreatedAt)
        .order_by_asc(messages::Column::Id)
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&store.conn())
        .await
        .unwrap()
        .into_iter()
        .map(|m| (m.id, m.is_compressed))
        .collect()
}

async fn summary_row(store: &TestStore, chat_id: Uuid) -> Option<thread_summaries::Model> {
    thread_summaries::Entity::find()
        .filter(thread_summaries::Column::ChatId.eq(chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&store.conn())
        .await
        .unwrap()
}

#[tokio::test]
async fn range_is_exactly_base_to_target_live_and_chronological() {
    let store = TestStore::new().await;
    let s = seeded(&store).await;

    let all = store
        .repo
        .summary_range(&scope(), s.chat_id, None, s.keys[3])
        .await
        .unwrap();
    let ids: Vec<Uuid> = all.iter().map(|m| m.id).collect();
    assert_eq!(ids, s.keys[..4].iter().map(|k| k.id).collect::<Vec<_>>());
    assert!(!ids.contains(&s.deleted));

    let tail = store
        .repo
        .summary_range(&scope(), s.chat_id, Some(s.keys[1]), s.keys[4])
        .await
        .unwrap();
    let ids: Vec<Uuid> = tail.iter().map(|m| m.id).collect();
    assert_eq!(ids, s.keys[2..5].iter().map(|k| k.id).collect::<Vec<_>>());
}

#[tokio::test]
async fn first_commit_inserts_compresses_the_range_and_enqueues_usage() {
    let store = TestStore::new().await;
    let s = seeded(&store).await;

    let outcome = store
        .repo
        .commit_summary(&scope(), commit(s.chat_id, None, s.keys[3], "First."))
        .await
        .unwrap();

    assert_eq!(outcome, ThreadSummaryCommitOutcome::Committed);
    let row = summary_row(&store, s.chat_id).await.expect("summary row");
    assert_eq!(row.summary_text, "First.");
    assert_eq!(row.summarized_up_to_message_id, s.keys[3].id);
    assert_eq!(row.summarized_up_to_created_at.at(), s.keys[3].created_at);
    assert_eq!(row.token_estimate, 7);
    assert_eq!(row.tenant_id, TENANT_A);

    let flags = compressed(&store, s.chat_id).await;
    for (id, is_compressed) in flags {
        let in_range = s.keys[..4].iter().any(|k| k.id == id);
        assert_eq!(is_compressed, in_range, "message {id}");
    }

    let usage_queue = store.enqueuer.queues().usage.clone();
    let delivered = store.wait_delivered(&usage_queue, 1).await;
    assert_eq!(delivered.len(), 1);
    let payload = &delivered[0].payload;
    assert_eq!(payload["billing_outcome"], "system_task");
    assert_eq!(payload["requester_type"], "system");
    assert!(payload.get("user_id").is_none());
}

#[tokio::test]
async fn second_commit_requires_the_stored_frontier_to_equal_the_base() {
    let store = TestStore::new().await;
    let s = seeded(&store).await;
    store
        .repo
        .commit_summary(&scope(), commit(s.chat_id, None, s.keys[1], "First."))
        .await
        .unwrap();

    // A first-summary task after the row exists loses the CAS.
    let lost = store
        .repo
        .commit_summary(&scope(), commit(s.chat_id, None, s.keys[3], "Stale."))
        .await
        .unwrap();
    assert_eq!(lost, ThreadSummaryCommitOutcome::CasConflict);
    // A task based on another frontier loses the CAS.
    let lost = store
        .repo
        .commit_summary(
            &scope(),
            commit(s.chat_id, Some(s.keys[2]), s.keys[4], "Stale."),
        )
        .await
        .unwrap();
    assert_eq!(lost, ThreadSummaryCommitOutcome::CasConflict);
    let row = summary_row(&store, s.chat_id).await.unwrap();
    assert_eq!(row.summary_text, "First.");
    assert_eq!(row.summarized_up_to_message_id, s.keys[1].id);
    assert!(
        !compressed(&store, s.chat_id)
            .await
            .iter()
            .any(|(id, c)| *c && *id == s.keys[2].id)
    );

    // The matching base advances the frontier and compresses (base, target].
    let won = store
        .repo
        .commit_summary(
            &scope(),
            commit(s.chat_id, Some(s.keys[1]), s.keys[4], "Second."),
        )
        .await
        .unwrap();
    assert_eq!(won, ThreadSummaryCommitOutcome::Committed);
    let row = summary_row(&store, s.chat_id).await.unwrap();
    assert_eq!(row.summary_text, "Second.");
    assert_eq!(row.summarized_up_to_message_id, s.keys[4].id);
    let flags = compressed(&store, s.chat_id).await;
    for (id, is_compressed) in flags {
        let in_range = s.keys[..5].iter().any(|k| k.id == id);
        assert_eq!(is_compressed, in_range, "message {id}");
    }
}

#[tokio::test]
async fn deleted_target_or_missing_base_writes_nothing() {
    let store = TestStore::new().await;
    let s = seeded(&store).await;
    let deleted_target = MessageKey {
        created_at: at(1500),
        id: s.deleted,
    };

    let outcome = store
        .repo
        .commit_summary(&scope(), commit(s.chat_id, None, deleted_target, "X."))
        .await
        .unwrap();
    assert_eq!(outcome, ThreadSummaryCommitOutcome::FrontierDeleted);

    let outcome = store
        .repo
        .commit_summary(
            &scope(),
            commit(s.chat_id, Some(s.keys[1]), s.keys[3], "X."),
        )
        .await
        .unwrap();
    assert_eq!(outcome, ThreadSummaryCommitOutcome::BaseMissing);

    assert!(summary_row(&store, s.chat_id).await.is_none());
    assert!(compressed(&store, s.chat_id).await.iter().all(|(_, c)| !c));
    let usage_queue = store.enqueuer.queues().usage.clone();
    assert!(store.wait_delivered(&usage_queue, 1).await.is_empty());
}
