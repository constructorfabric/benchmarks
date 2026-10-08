#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use sea_orm::{ActiveValue::NotSet, Set};
use time::OffsetDateTime;
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::service::quota::store::{self, Bucket, PeriodKind};
use crate::domain::service::quota::{PreflightDecision, PreflightInput, credits_micro};
use crate::domain::service::test_support::{TENANT_A, TestEnv, USER_A1};
use crate::infra::db::entity::{chat, chat_turn};

const USAGE_Q: &str = "mini-chat.usage_snapshot";
const AUDIT_Q: &str = "mini-chat.audit";

async fn new_chat(env: &TestEnv) -> Uuid {
    let id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat::Entity>(
        chat::ActiveModel {
            id: Set(id),
            tenant_id: Set(TENANT_A),
            user_id: Set(USER_A1),
            model: Set("gpt-premium".into()),
            title: Set(None),
            is_temporary: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .unwrap();
    id
}

struct TurnSpec {
    started_at: OffsetDateTime,
    last_progress_at: Option<OffsetDateTime>,
    state: &'static str,
    deleted: bool,
    reserve: Option<PreflightDecision>,
}

async fn new_turn(env: &TestEnv, spec: TurnSpec) -> chat_turn::Model {
    let chat_id = new_chat(env).await;
    let r = spec.reserve.as_ref();
    let am = chat_turn::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        request_id: Set(Uuid::new_v4()),
        requester_type: Set("user".into()),
        requester_user_id: Set(Some(USER_A1)),
        state: Set(spec.state.into()),
        provider_name: NotSet,
        provider_response_id: NotSet,
        assistant_message_id: NotSet,
        error_code: Set(None),
        reserve_tokens: Set(r.map(|d| d.reserve_tokens)),
        max_output_tokens_applied: Set(r.map(|d| d.max_output_tokens_applied)),
        reserved_credits_micro: Set(r.map(|d| d.reserved_credits_micro)),
        policy_version_applied: Set(r.map(|d| i64::try_from(d.policy_version).unwrap())),
        effective_model: Set(r.map(|d| d.effective.id.clone())),
        minimal_generation_floor_applied: Set(r.map(|d| d.minimal_generation_floor_applied)),
        error_detail: NotSet,
        deleted_at: Set(spec.deleted.then_some(spec.started_at)),
        replaced_by_request_id: NotSet,
        started_at: Set(spec.started_at),
        last_progress_at: Set(spec.last_progress_at),
        web_search_enabled: Set(true),
        web_search_completed_count: Set(2),
        code_interpreter_completed_count: Set(1),
        file_search_completed_count: Set(3),
        completed_at: Set(None),
        updated_at: Set(spec.started_at),
    };
    let conn = env.deps.db.conn().unwrap();
    secure_insert::<chat_turn::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap()
}

async fn load_turn(env: &TestEnv, id: Uuid) -> chat_turn::Model {
    let conn = env.deps.db.conn().unwrap();
    chat_turn::Entity::find()
        .secure()
        .scope_with(&AccessScope::allow_all())
        .and_id(id)
        .unwrap()
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
}

/// Preflight + reserve of a premium turn (`message_bytes = 1000`).
async fn reserved_decision(env: &TestEnv) -> PreflightDecision {
    let quota = Arc::clone(&env.services.quota);
    let d = quota
        .preflight(&PreflightInput {
            tenant_id: TENANT_A,
            user_id: USER_A1,
            selected_model: "gpt-premium".into(),
            message_bytes: 1000,
            image_count: 0,
            prior_context_tokens: 0,
            web_search_requested: false,
            has_ready_documents: false,
            has_ready_code_interpreter_files: false,
        })
        .await
        .unwrap();
    let dd = d.clone();
    env.deps
        .db
        .transaction(move |tx| Box::pin(async move { quota.reserve_in_tx(tx, TENANT_A, USER_A1, &dd).await }))
        .await
        .unwrap();
    d
}

fn spec(started_at: OffsetDateTime, last: Option<OffsetDateTime>, reserve: Option<PreflightDecision>) -> TurnSpec {
    TurnSpec {
        started_at,
        last_progress_at: last,
        state: "running",
        deleted: false,
        reserve,
    }
}

const STALE: time::Duration = time::Duration::seconds(400); // timeout 300

#[tokio::test]
async fn stale_turn_is_finalized_settled_and_billed() {
    let env = TestEnv::default_env().await;
    let d = reserved_decision(&env).await;
    let now = OffsetDateTime::now_utc();
    let turn = new_turn(&env, spec(now, Some(now), Some(d.clone()))).await;
    let scan_at = now + STALE;

    assert_eq!(scan_once(&env.deps, scan_at).await.unwrap(), 1);
    let t = load_turn(&env, turn.id).await;
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(t.completed_at, Some(scan_at));
    assert_eq!(t.updated_at, scan_at);

    // estimated settlement: credits_micro(reserve - max_out, floor)
    let expected = credits_micro(
        d.reserve_tokens - i64::from(d.max_output_tokens_applied),
        i64::from(d.minimal_generation_floor_applied),
        1_000_000,
        3_000_000,
    )
    .unwrap();
    let conn = env.deps.db.conn().unwrap();
    let rows = store::load_rows(&conn, &store::user_scope(TENANT_A, USER_A1), TENANT_A, USER_A1, &d.periods)
        .await
        .unwrap();
    for p in PeriodKind::ALL {
        for b in [Bucket::Total, Bucket::Premium] {
            let r = rows.get(&d.periods, p, b).unwrap();
            assert_eq!((r.reserved_credits_micro, r.spent_credits_micro, r.calls), (0, expected, 1));
        }
        let total = rows.get(&d.periods, p, Bucket::Total).unwrap();
        assert_eq!((total.web_search_calls, total.code_interpreter_calls), (2, 1));
        assert_eq!((total.input_tokens, total.output_tokens), (0, 0));
    }

    let usage = env.delivered_to(USAGE_Q, 1).await;
    assert_eq!(usage.len(), 1);
    let u = &usage[0];
    assert_eq!(u["billing_outcome"], "aborted");
    assert_eq!(u["settlement_method"], "estimated");
    assert_eq!(u["terminal_state"], "failed");
    assert!(u["usage"].is_null());
    assert_eq!(u["actual_credits_micro"], expected);
    assert_eq!(u["effective_model"], "gpt-premium");
    assert_eq!(u["selected_model"], "gpt-premium");
    assert_eq!(u["policy_version_applied"], 1);
    assert_eq!(u["web_search_calls"], 2);
    assert_eq!(u["file_search_calls"], 3);
    assert_eq!(
        u["dedupe_key"],
        format!("{}/{}/{}", TENANT_A.as_simple(), turn.id.as_simple(), turn.request_id.as_simple())
    );
    let audit = env.delivered_to(AUDIT_Q, 1).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["event_type"], "turn_failed");
    assert_eq!(audit[0]["error_code"], "orphan_timeout");
    assert_eq!(audit[0]["policy_decisions"]["quota"]["decision"], "unknown");

    // a second scan finds nothing
    assert_eq!(scan_once(&env.deps, scan_at).await.unwrap(), 0);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(env.delivered_to(USAGE_Q, 0).await.len(), 1);
    env.shutdown().await;
}

