#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    BillingOutcome, KillSwitches, ModelCatalogEntry, PolicySnapshot, SettlementMethod, UsageEvent,
    UserLimits,
};
use parking_lot::Mutex;
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{
    LeaderElector as _, OrphanCandidate, OrphanRepository, OrphanReserve, OrphanWatchdog,
    OrphanWatchdogParts, ScanReport,
};
use crate::domain::error::DomainError;
use crate::domain::metrics::Metrics;
use crate::domain::ports::{ModelPolicyGateway, UsagePublication};
use crate::domain::stream::{
    FinalizeCommand, FinalizeOutcome, ReserveCommand, StoredAssistantMessage, TurnRepository,
};
use crate::domain::turn::{TurnRecord, TurnState};

const NOW: OffsetDateTime = datetime!(2026-01-01 00:10 UTC);
const STARTED: OffsetDateTime = datetime!(2025-12-31 23:50 UTC);

#[derive(Default)]
struct Calls(Mutex<Vec<String>>);

impl Calls {
    fn push(&self, call: impl Into<String>) {
        self.0.lock().push(call.into());
    }
    fn all(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

impl Metrics for Calls {
    fn stream_started(&self, _: &str, _: &str) {}
    fn stream_completed(&self, _: &str, _: &str) {}
    fn stream_failed(&self, _: &str, _: &str, _: &str) {}
    fn stream_incomplete(&self, _: &str, _: &str, _: &str) {}
    fn stream_disconnected(&self, _: &str) {}
    fn active_streams(&self, _: i64) {}
    fn cancel_requested(&self, _: &str) {}
    fn cancel_effective(&self, _: &str) {}
    fn time_to_abort_ms(&self, _: &str, _: f64) {}
    fn streams_aborted(&self, trigger: &str) {
        self.push(format!("streams_aborted:{trigger}"));
    }
    fn quota_preflight(&self, _: &str, _: &str, _: &str) {}
    fn quota_reserve(&self, _: &str) {}
    fn quota_commit(&self, _: &str) {}
    fn quota_overshoot(&self, _: &str) {}
    fn quota_estimated_tokens(&self, _: f64) {}
    fn quota_actual_tokens(&self, _: f64) {}
    fn code_interpreter_calls(&self, _: &str, _: u64) {}
    fn finalization_latency_ms(&self, _: f64) {}
    fn ttft_provider_ms(&self, _: &str, _: &str, _: f64) {}
    fn stream_total_latency_ms(&self, _: &str, _: &str, _: f64) {}
    fn orphan_detected(&self, reason: &str) {
        self.push(format!("detected:{reason}"));
    }
    fn orphan_finalized(&self, reason: &str) {
        self.push(format!("finalized:{reason}"));
    }
    fn orphan_scan_duration_seconds(&self, _: f64) {
        self.push("scan_duration");
    }
    fn audit_emit(&self, _: &str) {}
}

struct Candidates {
    list: Vec<OrphanCandidate>,
    asked: Mutex<Vec<(OffsetDateTime, u64)>>,
}

#[async_trait]
impl OrphanRepository for Candidates {
    async fn stale_turns(
        &self,
        cutoff: OffsetDateTime,
        limit: u64,
        attempted: &[Uuid],
    ) -> Result<Vec<OrphanCandidate>, DomainError> {
        self.asked.lock().push((cutoff, limit));
        Ok(self
            .list
            .iter()
            .filter(|turn| !attempted.contains(&turn.turn_id))
            .take(usize::try_from(limit).unwrap())
            .cloned()
            .collect())
    }
}

struct Turns {
    outcome: FinalizeOutcome,
    commands: Mutex<Vec<(AccessScope, FinalizeCommand)>>,
}

#[async_trait]
impl TurnRepository for Turns {
    async fn find_by_request(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        Ok(None)
    }
    async fn find_running(
        &self,
        _: &AccessScope,
        _: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        Ok(None)
    }
    async fn assistant_message(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        Ok(None)
    }
    async fn reserve(&self, _: &AccessScope, _: ReserveCommand) -> Result<(), DomainError> {
        Ok(())
    }
    async fn finalize(
        &self,
        scope: &AccessScope,
        command: FinalizeCommand,
    ) -> Result<FinalizeOutcome, DomainError> {
        self.commands.lock().push((scope.clone(), command));
        Ok(self.outcome)
    }
    async fn touch_progress(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Ok(())
    }
    async fn record_tool_completed(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

fn entry(id: &str, tier: &str) -> ModelCatalogEntry {
    priced_entry(id, tier, 1_000_000)
}

fn priced_entry(id: &str, tier: &str, input_multiplier_micro: u64) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id, "provider_model_id": id, "display_name": id, "provider_id": "p",
        "provider_display_name": "P", "tier": tier, "enabled": true,
        "context_window": 128_000, "max_output_tokens": 4096, "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": input_multiplier_micro,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "", "available_from": "1970-01-01T00:00:00Z", "max_file_size_mb": 25,
            "api_params": { "stop": [] },
            "features": { "streaming": true, "structured_output": true },
            "tool_support": { "web_search": true, "file_search": true,
                "image_generation": false, "code_interpreter": true, "mcp": false },
            "supported_endpoints": {}
        },
        "preference": { "is_default": false, "sort_order": 0 }
    }))
    .unwrap()
}

