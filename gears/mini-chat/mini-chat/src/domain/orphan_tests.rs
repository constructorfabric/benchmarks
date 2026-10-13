#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Orphan finalization tests on in-memory `SQLite` with a running outbox and
//! the fake model-policy plugin (DESIGN section 4 "Orphan Turn Watchdog",
//! B.9.1, section 5.7 orphan path). The fixture is shared with the watchdog
//! worker tests.

use std::sync::Arc;

use mini_chat_sdk::{KillSwitches, MiniChatModelPolicyPluginClientV1, dedupe_key_for_turn};
use sea_orm::sea_query::Expr;
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use time::macros::datetime;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;

use super::*;
use crate::config::MiniChatConfig;
use crate::domain::chat_service::tests::{OwnerPdp, owner_authz};
use crate::domain::ports::{QuotaBucketKey, QuotaRepository};
use crate::domain::quota::{QuotaBucket, QuotaPeriod};
use crate::domain::quota_service::{PreflightInput, PreflightRequest, QuotaSettings};
use crate::domain::stream::service::tests::{FakePolicy, standard_credits, test_catalog};
use crate::infra::db::entities::{chat_turns, messages, quota_usage};
use crate::infra::db::repo::quota as quota_repo;
use crate::infra::db::repo::tests::{
    TENANT_A, TENANT_B, TestStore, USER_A1, USER_A2, at, seed_chat,
};
use crate::infra::outbox::OutboxQueues;

/// Ten minutes: older than the default 300 s orphan timeout.
pub const STALE_MS: i64 = -600_000;

/// Reserve columns of the seeded turns (`standard` model).
pub const RESERVE_TOKENS: i64 = 2_000;
pub const MAX_OUTPUT_TOKENS: i32 = 1_000;
pub const FLOOR: i32 = 50;

/// The estimated charge of a seeded turn: `credits_micro(reserve - max_out, floor)`.
pub fn estimated_charge() -> i64 {
    standard_credits(
        RESERVE_TOKENS - i64::from(MAX_OUTPUT_TOKENS),
        i64::from(FLOOR),
    )
}

/// Database, outbox, fake policy, the quota service and the finalizer
/// (timeout 300 s).
pub struct OrphanFixture {
    pub store: Arc<TestStore>,
    pub policy: Arc<FakePolicy>,
    pub quota: Arc<QuotaService>,
    pub finalizer: Arc<OrphanFinalizer>,
    pub queues: OutboxQueues,
}

impl OrphanFixture {
    pub async fn new() -> Self {
        let store = Arc::new(TestStore::new().await);
        let policy = FakePolicy::new(test_catalog());
        let quota = Arc::new(QuotaService::new(
            Arc::clone(&store.repo) as Arc<dyn QuotaRepository>,
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            owner_authz(&OwnerPdp::new()),
            QuotaSettings::from_config(&MiniChatConfig::default()),
        ));
        let finalizer = Arc::new(OrphanFinalizer::new(
            store.db.clone(),
            Arc::clone(&store.repo) as Arc<dyn OrphanTurnRepository>,
            Arc::clone(&store.repo) as Arc<dyn TurnOutbox>,
            Arc::clone(&quota),
            300,
        ));
        Self {
            store,
            policy,
            quota,
            finalizer,
            queues: OutboxQueues::default(),
        }
    }

    /// A running turn of a new chat of `tenant` (see [`RunningSeed`]).
    pub async fn running(&self, tenant: Uuid, seed: RunningSeed) -> OrphanCandidate {
        self.running_with(tenant, seed, at(seed.started_ms), 5_000)
            .await
    }