#[tokio::test]
async fn fresh_progress_is_not_finalized() {
    let env = TestEnv::default_env().await;
    let now = OffsetDateTime::now_utc();
    // started long ago but progressed recently
    let turn = new_turn(&env, spec(now, Some(now + time::Duration::seconds(350)), None)).await;
    assert_eq!(scan_once(&env.deps, now + STALE).await.unwrap(), 0);
    assert_eq!(load_turn(&env, turn.id).await.state, "running");
    env.shutdown().await;
}

#[tokio::test]
async fn cas_rechecks_progress_after_discovery() {
    let env = TestEnv::default_env().await;
    let now = OffsetDateTime::now_utc();
    // discovered with a cutoff older than the (refreshed) progress → CAS loses
    let turn = new_turn(&env, spec(now, Some(now + time::Duration::seconds(10)), None)).await;
    let quota = Arc::new(QuotaService::new(Arc::clone(&env.deps)));
    let won = finalize_orphan(&env.deps, &quota, turn.id, now, now + STALE).await.unwrap();
    assert!(!won);
    assert_eq!(load_turn(&env, turn.id).await.state, "running");
    env.shutdown().await;
}

#[tokio::test]
async fn null_last_progress_falls_back_to_started_at() {
    let env = TestEnv::default_env().await;
    let now = OffsetDateTime::now_utc();
    let stale = new_turn(&env, spec(now, None, None)).await;
    let fresh = new_turn(&env, spec(now + time::Duration::seconds(350), None, None)).await;
    assert_eq!(scan_once(&env.deps, now + STALE).await.unwrap(), 1);
    assert_eq!(load_turn(&env, stale.id).await.state, "failed");
    assert_eq!(load_turn(&env, fresh.id).await.state, "running");
    env.shutdown().await;
}

