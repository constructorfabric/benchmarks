#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the Secure ORM orphan store
//! (`cpt-cf-mini-chat-algo-background-workers-run-watchdog`, `inst-select-orphans`;
//! `-finalize-orphan`, `inst-enqueue-orphan-audit`): the scan predicate, its
//! order and batch bound, and the audit enqueue of the finalization
//! transaction.

use time::OffsetDateTime;
use uuid::Uuid;

use crate::chat_test_env::request_id;
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::workers::orphan::{OrphanCandidate, orphan_audit_event};
use crate::quota_test_env::TurnSeed;
use crate::test_doubles::{USER, USER_TENANT};
use crate::worker_test_env::WorkerEnv;

fn cutoff() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_773_143_700).unwrap() // 2026-03-10T11:55:00Z
}

fn stale(n: u128, chat: Uuid, started: &str, progress: Option<&str>) -> TurnSeed {
    TurnSeed::standard(n, chat).started(started, progress)
}

async fn chat(env: &WorkerEnv, n: u128) -> Uuid {
    if n == 1 {
        env.chat
    } else {
        env.quota.seed_chat(n).await
    }
}

async fn ids(env: &WorkerEnv) -> Vec<Uuid> {
    env.orphan_store()
        .scan(cutoff(), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[tokio::test]
async fn the_scan_reads_the_columns_the_watchdog_needs() {
    let env = WorkerEnv::new().await;
    let seed = stale(
        1,
        env.chat,
        "2026-03-10T11:00:00Z",
        Some("2026-03-10T11:10:00Z"),
    )
    .tools_done(2, 0, 3);
    env.quota.seed_turn(&seed).await;

    let rows = env.orphan_store().scan(cutoff(), 100).await.unwrap();

    assert_eq!(
        rows,
        vec![OrphanCandidate {
            id: seed.id(),
            tenant_id: USER_TENANT,
            chat_id: env.chat,
            request_id: request_id(1),
            requester_user_id: Some(USER),
            effective_model: Some("s".to_owned()),
            policy_version_applied: Some(7),
            started_at: OffsetDateTime::from_unix_timestamp(1_773_140_400).unwrap(),
            web_search_completed_count: 2,
            file_search_completed_count: 3,
        }]
    );
}

#[tokio::test]
async fn a_turn_before_its_reserve_has_no_model_and_no_version() {
    let env = WorkerEnv::new().await;
    env.quota
        .seed_turn(
            &stale(1, env.chat, "2026-03-10T11:00:00Z", None)
                .without_reserve()
                .without_requester(),
        )
        .await;

    let rows = env.orphan_store().scan(cutoff(), 100).await.unwrap();

    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].effective_model, None);
    assert_eq!(rows[0].policy_version_applied, None);
    assert_eq!(rows[0].requester_user_id, None);
}

#[tokio::test]
async fn the_scan_selects_running_non_deleted_turns_with_stale_progress_only() {
    let env = WorkerEnv::new().await;
    let c = [
        chat(&env, 1).await,
        chat(&env, 2).await,
        chat(&env, 3).await,
        chat(&env, 4).await,
        chat(&env, 5).await,
        chat(&env, 6).await,
    ];
    let stale_by_progress = env
        .quota
        .seed_turn(&stale(
            1,
            c[0],
            "2026-03-10T10:00:00Z",
            Some("2026-03-10T11:00:00Z"),
        ))
        .await;
    let stale_by_started = env
        .quota
        .seed_turn(&stale(2, c[1], "2026-03-10T11:00:00Z", None))
        .await;
    // recent progress beats an old started_at
    env.quota
        .seed_turn(&stale(
            3,
            c[2],
            "2026-03-09T00:00:00Z",
            Some("2026-03-10T11:58:00Z"),
        ))
        .await;
    // recent start without progress
    env.quota
        .seed_turn(&stale(4, c[3], "2026-03-10T11:58:00Z", None))
        .await;
    let mut deleted = stale(
        5,
        c[4],
        "2026-03-10T10:00:00Z",
        Some("2026-03-10T11:00:00Z"),
    );
    deleted.deleted = true;
    env.quota.seed_turn(&deleted).await;
    env.quota
        .seed_turn(
            &stale(
                6,
                c[5],
                "2026-03-10T10:00:00Z",
                Some("2026-03-10T11:00:00Z"),
            )
            .state("completed"),
        )
        .await;

    let selected = ids(&env).await;

    assert_eq!(selected.len(), 2, "{selected:?}");
    assert!(selected.contains(&stale_by_progress));
    assert!(selected.contains(&stale_by_started));
}