    /// A running turn of a new chat of `tenant` started at `started_at`
    /// with `reserved_credits_micro` in its reserve columns (`seed` gives
    /// the rest; its `started_ms` is ignored).
    pub async fn running_with(
        &self,
        tenant: Uuid,
        seed: RunningSeed,
        started_at: OffsetDateTime,
        reserved_credits_micro: i64,
    ) -> OrphanCandidate {
        let chat_id = seed_chat(&self.store, tenant, USER_A1, None).await;
        let id = Uuid::new_v4();
        let reserve = seed.with_reserve;
        let model = chat_turns::ActiveModel {
            id: Set(id),
            tenant_id: Set(tenant),
            chat_id: Set(chat_id),
            request_id: Set(Uuid::new_v4()),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(seed.requester),
            state: Set("running".to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(reserve.then_some(RESERVE_TOKENS)),
            max_output_tokens_applied: Set(reserve.then_some(MAX_OUTPUT_TOKENS)),
            reserved_credits_micro: Set(reserve.then_some(reserved_credits_micro)),
            policy_version_applied: Set(reserve.then_some(seed.policy_version)),
            effective_model: Set(reserve.then(|| seed.model.to_owned())),
            minimal_generation_floor_applied: Set(reserve.then_some(FLOOR)),
            error_detail: Set(None),
            deleted_at: Set(seed.deleted.then(|| self.store.stamp(at(0)))),
            replaced_by_request_id: Set(None),
            started_at: Set(self.store.stamp(started_at)),
            last_progress_at: Set(seed.progress_ms.map(|ms| self.store.stamp(at(ms)))),
            web_search_enabled: Set(false),
            web_search_completed_count: Set(2),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(1),
            completed_at: Set(None),
            updated_at: Set(self.store.stamp(started_at)),
        };
        secure_insert::<chat_turns::Entity>(model, &AccessScope::allow_all(), &self.store.conn())
            .await
            .expect("seed turn");
        OrphanCandidate {
            id,
            tenant_id: tenant,
            chat_id,
        }
    }

    pub async fn turn(&self, id: Uuid) -> chat_turns::Model {
        chat_turns::Entity::find()
            .filter(chat_turns::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&self.store.conn())
            .await
            .unwrap()
            .unwrap()
    }

    /// Sets `last_progress_at` of `id` to `at(offset_ms)`.
    pub async fn progress(&self, id: Uuid, offset_ms: i64) {
        chat_turns::Entity::update_many()
            .col_expr(
                chat_turns::Column::LastProgressAt,
                Expr::value(Some(self.store.stamp(at(offset_ms)))),
            )
            .filter(chat_turns::Column::Id.eq(id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .exec(&self.store.conn())
            .await
            .unwrap();
    }

    /// Books `credits_micro` on bucket `total` of both periods of a turn
    /// started at `at(started_ms)` for `USER_A1` of `tenant`.
    pub async fn reserve_total(&self, tenant: Uuid, started_ms: i64, credits_micro: i64) {
        self.reserve_buckets(tenant, started_ms, credits_micro, &[QuotaBucket::Total])
            .await;
    }

    /// Books `credits_micro` on `buckets` of both periods of a turn started
    /// at `at(started_ms)` for `USER_A1` of `tenant`.
    pub async fn reserve_buckets(
        &self,
        tenant: Uuid,
        started_ms: i64,
        credits_micro: i64,
        buckets: &[QuotaBucket],
    ) {
        let periods = PeriodStarts::at(at(started_ms));
        let keys: Vec<_> = buckets
            .iter()
            .flat_map(|&bucket| {
                QuotaPeriod::ALL.iter().map(move |&period| QuotaBucketKey {
                    period,
                    period_start: periods.start(period),
                    bucket,
                })
            })
            .collect();
        quota_repo::add_reserve(
            &self.store.conn(),
            self.store.ts(),
            &AccessScope::allow_all(),
            QuotaOwner {
                tenant_id: tenant,
                user_id: USER_A1,
            },
            &keys,
            credits_micro,
            at(0),
        )
        .await
        .unwrap();
    }

    pub async fn quota_rows(&self, user: Uuid) -> Vec<quota_usage::Model> {
        quota_usage::Entity::find()
            .filter(quota_usage::Column::UserId.eq(user))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&self.store.conn())
            .await
            .unwrap()
    }
}

/// A running turn to seed: `started_ms` / `progress_ms` are offsets from now
/// (`progress_ms = None` stores a NULL `last_progress_at`).
#[derive(Debug, Clone, Copy)]
pub struct RunningSeed {
    pub started_ms: i64,
    pub progress_ms: Option<i64>,
    pub with_reserve: bool,
    pub requester: Option<Uuid>,
    pub deleted: bool,
    /// `policy_version_applied` (with the reserve columns).
    pub policy_version: i64,
    /// `effective_model` (with the reserve columns).
    pub model: &'static str,
}

impl RunningSeed {
    /// Started and last progressed ten minutes ago, reserve columns set.
    pub const fn stale() -> Self {
        Self {
            started_ms: STALE_MS,
            progress_ms: Some(STALE_MS),
            with_reserve: true,
            requester: Some(USER_A1),
            deleted: false,
            policy_version: 1,
            model: "standard",
        }
    }

    /// Started ten minutes ago, progressed just now.
    pub const fn progressing() -> Self {
        Self {
            progress_ms: Some(0),
            ..Self::stale()
        }
    }
}

fn payload_of(
    delivered: &[crate::infra::db::repo::tests::Delivered],
    turn_id: Uuid,
) -> Vec<&serde_json::Value> {
    delivered
        .iter()
        .map(|d| &d.payload)
        .filter(|p| p["turn_id"] == serde_json::json!(turn_id))
        .collect()
}

// ── Finalization ─────────────────────────────────────────────────────────

#[tokio::test]
async fn stale_turn_is_failed_settled_estimated_and_reported() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    let before = fx.turn(turn.id).await;

    let candidates = fx.finalizer.candidates().await.unwrap();
    assert_eq!(candidates, [turn]);
    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    let charge = estimated_charge();
    assert_eq!(
        outcome,
        OrphanOutcome::Finalized(OrphanSettlement::Estimated {
            committed_credits_micro: charge
        })
    );
    let row = fx.turn(turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some(ORPHAN_TIMEOUT));
    assert_eq!(row.completed_at, Some(row.updated_at));
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(row.last_progress_at, before.last_progress_at);

    // Estimated settlement on the periods of `started_at`; the reserve is
    // released (clamped at zero), no token telemetry.
    let rows = fx.quota_rows(USER_A1).await;
    let totals: Vec<_> = rows.iter().filter(|r| r.bucket == "total").collect();
    assert_eq!(totals.len(), 2, "{rows:?}");
    let periods = PeriodStarts::at(before.started_at.at());
    for row in totals {
        assert_eq!(row.spent_credits_micro, charge, "{row:?}");
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.calls, 1, "{row:?}");
        assert_eq!((row.input_tokens, row.output_tokens), (0, 0), "{row:?}");
        assert!(row.period_start == periods.daily || row.period_start == periods.monthly);
    }