struct Policy {
    snapshot: Option<PolicySnapshot>,
    /// The plugin does not know the version: a deterministic failure, not an outage.
    version_unknown: bool,
    /// The current snapshot, when it differs from what `snapshot_at` serves.
    current: Option<PolicySnapshot>,
}

#[async_trait]
impl ModelPolicyGateway for Policy {
    async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        if let Some(current) = &self.current {
            return Ok(current.clone());
        }
        self.snapshot_at(user_id, 1).await
    }
    async fn snapshot_at(&self, _: Uuid, _: i64) -> Result<PolicySnapshot, DomainError> {
        if self.version_unknown {
            return Err(DomainError::internal("policy version 1 not found"));
        }
        self.snapshot
            .clone()
            .ok_or_else(|| DomainError::PolicyUnavailable {
                diagnostic: "unavailable".to_owned(),
            })
    }
    async fn user_limits(&self, _: Uuid, _: i64) -> Result<UserLimits, DomainError> {
        Err(DomainError::internal("not used"))
    }
    async fn publish_usage(&self, _: UsageEvent) -> UsagePublication {
        UsagePublication::Published
    }
}

fn snapshot() -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![entry("premium", "premium"), entry("standard", "standard")],
        kill_switches: KillSwitches::default(),
    }
}

fn candidate(reserve: Option<OrphanReserve>) -> OrphanCandidate {
    OrphanCandidate {
        turn_id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        user_id: Some(Uuid::new_v4()),
        started_at: STARTED,
        // 10 minutes without progress: past the cutoff, inside the transient retry window.
        last_progress_at: Some(datetime!(2026-01-01 00:00 UTC)),
        web_search_calls: 1,
        code_interpreter_calls: 0,
        file_search_calls: 2,
        reserve,
    }
}

fn reserve(model: &str) -> OrphanReserve {
    OrphanReserve {
        reserve_tokens: 4209,
        max_output_tokens_applied: 4096,
        reserved_credits_micro: 12_401,
        policy_version: 1,
        effective_model: model.to_owned(),
        minimal_generation_floor_applied: 50,
    }
}

struct Fixture {
    watchdog: OrphanWatchdog,
    candidates: Arc<Candidates>,
    turns: Arc<Turns>,
    calls: Arc<Calls>,
}

fn fixture(
    list: Vec<OrphanCandidate>,
    outcome: FinalizeOutcome,
    policy: Option<PolicySnapshot>,
) -> Fixture {
    fixture_every(list, outcome, policy, Duration::from_secs(60))
}

fn fixture_every(
    list: Vec<OrphanCandidate>,
    outcome: FinalizeOutcome,
    policy: Option<PolicySnapshot>,
    scan_interval: Duration,
) -> Fixture {
    build_fixture(
        list,
        outcome,
        Policy {
            snapshot: policy,
            version_unknown: false,
            current: None,
        },
        scan_interval,
    )
}

fn build_fixture(
    list: Vec<OrphanCandidate>,
    outcome: FinalizeOutcome,
    policy: Policy,
    scan_interval: Duration,
) -> Fixture {
    let candidates = Arc::new(Candidates {
        list,
        asked: Mutex::new(Vec::new()),
    });
    let turns = Arc::new(Turns {
        outcome,
        commands: Mutex::new(Vec::new()),
    });
    let calls = Arc::new(Calls::default());
    let watchdog = OrphanWatchdog::new(OrphanWatchdogParts {
        candidates: candidates.clone(),
        turns: turns.clone(),
        policy: Arc::new(policy),
        metrics: calls.clone(),
        timeout: Duration::from_secs(300),
        scan_interval,
    });
    Fixture {
        watchdog,
        candidates,
        turns,
        calls,
    }
}