#[tokio::test]
async fn the_scan_orders_null_progress_first_then_progress_started_at_and_id() {
    let env = WorkerEnv::new().await;
    let mut turns = Vec::new();
    // (n, started, progress) in a deliberately scrambled insertion order
    let specs = [
        (1_u128, "2026-03-10T10:00:00Z", Some("2026-03-10T11:20:00Z")),
        (2, "2026-03-10T10:50:00Z", None),
        (3, "2026-03-10T10:00:00Z", Some("2026-03-10T11:10:00Z")),
        (4, "2026-03-10T10:30:00Z", None),
        (5, "2026-03-10T10:05:00Z", Some("2026-03-10T11:10:00Z")),
        (6, "2026-03-10T10:05:00Z", Some("2026-03-10T11:10:00Z")),
    ];
    for (n, started, progress) in specs {
        let chat = chat(&env, n).await;
        turns.push((
            n,
            env.quota
                .seed_turn(&stale(n, chat, started, progress))
                .await,
        ));
    }
    let id_of = |n: u128| turns.iter().find(|(m, _)| *m == n).unwrap().1;

    let selected = ids(&env).await;

    assert_eq!(
        selected,
        vec![
            id_of(4), // NULL progress, started 10:30
            id_of(2), // NULL progress, started 10:50
            id_of(3), // progress 11:10, started 10:00
            id_of(5), // progress 11:10, started 10:05, lower id
            id_of(6), // same, higher id
            id_of(1), // progress 11:20
        ]
    );
}

#[tokio::test]
async fn the_scan_returns_at_most_the_limit() {
    let env = WorkerEnv::new().await;
    for n in 1..=7_u128 {
        let chat = chat(&env, n).await;
        env.quota
            .seed_turn(&stale(
                n,
                chat,
                "2026-03-10T10:00:00Z",
                Some(&format!("2026-03-10T11:0{n}:00Z")),
            ))
            .await;
    }

    let rows = env.orphan_store().scan(cutoff(), 3).await.unwrap();

    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].id,
        TurnSeed::standard(1, env.chat).id(),
        "the oldest progress first"
    );
}

#[tokio::test]
async fn the_scan_crosses_tenants() {
    let env = WorkerEnv::new().await;
    let other = crate::test_doubles::OTHER_TENANT;
    let chat2 = env
        .quota
        .seed_chat_of(2, other, crate::test_doubles::OTHER_USER)
        .await;
    env.quota
        .seed_turn(&stale(
            1,
            env.chat,
            "2026-03-10T10:00:00Z",
            Some("2026-03-10T11:00:00Z"),
        ))
        .await;
    let mut foreign = stale(
        2,
        chat2,
        "2026-03-10T10:00:00Z",
        Some("2026-03-10T11:01:00Z"),
    );
    foreign.tenant = other;
    env.quota.seed_turn(&foreign).await;

    let rows = env.orphan_store().scan(cutoff(), 100).await.unwrap();

    let tenants: Vec<Uuid> = rows.iter().map(|row| row.tenant_id).collect();
    assert_eq!(tenants, vec![USER_TENANT, other]);
}

// ---- the finalization transaction ---------------------------------------------------------------------------------

fn orphan_request(turn: Uuid, env: &WorkerEnv) -> FinalizeRequest {
    FinalizeRequest::orphan(turn, cutoff(), SettlementSnapshot::Unavailable, env.now())
}