    // No message is created.
    let messages = messages::Entity::find()
        .filter(messages::Column::ChatId.eq(turn.chat_id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&fx.store.conn())
        .await
        .unwrap();
    assert!(messages.is_empty());

    // Usage: aborted / estimated, selected model = effective model.
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    let usage = payload_of(&usage, turn.id);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = usage[0];
    assert_eq!(event["billing_outcome"], "aborted");
    assert_eq!(event["settlement_method"], "estimated");
    assert_eq!(event["terminal_state"], "failed");
    assert_eq!(event["usage"], serde_json::Value::Null);
    assert_eq!(event["actual_credits_micro"], charge);
    assert_eq!(event["effective_model"], "standard");
    assert_eq!(event["selected_model"], "standard");
    assert_eq!(event["policy_version_applied"], 1);
    assert_eq!(event["user_id"], serde_json::json!(USER_A1));
    assert_eq!(event["request_id"], serde_json::json!(before.request_id));
    assert_eq!(event["web_search_calls"], 2);
    assert_eq!(event["file_search_calls"], 1);
    assert_eq!(
        event["dedupe_key"],
        dedupe_key_for_turn(TENANT_A, turn.id, before.request_id)
    );

    // Audit: turn_failed, quota decision unknown, no content.
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    let audit = payload_of(&audit, turn.id);
    assert_eq!(audit.len(), 1, "{audit:?}");
    let event = audit[0];
    assert_eq!(event["event_type"], "turn_failed");
    assert_eq!(event["error_code"], ORPHAN_TIMEOUT);
    assert_eq!(event["policy_decisions"]["quota"]["decision"], "unknown");
    assert_eq!(event["selected_model"], "standard");
    assert_eq!(event["effective_model"], "standard");
    assert_eq!(event["prompt"], "");
    assert_eq!(event["response"], "");
}

#[tokio::test]
async fn turn_without_reserve_fields_emits_a_zero_usage_event_without_settlement() {
    let fx = OrphanFixture::new().await;
    let turn = fx
        .running(
            TENANT_A,
            RunningSeed {
                with_reserve: false,
                ..RunningSeed::stale()
            },
        )
        .await;

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(outcome, OrphanOutcome::Finalized(OrphanSettlement::Skipped));
    assert_eq!(fx.turn(turn.id).await.state, "failed");
    assert!(fx.quota_rows(USER_A1).await.is_empty(), "no settlement");
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    let usage = payload_of(&usage, turn.id);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = usage[0];
    assert_eq!(event["billing_outcome"], "aborted");
    assert_eq!(event["settlement_method"], "estimated");
    assert_eq!(event["actual_credits_micro"], 0);
    assert_eq!(event["usage"], serde_json::Value::Null);
    assert_eq!(event["effective_model"], "");
    assert_eq!(event["selected_model"], "");
    assert_eq!(event["policy_version_applied"], 0);
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(payload_of(&audit, turn.id).len(), 1);
}

#[tokio::test]
async fn turn_without_requester_falls_back_to_the_chat_owner_and_skips_settlement() {
    let fx = OrphanFixture::new().await;
    let turn = fx
        .running(
            TENANT_A,
            RunningSeed {
                requester: None,
                ..RunningSeed::stale()
            },
        )
        .await;

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(outcome, OrphanOutcome::Finalized(OrphanSettlement::Skipped));
    assert!(fx.quota_rows(USER_A1).await.is_empty());
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    let usage = payload_of(&usage, turn.id);
    assert_eq!(usage[0]["user_id"], serde_json::json!(USER_A1));
    assert_eq!(usage[0]["actual_credits_micro"], 0);
}

// ── CAS guard ────────────────────────────────────────────────────────────

#[tokio::test]
async fn recent_progress_is_neither_a_candidate_nor_finalized() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::progressing()).await;

    assert!(fx.finalizer.candidates().await.unwrap().is_empty());
    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(outcome, OrphanOutcome::Skipped);
    let row = fx.turn(turn.id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.error_code, None);
    assert_eq!(row.completed_at, None);
}