#[tokio::test]
async fn the_scan_asks_for_at_most_one_hundred_turns_older_than_the_cutoff() {
    let fx = fixture(Vec::new(), FinalizeOutcome::Committed, Some(snapshot()));

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report, ScanReport::default());
    assert_eq!(
        *fx.candidates.asked.lock(),
        [(datetime!(2026-01-01 00:05 UTC), 100)]
    );
    assert_eq!(fx.calls.all(), ["scan_duration"]);
}

#[tokio::test]
async fn a_reserved_turn_is_finalized_with_the_estimated_settlement_of_its_started_periods() {
    let turn = candidate(Some(reserve("premium")));
    let fx = fixture(
        vec![turn.clone()],
        FinalizeOutcome::Committed,
        Some(snapshot()),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );
    let commands = fx.turns.commands.lock();
    let (scope, command) = &commands[0];
    assert_eq!(*scope, AccessScope::for_tenant(turn.tenant_id));
    assert_eq!(command.state, TurnState::Failed);
    assert_eq!(command.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(command.orphan_cutoff, Some(datetime!(2026-01-01 00:05 UTC)));
    assert!(command.book_quota);
    assert_eq!(command.premium, Some(true));
    assert!(command.assistant_message.is_none());
    assert_eq!(command.reserved_credits_micro, 12_401);
    assert_eq!(command.settlement.method, SettlementMethod::Estimated);
    assert_eq!(command.settlement.committed_credits_micro, 263);
    assert_eq!(
        command.periods[0].start,
        datetime!(2025-12-31 0:00 UTC).date()
    );
    assert_eq!(
        command.periods[1].start,
        datetime!(2025-12-01 0:00 UTC).date()
    );
    assert_eq!(command.now, NOW);
    assert_eq!(
        (
            command.tool_counts.web_search,
            command.tool_counts.file_search
        ),
        (1, 2)
    );
    let usage = &command.usage_event;
    assert_eq!(usage.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(usage.settlement_method, SettlementMethod::Estimated);
    assert_eq!(usage.selected_model, "premium");
    assert_eq!(usage.effective_model, "premium");
    assert_eq!(usage.actual_credits_micro, 263);
    assert!(usage.usage.is_none());
    assert_eq!(usage.policy_version_applied, 1);
    let audit = &command.audit_event;
    assert_eq!(audit.policy_decisions.quota.decision, "unknown");
    assert_eq!(audit.error_code.as_deref(), Some("orphan_timeout"));
    assert!(audit.trace_id.is_none());
    assert_eq!(
        fx.calls.all(),
        [
            "detected:stale_progress",
            "finalized:stale_progress",
            "streams_aborted:orphan_timeout",
            "scan_duration"
        ]
    );
}

#[tokio::test]
async fn a_standard_turn_is_not_booked_in_the_premium_bucket() {
    let fx = fixture(
        vec![candidate(Some(reserve("standard")))],
        FinalizeOutcome::Committed,
        Some(snapshot()),
    );

    fx.watchdog.scan_once(NOW).await;

    assert_eq!(fx.turns.commands.lock()[0].1.premium, Some(false));
}

#[tokio::test]
async fn a_turn_without_reserve_fields_books_no_quota_and_still_emits_the_events() {
    let fx = fixture(
        vec![candidate(None)],
        FinalizeOutcome::Committed,
        Some(snapshot()),
    );

    fx.watchdog.scan_once(NOW).await;

    let commands = fx.turns.commands.lock();
    let command = &commands[0].1;
    assert!(!command.book_quota);
    assert_eq!(command.settlement.committed_credits_micro, 0);
    assert_eq!(command.reserved_credits_micro, 0);
    assert_eq!(command.usage_event.effective_model, "");
    assert_eq!(command.usage_event.selected_model, "");
    assert_eq!(command.usage_event.policy_version_applied, 0);
    assert_eq!(command.usage_event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(command.audit_event.effective_model, "");
    assert!(
        command.require_unreserved,
        "the CAS must lose if a reserve is filled after the read of NULL reserve fields"
    );
}

#[tokio::test]
async fn a_reserved_turn_does_not_require_an_unreserved_row_in_the_cas() {
    let fx = fixture(
        vec![candidate(Some(reserve("premium")))],
        FinalizeOutcome::Committed,
        Some(snapshot()),
    );

    fx.watchdog.scan_once(NOW).await;

    assert!(!fx.turns.commands.lock()[0].1.require_unreserved);
}

#[tokio::test]
async fn a_lost_cas_records_no_finalization_metric() {
    let fx = fixture(
        vec![candidate(Some(reserve("premium")))],
        FinalizeOutcome::Lost,
        Some(snapshot()),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 0
        }
    );
    assert_eq!(fx.calls.all(), ["detected:stale_progress", "scan_duration"]);
}

#[tokio::test]
async fn a_turn_whose_policy_snapshot_is_unavailable_is_left_running_for_the_next_scan() {
    let fx = fixture(
        vec![candidate(Some(reserve("premium")))],
        FinalizeOutcome::Committed,
        None,
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 0
        }
    );
    assert!(fx.turns.commands.lock().is_empty());
}

/// The reserve of a turn whose settlement basis can never be resolved is released without a
/// charge and the turn is still finalized.
fn assert_released(fx: &Fixture, turn: &OrphanCandidate) {
    let commands = fx.turns.commands.lock();
    assert_eq!(commands.len(), 1, "the turn is finalized");
    let command = &commands[0].1;
    assert_eq!(command.state, TurnState::Failed);
    assert_eq!(command.error_code.as_deref(), Some("orphan_timeout"));
    assert!(command.book_quota);
    assert_eq!(command.reserved_credits_micro, 12_401);
    assert_eq!(command.settlement.method, SettlementMethod::Released);
    assert_eq!(command.settlement.committed_credits_micro, 0);
    let usage = &command.usage_event;
    assert_eq!(usage.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(usage.settlement_method, SettlementMethod::Released);
    assert_eq!(usage.actual_credits_micro, 0);
    assert_eq!(usage.user_id, turn.user_id);
    assert_eq!(
        command.audit_event.error_code.as_deref(),
        Some("orphan_timeout")
    );
    assert_eq!(
        fx.calls.all(),
        [
            "detected:stale_progress",
            "finalized:stale_progress",
            "streams_aborted:orphan_timeout",
            "scan_duration"
        ]
    );
}

#[tokio::test]
async fn a_turn_whose_model_is_missing_from_its_snapshot_is_finalized_with_a_released_reserve() {
    let turn = candidate(Some(reserve("gone")));
    let fx = fixture(
        vec![turn.clone()],
        FinalizeOutcome::Committed,
        Some(snapshot()),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 1
        }
    );
    assert_released(&fx, &turn);
    let commands = fx.turns.commands.lock();
    let command = &commands[0].1;
    assert_eq!(
        command.premium, None,
        "the tier of an unknown model is unknown"
    );
    assert_eq!(command.usage_event.effective_model, "gone");
    assert_eq!(command.usage_event.policy_version_applied, 1);
}

#[tokio::test]
async fn a_turn_whose_policy_version_is_unknown_is_finalized_with_a_released_reserve() {
    let turn = candidate(Some(reserve("premium")));
    let fx = build_fixture(
        vec![turn.clone()],
        FinalizeOutcome::Committed,
        Policy {
            snapshot: Some(snapshot()),
            version_unknown: true,
            current: None,
        },
        Duration::from_secs(60),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report.finalized, 1);
    assert_released(&fx, &turn);
}

#[tokio::test]
async fn the_tier_of_a_turn_whose_version_is_unknown_comes_from_the_current_policy() {
    for (model, premium) in [("premium", true), ("standard", false)] {
        let turn = candidate(Some(reserve(model)));
        let fx = build_fixture(
            vec![turn.clone()],
            FinalizeOutcome::Committed,
            Policy {
                snapshot: None,
                version_unknown: true,
                current: Some(snapshot()),
            },
            Duration::from_secs(60),
        );

        let report = fx.watchdog.scan_once(NOW).await;

        assert_eq!(report.finalized, 1);
        assert_released(&fx, &turn);
        let commands = fx.turns.commands.lock();
        assert_eq!(commands[0].1.premium, Some(premium), "{model}");
    }
}

#[tokio::test]
async fn a_turn_whose_tier_cannot_be_resolved_anywhere_has_an_unknown_tier() {
    let turn = candidate(Some(reserve("retired")));
    let fx = build_fixture(
        vec![turn.clone()],
        FinalizeOutcome::Committed,
        Policy {
            snapshot: None,
            version_unknown: true,
            current: Some(snapshot()),
        },
        Duration::from_secs(60),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report.finalized, 1);
    assert_released(&fx, &turn);
    assert_eq!(fx.turns.commands.lock()[0].1.premium, None);
}

#[tokio::test]
async fn a_turn_is_retried_while_the_policy_is_unavailable_inside_the_retry_window() {
    let turn = candidate(Some(reserve("premium")));
    let fx = fixture(vec![turn], FinalizeOutcome::Committed, None);

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 1,
            finalized: 0
        }
    );
    assert!(
        fx.turns.commands.lock().is_empty(),
        "left running for the next scan"
    );
}