#[tokio::test]
async fn the_audit_event_is_enqueued_in_the_same_transaction_as_the_cas_and_the_usage_event() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let seed = stale(
        1,
        env.chat,
        "2026-03-10T11:00:00Z",
        Some("2026-03-10T11:10:00Z"),
    );
    let turn = env.quota.seed_turn(&seed).await;
    let candidate = env
        .orphan_store()
        .scan(cutoff(), 10)
        .await
        .unwrap()
        .remove(0);
    let audit = orphan_audit_event(&candidate, env.now());

    let result = env
        .orphan_store()
        .finalize(&orphan_request(turn, &env), audit.as_ref())
        .await
        .unwrap();

    assert!(matches!(result, FinalizeResult::Won(_)));
    assert_eq!(env.usage().await.len(), 1);
    assert_eq!(env.audits().await.len(), 1);
}

#[tokio::test]
async fn a_lost_cas_enqueues_neither_usage_nor_audit() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    // progress is fresh: the CAS predicate fails
    let turn = env
        .quota
        .seed_turn(&stale(
            1,
            env.chat,
            "2026-03-10T10:00:00Z",
            Some("2026-03-10T11:59:00Z"),
        ))
        .await;
    let candidate = OrphanCandidate {
        id: turn,
        tenant_id: USER_TENANT,
        chat_id: env.chat,
        request_id: request_id(1),
        requester_user_id: Some(USER),
        effective_model: Some("s".to_owned()),
        policy_version_applied: Some(7),
        started_at: env.now(),
        web_search_completed_count: 0,
        file_search_completed_count: 0,
    };
    let audit = orphan_audit_event(&candidate, env.now());

    let result = env
        .orphan_store()
        .finalize(&orphan_request(turn, &env), audit.as_ref())
        .await
        .unwrap();

    assert_eq!(result, FinalizeResult::Lost);
    assert!(env.usage().await.is_empty());
    assert!(env.audits().await.is_empty());
}

#[tokio::test]
async fn a_finalization_without_an_audit_event_enqueues_only_the_usage_event() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env
        .quota
        .seed_turn(&stale(
            1,
            env.chat,
            "2026-03-10T11:00:00Z",
            Some("2026-03-10T11:10:00Z"),
        ))
        .await;

    env.orphan_store()
        .finalize(&orphan_request(turn, &env), None)
        .await
        .unwrap();

    assert_eq!(env.usage().await.len(), 1);
    assert!(env.audits().await.is_empty());
}

#[tokio::test]
async fn a_failing_audit_enqueue_rolls_the_whole_finalization_back() {
    let env = WorkerEnv::new().await;
    env.quota.seed_design_world(1_500_000, false).await;
    let turn = env
        .quota
        .seed_turn(&stale(
            1,
            env.chat,
            "2026-03-10T11:00:00Z",
            Some("2026-03-10T11:10:00Z"),
        ))
        .await;
    let candidate = env
        .orphan_store()
        .scan(cutoff(), 10)
        .await
        .unwrap()
        .remove(0);
    let audit = orphan_audit_event(&candidate, env.now());
    // the audit queue of this store is not registered: the enqueue fails after the other statements ran
    let quota = &env.quota;
    let mismatched = std::sync::Arc::new(crate::infra::outbox::queues::OutboxEnqueuer::new(
        &crate::config::OutboxConfig {
            audit_queue_name: "mini-chat.not_registered".to_owned(),
            num_partitions: 4,
            ..crate::config::OutboxConfig::default()
        },
    ));
    assert!(mismatched.bind(&quota.enqueuer.outbox().unwrap()));
    let store = super::SeaOrphanStore::new(
        std::sync::Arc::clone(&quota.provider),
        std::sync::Arc::clone(&quota.repo),
        mismatched,
    );
    let quota_before = quota.quota_dump().await;

    let result = crate::domain::service::workers::ports::OrphanStore::finalize(
        &store,
        &orphan_request(turn, &env),
        audit.as_ref(),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(
        quota.turn_state(turn).await,
        "running",
        "the CAS was rolled back"
    );
    assert_eq!(quota.quota_dump().await, quota_before);
    assert!(env.usage().await.is_empty());
}