#[tokio::test]
async fn progress_after_the_scan_wins_over_the_candidate() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    assert_eq!(fx.finalizer.candidates().await.unwrap(), [turn]);

    fx.progress(turn.id, 0).await;
    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(outcome, OrphanOutcome::Skipped);
    assert_eq!(fx.turn(turn.id).await.state, "running");
    assert!(
        fx.quota_rows(USER_A1).await.is_empty(),
        "a CAS loser never settles"
    );
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(fx.store.delivered(&fx.queues.usage).is_empty());
    assert!(fx.store.delivered(&fx.queues.audit).is_empty());
}

#[tokio::test]
async fn null_progress_falls_back_to_started_at() {
    let fx = OrphanFixture::new().await;
    let old = fx
        .running(
            TENANT_A,
            RunningSeed {
                progress_ms: None,
                ..RunningSeed::stale()
            },
        )
        .await;
    let recent = fx
        .running(
            TENANT_A,
            RunningSeed {
                started_ms: 0,
                progress_ms: None,
                ..RunningSeed::stale()
            },
        )
        .await;

    assert_eq!(fx.finalizer.candidates().await.unwrap(), [old]);
    assert_eq!(
        fx.finalizer.finalize_orphan(recent).await.unwrap(),
        OrphanOutcome::Skipped
    );
    assert!(matches!(
        fx.finalizer.finalize_orphan(old).await.unwrap(),
        OrphanOutcome::Finalized(_)
    ));
    assert_eq!(fx.turn(recent.id).await.state, "running");
    assert_eq!(fx.turn(old.id).await.state, "failed");
}