#[tokio::test]
async fn a_turn_stale_past_the_retry_window_is_finalized_released_though_the_policy_stays_unavailable()
 {
    // timeout 300 s: the window is the cutoff plus 3 timeouts, 1200 s of staleness.
    let mut turn = candidate(Some(reserve("premium")));
    turn.last_progress_at = Some(NOW - time::Duration::seconds(1_200));
    let fx = fixture(vec![turn.clone()], FinalizeOutcome::Committed, None);

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report.finalized, 1);
    assert_released(&fx, &turn);
    assert_eq!(fx.turns.commands.lock()[0].1.premium, None);
}

#[tokio::test]
async fn without_last_progress_the_retry_window_counts_from_the_start() {
    let mut turn = candidate(Some(reserve("premium")));
    turn.last_progress_at = None;
    turn.started_at = NOW - time::Duration::seconds(1_200);
    let fx = fixture(vec![turn.clone()], FinalizeOutcome::Committed, None);

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report.finalized, 1);
    assert_released(&fx, &turn);
}

#[tokio::test]
async fn a_turn_whose_settlement_is_refused_is_finalized_with_a_released_reserve() {
    let refused = PolicySnapshot {
        policy_version: 1,
        model_catalog: vec![priced_entry("premium", "premium", u64::MAX)],
        kill_switches: KillSwitches::default(),
    };
    let turn = candidate(Some(reserve("premium")));
    let fx = fixture(
        vec![turn.clone()],
        FinalizeOutcome::Committed,
        Some(refused),
    );

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(report.finalized, 1);
    assert_released(&fx, &turn);
    let commands = fx.turns.commands.lock();
    assert_eq!(commands[0].1.premium, Some(true), "the tier is known");
}