#[tokio::test]
async fn finalized_and_deleted_turns_are_skipped() {
    let env = TestEnv::default_env().await;
    let now = OffsetDateTime::now_utc();
    let done = new_turn(
        &env,
        TurnSpec {
            state: "completed",
            ..spec(now, Some(now), None)
        },
    )
    .await;
    let deleted = new_turn(
        &env,
        TurnSpec {
            deleted: true,
            ..spec(now, Some(now), None)
        },
    )
    .await;
    assert_eq!(scan_once(&env.deps, now + STALE).await.unwrap(), 0);
    let quota = Arc::new(QuotaService::new(Arc::clone(&env.deps)));
    for id in [done.id, deleted.id] {
        assert!(!finalize_orphan(&env.deps, &quota, id, now + STALE, now + STALE).await.unwrap());
    }
    assert_eq!(load_turn(&env, done.id).await.state, "completed");
    assert_eq!(load_turn(&env, deleted.id).await.state, "running");
    env.shutdown().await;
}

#[tokio::test]
async fn missing_reserve_fields_skip_settlement_but_bill_zero() {
    let env = TestEnv::default_env().await;
    let now = OffsetDateTime::now_utc();
    let turn = new_turn(&env, spec(now, Some(now), None)).await;
    assert_eq!(scan_once(&env.deps, now + STALE).await.unwrap(), 1);
    let usage = env.delivered_to(USAGE_Q, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["actual_credits_micro"], 0);
    assert_eq!(usage[0]["effective_model"], "");
    assert_eq!(usage[0]["selected_model"], "");
    assert_eq!(usage[0]["policy_version_applied"], 0);
    assert_eq!(usage[0]["turn_id"], turn.id.to_string());
    let conn = env.deps.db.conn().unwrap();
    let periods = QuotaPeriods::of(now);
    let rows = store::load_rows(&conn, &store::user_scope(TENANT_A, USER_A1), TENANT_A, USER_A1, &periods)
        .await
        .unwrap();
    assert!(rows.get(&periods, PeriodKind::Daily, Bucket::Total).is_none(), "no settlement");
    env.shutdown().await;
}

#[tokio::test]
async fn failing_settlement_leaves_turn_running() {
    let env = TestEnv::default_env().await;
    let mut d = reserved_decision(&env).await;
    d.effective.id = "not-in-catalog".into();
    let now = OffsetDateTime::now_utc();
    let turn = new_turn(&env, spec(now, Some(now), Some(d))).await;
    assert_eq!(scan_once(&env.deps, now + STALE).await.unwrap(), 0);
    assert_eq!(load_turn(&env, turn.id).await.state, "running", "rolled back; retried next scan");
    env.shutdown().await;
}

#[tokio::test]
async fn run_loop_scans_as_leader_and_stops() {
    struct Never;
    #[async_trait::async_trait]
    impl LeaderElector for Never {
        async fn is_leader(&self) -> bool {
            false
        }
    }
    let env = TestEnv::default_env().await;
    let old = OffsetDateTime::now_utc() - time::Duration::seconds(1000);
    let turn = new_turn(&env, spec(old, Some(old), None)).await;

    let cancel = CancellationToken::new();
    let h = tokio::spawn(run_with_elector(Arc::clone(&env.deps), Arc::new(Never), cancel.clone()));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    cancel.cancel();
    h.await.unwrap();
    assert_eq!(load_turn(&env, turn.id).await.state, "running", "non-leader does not scan");

    let cancel = CancellationToken::new();
    let h = tokio::spawn(run(Arc::clone(&env.deps), cancel.clone()));
    for _ in 0..50 {
        if load_turn(&env, turn.id).await.state == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    cancel.cancel();
    h.await.unwrap();
    assert_eq!(load_turn(&env, turn.id).await.state, "failed");
    env.shutdown().await;
}