#[tokio::test]
async fn deleted_and_terminal_turns_are_skipped() {
    let fx = OrphanFixture::new().await;
    let deleted = fx
        .running(
            TENANT_A,
            RunningSeed {
                deleted: true,
                ..RunningSeed::stale()
            },
        )
        .await;
    let finished = fx.running(TENANT_A, RunningSeed::stale()).await;
    assert!(matches!(
        fx.finalizer.finalize_orphan(finished).await.unwrap(),
        OrphanOutcome::Finalized(_)
    ));

    assert!(fx.finalizer.candidates().await.unwrap().is_empty());
    assert_eq!(
        fx.finalizer.finalize_orphan(deleted).await.unwrap(),
        OrphanOutcome::Skipped
    );
    assert_eq!(
        fx.finalizer.finalize_orphan(finished).await.unwrap(),
        OrphanOutcome::Skipped,
        "a finalized orphan is never finalized twice"
    );
    let row = fx.turn(deleted.id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.error_code, None);
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage.len(), 1, "one usage event per finalized turn");
}

#[tokio::test]
async fn settlement_failure_rolls_back_and_keeps_the_turn_running() {
    let fx = OrphanFixture::new().await;
    let turn = fx.running(TENANT_A, RunningSeed::stale()).await;
    fx.policy.set_failing(true);

    assert!(fx.finalizer.finalize_orphan(turn).await.is_err());

    let row = fx.turn(turn.id).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.error_code, None);
    fx.policy.set_failing(false);
    assert!(matches!(
        fx.finalizer.finalize_orphan(turn).await.unwrap(),
        OrphanOutcome::Finalized(_)
    ));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage.len(), 1);
}

// ── Unpriceable turns ────────────────────────────────────────────────────

/// Asserts that `turn` was finalized with its 5 000 reserve released from
/// bucket `total`, no charge and one zero usage event carrying the persisted
/// model and policy version.
async fn assert_released(fx: &OrphanFixture, turn: OrphanCandidate, seed: RunningSeed) {
    let row = fx.turn(turn.id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some(ORPHAN_TIMEOUT));
    assert_reserve_released(fx).await;
    assert_zero_usage_event(fx, turn, seed).await;
    assert!(
        fx.finalizer.candidates().await.unwrap().is_empty(),
        "a released turn is not a candidate of the next scan"
    );
}

/// Bucket `total` of both periods: reserve released, nothing spent.
async fn assert_reserve_released(fx: &OrphanFixture) {
    let rows = fx.quota_rows(USER_A1).await;
    let totals: Vec<_> = rows.iter().filter(|r| r.bucket == "total").collect();
    assert_eq!(totals.len(), 2, "{rows:?}");
    for row in totals {
        assert_eq!(
            (
                row.reserved_credits_micro,
                row.spent_credits_micro,
                row.calls
            ),
            (0, 0, 1),
            "{row:?}"
        );
        assert_eq!((row.web_search_calls, row.code_interpreter_calls), (0, 0));
    }
}

/// One zero usage event and one audit event for `turn`.
async fn assert_zero_usage_event(fx: &OrphanFixture, turn: OrphanCandidate, seed: RunningSeed) {
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    let usage = payload_of(&usage, turn.id);
    assert_eq!(usage.len(), 1, "{usage:?}");
    let event = usage[0];
    assert_eq!(event["billing_outcome"], "aborted");
    assert_eq!(event["settlement_method"], "estimated");
    assert_eq!(event["actual_credits_micro"], 0);
    assert_eq!(event["effective_model"], seed.model);
    assert_eq!(event["policy_version_applied"], seed.policy_version);
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(payload_of(&audit, turn.id).len(), 1);
}

#[tokio::test]
async fn unknown_policy_version_releases_the_reserve_and_finalizes() {
    let fx = OrphanFixture::new().await;
    let seed = RunningSeed {
        policy_version: 99,
        ..RunningSeed::stale()
    };
    let turn = fx.running(TENANT_A, seed).await;
    fx.reserve_total(TENANT_A, seed.started_ms, 5_000).await;

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(
        outcome,
        OrphanOutcome::Finalized(OrphanSettlement::ReserveReleased {
            released_credits_micro: 5_000
        })
    );
    assert_released(&fx, turn, seed).await;
}

#[tokio::test]
async fn effective_model_missing_from_the_snapshot_releases_the_reserve() {
    let fx = OrphanFixture::new().await;
    let seed = RunningSeed {
        model: "retired",
        ..RunningSeed::stale()
    };
    let turn = fx.running(TENANT_A, seed).await;
    fx.reserve_total(TENANT_A, seed.started_ms, 5_000).await;

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(
        outcome,
        OrphanOutcome::Finalized(OrphanSettlement::ReserveReleased {
            released_credits_micro: 5_000
        })
    );
    assert_released(&fx, turn, seed).await;
}