#[tokio::test]
async fn a_turn_without_a_requester_books_nothing_and_sends_no_user() {
    for reserve in [Some(reserve("premium")), None] {
        let mut turn = candidate(reserve);
        turn.user_id = None;
        let fx = fixture(vec![turn], FinalizeOutcome::Committed, Some(snapshot()));

        let report = fx.watchdog.scan_once(NOW).await;

        assert_eq!(report.finalized, 1);
        let commands = fx.turns.commands.lock();
        let command = &commands[0].1;
        assert!(!command.book_quota);
        assert_eq!(command.reserved_credits_micro, 0);
        assert_eq!(command.settlement.committed_credits_micro, 0);
        assert_eq!(command.usage_event.user_id, None);
        assert_eq!(command.usage_event.billing_outcome, BillingOutcome::Aborted);
        assert_eq!(command.usage_event.effective_model, "");
    }
}

#[tokio::test]
async fn a_transiently_failing_turn_does_not_block_the_turns_behind_it() {
    let mut list: Vec<OrphanCandidate> = (0..101)
        .map(|_| candidate(Some(reserve("premium"))))
        .collect();
    let good = candidate(None);
    list.push(good.clone());
    let fx = fixture(list, FinalizeOutcome::Committed, None);

    let report = fx.watchdog.scan_once(NOW).await;

    assert_eq!(
        report,
        ScanReport {
            detected: 102,
            finalized: 1
        }
    );
    let commands = fx.turns.commands.lock();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].1.turn_id, good.turn_id);
    drop(commands);
    let asked = fx.candidates.asked.lock();
    assert!(asked.iter().all(|(_, limit)| *limit <= 100));
}

struct Never;

impl super::LeaderElector for Never {
    fn is_leader(&self) -> bool {
        false
    }
}

async fn run_for(fx: &Fixture, elector: Arc<dyn super::LeaderElector>, millis: u64) {
    let cancel = tokio_util::sync::CancellationToken::new();
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(millis)).await;
        stop.cancel();
    });
    fx.watchdog.run(elector, cancel).await;
}

#[tokio::test]
async fn the_leader_scans_every_interval_until_cancelled() {
    let fx = fixture_every(
        Vec::new(),
        FinalizeOutcome::Committed,
        Some(snapshot()),
        Duration::from_millis(10),
    );

    run_for(&fx, Arc::new(super::AlwaysLeader), 200).await;

    assert!(
        fx.candidates.asked.lock().len() >= 2,
        "the watchdog scanned repeatedly"
    );
}

#[tokio::test]
async fn a_process_that_is_not_the_leader_never_scans() {
    let fx = fixture_every(
        Vec::new(),
        FinalizeOutcome::Committed,
        Some(snapshot()),
        Duration::from_millis(10),
    );

    run_for(&fx, Arc::new(Never), 100).await;

    assert!(fx.candidates.asked.lock().is_empty());
}

#[test]
fn the_noop_elector_always_reports_leadership() {
    assert!(super::AlwaysLeader.is_leader());
}
