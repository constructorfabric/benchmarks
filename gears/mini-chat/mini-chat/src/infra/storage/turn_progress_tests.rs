//! The durable progress and counter updates of a running turn over in-memory `SQLite`: the
//! three completed counters, the `last_progress_at` write, the `state = 'running'` guard and the
//! tenant scoping.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `tool-accounting` (`inst-ta-durable-count`, `inst-ta-progress-write`); definition of done
//! `tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm_migration::MigratorTrait;
use uuid::Uuid;

use crate::domain::streaming::{ToolKind, TurnProgress};
use crate::infra::storage::migrations::Migrator;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, at, seed_chat};
use crate::test_support::quota::{TurnSeed, seed_turn, turn_row};
use crate::test_support::send::{TurnSeedFull, seed_turn_full, t};
use crate::test_support::{TENANT_B, TestDb, sqlite_db};

async fn setup() -> (TestDb, ChatRecord) {
    let db = sqlite_db(Migrator::migrations()).await;
    let chat = seed_chat(&db, &a1(), Uuid::new_v4(), Some("chat"), "m", at(0)).await;
    (db, chat)
}

async fn running_turn(db: &TestDb, chat: &ChatRecord) -> Uuid {
    seed_turn(db, &TurnSeed::running(chat, t(0))).await
}

#[tokio::test]
async fn each_tool_increments_its_own_counter() {
    let (db, chat) = setup().await;
    let turn = running_turn(&db, &chat).await;
    for tool in [
        ToolKind::WebSearch,
        ToolKind::WebSearch,
        ToolKind::CodeInterpreter,
        ToolKind::FileSearch,
        ToolKind::FileSearch,
        ToolKind::FileSearch,
    ] {
        db.provider
            .record_tool_done(chat.tenant_id, turn, tool)
            .await
            .unwrap();
    }
    let row = turn_row(&db, turn).await;
    assert_eq!(row.web_search_completed_count, 2);
    assert_eq!(row.code_interpreter_completed_count, 1);
    assert_eq!(row.file_search_completed_count, 3);
}

#[tokio::test]
async fn search_knowledge_counts_as_a_file_search_call() {
    let (db, chat) = setup().await;
    let turn = running_turn(&db, &chat).await;
    db.provider
        .record_tool_done(chat.tenant_id, turn, ToolKind::SearchKnowledge)
        .await
        .unwrap();
    let row = turn_row(&db, turn).await;
    assert_eq!(row.file_search_completed_count, 1);
    assert_eq!(row.web_search_completed_count, 0);
}

#[tokio::test]
async fn a_turn_that_left_running_keeps_its_counters() {
    let (db, chat) = setup().await;
    for state in ["completed", "failed", "cancelled"] {
        let id = seed_turn_full(&db, &chat, TurnSeedFull::new(state)).await;
        db.provider
            .record_tool_done(chat.tenant_id, id, ToolKind::WebSearch)
            .await
            .unwrap();
        assert_eq!(
            turn_row(&db, id).await.web_search_completed_count,
            0,
            "{state}"
        );
    }
}

#[tokio::test]
async fn another_tenant_cannot_move_the_counters() {
    let (db, chat) = setup().await;
    let turn = running_turn(&db, &chat).await;
    db.provider
        .record_tool_done(TENANT_B, turn, ToolKind::WebSearch)
        .await
        .unwrap();
    assert_eq!(turn_row(&db, turn).await.web_search_completed_count, 0);
}

#[tokio::test]
async fn touching_progress_sets_last_progress_at_of_a_running_turn() {
    let (db, chat) = setup().await;
    let turn = running_turn(&db, &chat).await;
    let at = t(45_000_000);
    db.provider
        .touch_progress(chat.tenant_id, turn, at)
        .await
        .unwrap();
    let row = turn_row(&db, turn).await;
    assert_eq!(row.last_progress_at, Some(at));
}

#[tokio::test]
async fn touching_progress_is_guarded_by_the_running_state() {
    let (db, chat) = setup().await;
    let id = seed_turn_full(&db, &chat, TurnSeedFull::new("completed")).await;
    let before = turn_row(&db, id).await;
    db.provider
        .touch_progress(chat.tenant_id, id, t(99_000_000))
        .await
        .unwrap();
    let after = turn_row(&db, id).await;
    assert_eq!(after.last_progress_at, before.last_progress_at);
    assert_eq!(after.updated_at, before.updated_at);
}