#[tokio::test]
async fn premium_turn_whose_charge_fails_releases_the_total_and_premium_reserves() {
    let fx = OrphanFixture::new().await;
    // Policy version 2: `premium` is in the snapshot, but its zero input
    // multiplier makes the credit computation fail (permanent, tier known).
    let mut catalog = test_catalog();
    for entry in &mut catalog {
        if entry.id == "premium" {
            entry.input_tokens_credit_multiplier_micro = 0;
        }
    }
    fx.policy.publish(catalog, KillSwitches::default());
    let seed = RunningSeed {
        policy_version: 2,
        model: "premium",
        ..RunningSeed::stale()
    };
    let turn = fx.running(TENANT_A, seed).await;
    fx.reserve_buckets(
        TENANT_A,
        seed.started_ms,
        5_000,
        &[QuotaBucket::Total, QuotaBucket::Premium],
    )
    .await;

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert_eq!(
        outcome,
        OrphanOutcome::Finalized(OrphanSettlement::ReserveReleased {
            released_credits_micro: 5_000
        })
    );
    let rows = fx.quota_rows(USER_A1).await;
    assert_eq!(rows.len(), 4, "{rows:?}");
    for bucket in ["total", "tier:premium"] {
        let of_bucket: Vec<_> = rows.iter().filter(|r| r.bucket == bucket).collect();
        assert_eq!(of_bucket.len(), 2, "{rows:?}");
        for row in of_bucket {
            assert_eq!(
                (row.reserved_credits_micro, row.spent_credits_micro),
                (0, 0),
                "{bucket} reserve released without a charge: {row:?}"
            );
        }
    }
    assert_zero_usage_event(&fx, turn, seed).await;
}

#[tokio::test]
async fn unpriceable_turns_never_starve_a_newer_orphan() {
    let fx = OrphanFixture::new().await;
    let mut broken = Vec::new();
    for index in 0..ORPHAN_SCAN_LIMIT {
        let seed = RunningSeed {
            started_ms: STALE_MS - 1_000 * (200 - i64::try_from(index).unwrap()),
            policy_version: 99,
            ..RunningSeed::stale()
        };
        broken.push(fx.running(TENANT_A, seed).await);
    }
    let newer = fx.running(TENANT_A, RunningSeed::stale()).await;

    let first = fx.finalizer.candidates().await.unwrap();
    assert_eq!(first, broken, "the oldest 100 fill the first scan");
    for candidate in first {
        assert!(matches!(
            fx.finalizer.finalize_orphan(candidate).await.unwrap(),
            OrphanOutcome::Finalized(OrphanSettlement::ReserveReleased { .. })
        ));
    }

    assert_eq!(fx.finalizer.candidates().await.unwrap(), [newer]);
    assert_eq!(
        fx.finalizer.finalize_orphan(newer).await.unwrap(),
        OrphanOutcome::Finalized(OrphanSettlement::Estimated {
            committed_credits_micro: estimated_charge()
        })
    );
    assert_eq!(fx.turn(newer.id).await.state, "failed");
}

// ── Quota period boundary ────────────────────────────────────────────────

#[tokio::test]
async fn orphan_settlement_targets_the_rows_reserved_at_a_day_and_month_boundary() {
    let fx = OrphanFixture::new().await;
    let owner = QuotaOwner {
        tenant_id: TENANT_A,
        user_id: USER_A1,
    };
    // The preflight runs one millisecond before a day and month boundary;
    // the turn row is written after midnight.
    let preflight_at = datetime!(2024-01-31 23:59:59.999 UTC);
    let policy = fx.quota.load_policy(USER_A1).await.unwrap();
    let decision = fx
        .quota
        .preflight(PreflightInput {
            owner,
            selected_model: "standard",
            policy_version: policy.policy_version,
            snapshot: &policy.snapshot,
            user_limits: &policy.user_limits,
            request: PreflightRequest {
                message_utf8_bytes: 100,
                ..PreflightRequest::default()
            },
            prior_context_tokens: 0,
            now: preflight_at,
        })
        .await
        .unwrap();
    assert_eq!(decision.at, preflight_at, "the turn's started_at");
    assert_eq!(decision.periods, PeriodStarts::at(preflight_at));
    assert_ne!(
        decision.periods,
        PeriodStarts::at(preflight_at + time::Duration::milliseconds(1)),
        "an insert-time started_at would target other bucket rows"
    );
    let quota = Arc::clone(&fx.quota);
    let reserve = decision.clone();
    fx.store
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move { quota.reserve_in_tx(tx, &reserve, owner).await })
        })
        .await
        .unwrap();
    let turn = fx
        .running_with(
            TENANT_A,
            RunningSeed::stale(),
            decision.at,
            decision.reserved_credits_micro,
        )
        .await;
    assert_eq!(fx.turn(turn.id).await.started_at.at(), preflight_at);

    let outcome = fx.finalizer.finalize_orphan(turn).await.unwrap();

    assert!(
        matches!(
            outcome,
            OrphanOutcome::Finalized(OrphanSettlement::Estimated { .. })
        ),
        "{outcome:?}"
    );
    // The settlement hit exactly the reserved rows: no other period row was
    // created and every reserve was released.
    let rows = fx.quota_rows(USER_A1).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    for row in &rows {
        assert_eq!(row.bucket, "total");
        assert_eq!(row.reserved_credits_micro, 0, "{row:?}");
        assert_eq!(row.calls, 1, "{row:?}");
        assert!(
            row.period_start == datetime!(2024-01-31 00:00 UTC).date()
                || row.period_start == datetime!(2024-01-01 00:00 UTC).date(),
            "{row:?}"
        );
    }
}

// ── Scan ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn scan_crosses_tenants_oldest_first_and_takes_at_most_100() {
    let fx = OrphanFixture::new().await;
    let mut seeded = Vec::new();
    for index in 0..101_i64 {
        let tenant = if index % 2 == 0 { TENANT_A } else { TENANT_B };
        let seed = RunningSeed {
            started_ms: STALE_MS - 1_000 * (101 - index),
            ..RunningSeed::stale()
        };
        seeded.push(fx.running(tenant, seed).await);
    }

    let candidates = fx.finalizer.candidates().await.unwrap();

    assert_eq!(candidates.len(), 100);
    assert_eq!(candidates, seeded[..100]);
    assert!(candidates.iter().any(|c| c.tenant_id == TENANT_B));
}

#[test]
fn settlement_input_requires_every_reserve_column_and_the_requester() {
    let started = at(0);
    let row = FinalizedTurnRecord {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_user_id: Some(USER_A2),
        effective_model: Some("standard".to_owned()),
        policy_version_applied: Some(7),
        reserve_tokens: Some(RESERVE_TOKENS),
        max_output_tokens_applied: Some(MAX_OUTPUT_TOKENS),
        reserved_credits_micro: Some(9),
        minimal_generation_floor_applied: Some(FLOOR),
        started_at: started,
        counts: crate::domain::ports::ToolCallCounts {
            web_search: 1,
            code_interpreter: 2,
            file_search: 3,
        },
    };
    let input = settlement_input(&row).unwrap();
    assert_eq!(input.owner.user_id, USER_A2);
    assert_eq!(input.policy_version_applied, 7);
    assert_eq!(input.periods, PeriodStarts::at(started));
    assert_eq!(input.terminal, TerminalState::Failed);
    assert_eq!(input.error_code.as_deref(), Some(ORPHAN_TIMEOUT));
    assert_eq!(input.usage, None);
    assert_eq!(
        (input.web_search_calls, input.code_interpreter_calls),
        (1, 2)
    );

    let missing = [
        FinalizedTurnRecord {
            requester_user_id: None,
            ..row.clone()
        },
        FinalizedTurnRecord {
            effective_model: None,
            ..row.clone()
        },
        FinalizedTurnRecord {
            policy_version_applied: Some(-1),
            ..row.clone()
        },
        FinalizedTurnRecord {
            reserve_tokens: None,
            ..row.clone()
        },
        FinalizedTurnRecord {
            minimal_generation_floor_applied: None,
            ..row
        },
    ];
    for row in missing {
        assert_eq!(settlement_input(&row), None, "{row:?}");
    }
}
