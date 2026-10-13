#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2, 3, 4, 5 and 6 through the `QuotaService` over fakes: the
//! preflight orchestration (kill switch, tool quotas, metrics, policy and read
//! failures), the reserve and finalization pass-through with their metrics, the
//! status and the warnings.

use mini_chat_sdk::{BillingOutcome, KillSwitches, ModelTier, SettlementMethod, TerminalState};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use super::{QuotaService, QuotaServiceConfig};
use crate::config::QuotaConfig;
use crate::domain::error::{DisabledFeature, DomainError, QuotaSubject};
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult, FinalizeWon};
use crate::domain::service::quota::policy::SettlementSnapshot;
use crate::domain::service::quota::ports::PreflightLabel;
use crate::domain::service::quota::preflight::{DowngradeReason, QuotaDecision};
use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest, ReservedColumns};
use crate::domain::service::quota::test_support::{
    FakePolicy, FakeRepo, Fixture, Metric, NO_SWITCHES, PolicyCall, PolicyStep, RepoCall,
    default_config, design_catalog, design_fixture, design_limits, fixed_clock, fixture as world,
    fixture_with, limits, model, noon, noon_keys, request, snapshot, utc, with_switches,
};
use crate::domain::service::quota::tools::{ToolKind, ToolLimit};
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};
use crate::domain::service::quota::types::QuotaPeriod::{Daily, Monthly};
use crate::domain::service::quota::types::{BucketRow, QuotaPeriod, QuotaTier};
use crate::test_doubles::{PdpReply, USER, USER_TENANT, user_ctx};

fn premium_world(rows: Vec<BucketRow>) -> Fixture {
    world(design_catalog(), design_limits(7), rows)
}

fn sorted(mut events: Vec<Metric>) -> Vec<String> {
    let mut out: Vec<String> = events.drain(..).map(|m| format!("{m:?}")).collect();
    out.sort();
    out
}

// ================================================================================================
// Preflight
// ================================================================================================

#[tokio::test]
async fn an_available_premium_selection_is_allowed_with_the_decision_of_the_effective_model() {
    let fixture = premium_world(Vec::new());

    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(decision.effective_model, "p");
    assert_eq!(decision.entry.id, "p");
    assert_eq!(decision.effective_tier, ModelTier::Premium);
    assert_eq!(decision.provider_id, "openai");
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
    assert_eq!(decision.downgrade_from, None);
    assert_eq!(decision.downgrade_reason, None);
    assert_eq!(decision.policy_version, 7);
    assert_eq!(decision.snapshot.policy_version, 7);
    assert_eq!(decision.limits, design_limits(7));
    assert_eq!(decision.period_keys, noon_keys());
    assert_eq!(decision.reserve.estimated_input_tokens, 1000);
    assert_eq!(decision.reserve.max_output_tokens_applied, 500);
    assert_eq!(decision.reserve.reserve_tokens, 1500);
    assert_eq!(decision.reserve.reserved_credits_micro, 3_750_000);
    assert_eq!(decision.reserve.minimal_generation_floor_applied, 50);
    assert!(decision.vision_input);
    assert!(!decision.web_search_allowed);
    assert!(!decision.code_interpreter_allowed);
}

#[tokio::test]
async fn design_5_10_downgrades_to_standard_and_books_the_standard_reserve() {
    let fixture = design_fixture();

    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(decision.effective_model, "s");
    assert_eq!(decision.effective_tier, ModelTier::Standard);
    assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some("p"));
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(decision.reserve.reserved_credits_micro, 1_500_000);
    assert_eq!(decision.reserve.reserve_tokens, 1500);
}

#[tokio::test]
async fn the_kill_switches_and_a_disabled_model_give_their_reasons() {
    let cases = [
        (
            KillSwitches {
                force_standard_tier: true,
                ..NO_SWITCHES
            },
            DowngradeReason::ForceStandardTier,
        ),
        (
            KillSwitches {
                disable_premium_tier: true,
                ..NO_SWITCHES
            },
            DowngradeReason::DisablePremiumTier,
        ),
    ];
    for (switches, reason) in cases {
        let fixture = premium_world(Vec::new());
        fixture
            .policy
            .add_snapshot(with_switches(snapshot(7, design_catalog()), switches));

        let decision = fixture.service.preflight(&request("p")).await.unwrap();

        assert_eq!(decision.effective_model, "s", "{reason:?}");
        assert_eq!(decision.quota_decision, QuotaDecision::Downgrade);
        assert_eq!(decision.downgrade_reason, Some(reason));
    }

    let catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).build(),
    ];
    let fixture = fixture_with_catalog(catalog);
    let decision = fixture.service.preflight(&request("p")).await.unwrap();
    assert_eq!(decision.effective_model, "s");
    assert_eq!(
        decision.downgrade_reason,
        Some(DowngradeReason::ModelDisabled)
    );
}

fn fixture_with_catalog(catalog: Vec<mini_chat_sdk::ModelCatalogEntry>) -> Fixture {
    world(catalog, design_limits(7), Vec::new())
}

#[tokio::test]
async fn a_standard_selection_never_runs_on_premium() {
    let fixture = premium_world(Vec::new());

    let decision = fixture.service.preflight(&request("s")).await.unwrap();

    assert_eq!(decision.effective_model, "s");
    assert_eq!(decision.quota_decision, QuotaDecision::Allow);
}

#[tokio::test]
async fn when_every_tier_is_exhausted_preflight_is_a_tokens_quota_error_and_writes_nothing() {
    let fixture = premium_world(vec![BucketRow::new(Daily, Total, 60_000_000, 0)]);

    let error = fixture.service.preflight(&request("p")).await.unwrap_err();

    assert!(
        matches!(error, DomainError::QuotaExceeded(QuotaSubject::Tokens)),
        "{error:?}"
    );
    assert!(
        fixture
            .repo
            .calls()
            .iter()
            .all(|call| matches!(call, RepoCall::ReadPreflight(_))),
        "{:?}",
        fixture.repo.calls()
    );
}

#[tokio::test]
async fn the_all_exhausted_rejection_is_a_429_with_the_tokens_violation() {
    let fixture = premium_world(vec![BucketRow::new(Daily, Total, 60_000_000, 0)]);

    let error = fixture.service.preflight(&request("p")).await.unwrap_err();

    let problem = Problem::from(CanonicalError::from(error));
    assert_eq!(problem.status, Some(429));
    let body = serde_json::to_value(&problem).unwrap();
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("resource_exhausted")
    );
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(
        body["context"]["violations"][0]["description"],
        "quota_exceeded"
    );
}

#[tokio::test]
async fn no_enabled_model_in_any_tier_is_a_tokens_quota_error() {
    let catalog = vec![
        model("p", ModelTier::Premium).enabled(false).build(),
        model("s", ModelTier::Standard).enabled(false).build(),
    ];
    let fixture = fixture_with_catalog(catalog);

    let error = fixture.service.preflight(&request("p")).await.unwrap_err();

    assert!(matches!(
        error,
        DomainError::QuotaExceeded(QuotaSubject::Tokens)
    ));
}

#[tokio::test]
async fn preflight_reads_the_chat_and_the_four_buckets_of_the_current_utc_periods() {
    let fixture = premium_world(Vec::new());
    let req = request("p");

    fixture.service.preflight(&req).await.unwrap();

    let reads: Vec<_> = fixture
        .repo
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            RepoCall::ReadPreflight(read) => Some(read),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 1);
    assert_eq!(reads[0].tenant_id, req.tenant_id);
    assert_eq!(reads[0].user_id, req.user_id);
    assert_eq!(reads[0].chat_id, req.chat_id);
    assert_eq!(reads[0].keys, noon_keys());
}

#[tokio::test]
async fn the_period_keys_follow_the_clock_at_the_midnight_boundary() {
    let Fixture { service, .. } = premium_world(Vec::new());
    let before = service.with_clock(fixed_clock(utc(2026, 3, 10, 23, 59, 59)));

    let decision = before.preflight(&request("p")).await.unwrap();
    assert_eq!(decision.period_keys.daily_start.to_string(), "2026-03-10");

    let service = before.with_clock(fixed_clock(utc(2026, 3, 11, 0, 0, 0)));
    let decision = service.preflight(&request("p")).await.unwrap();
    assert_eq!(decision.period_keys.daily_start.to_string(), "2026-03-11");
    assert_eq!(decision.period_keys.monthly_start.to_string(), "2026-03-01");
}

#[tokio::test]
async fn the_prior_context_of_the_chat_is_part_of_the_estimate() {
    let fixture = premium_world(Vec::new());
    fixture.repo.rows.lock().unwrap().prior_context_tokens = 700;

    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(decision.reserve.estimated_input_tokens, 1700);
    assert_eq!(decision.reserve.reserve_tokens, 2200);
}

#[tokio::test]
async fn the_gear_configuration_bounds_the_output_cap_and_the_floor() {
    let fixture = fixture_with(
        FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7)),
        FakeRepo::empty(),
        QuotaServiceConfig {
            streaming_max_output_tokens: 200,
            minimal_generation_floor: 300,
            ..default_config()
        },
    );

    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(decision.reserve.max_output_tokens_applied, 200);
    assert_eq!(decision.reserve.minimal_generation_floor_applied, 200);
}

#[tokio::test]
async fn nothing_is_cached_between_preflights() {
    let fixture = premium_world(Vec::new());

    fixture.service.preflight(&request("p")).await.unwrap();
    fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(fixture.policy.calls().len(), 6);
    assert_eq!(fixture.repo.calls().len(), 2);
}

#[tokio::test]
async fn the_policy_is_resolved_for_the_requesting_user() {
    let fixture = premium_world(Vec::new());

    fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(
        fixture.policy.calls(),
        vec![
            PolicyCall::Version(USER),
            PolicyCall::Snapshot(USER, 7),
            PolicyCall::Limits(USER, 7)
        ]
    );
}

#[tokio::test]
async fn a_policy_failure_is_internal_and_reads_and_records_nothing() {
    for step in [
        PolicyStep::Version,
        PolicyStep::Snapshot,
        PolicyStep::Limits,
    ] {
        let fixture = premium_world(Vec::new());
        fixture.policy.fail(step);

        let error = fixture.service.preflight(&request("p")).await.unwrap_err();

        assert!(
            matches!(error, DomainError::PolicyResolution(_)),
            "{step:?}"
        );
        assert!(fixture.repo.calls().is_empty(), "{step:?}");
        assert!(fixture.metrics.events().is_empty(), "{step:?}");
    }
}

#[tokio::test]
async fn a_failing_read_is_a_database_error() {
    let fixture = premium_world(Vec::new());
    fixture.repo.fail_reads();

    let error = fixture.service.preflight(&request("p")).await.unwrap_err();

    assert!(matches!(error, DomainError::Database(_)), "{error:?}");
}

// ---- web search ------------------------------------------------------------------------------------

#[tokio::test]
async fn web_search_under_the_kill_switch_is_a_feature_disabled_error_before_any_quota_read() {
    let fixture = premium_world(Vec::new());
    fixture.policy.add_snapshot(with_switches(
        snapshot(7, design_catalog()),
        KillSwitches {
            disable_web_search: true,
            ..NO_SWITCHES
        },
    ));
    let mut req = request("p");
    req.web_search_enabled = true;

    let error = fixture.service.preflight(&req).await.unwrap_err();

    assert!(
        matches!(
            error,
            DomainError::FeatureDisabled(DisabledFeature::WebSearch)
        ),
        "{error:?}"
    );
    assert!(fixture.repo.calls().is_empty(), "no quota or chat read");
    assert!(fixture.metrics.events().is_empty());
}

#[tokio::test]
async fn the_web_search_kill_switch_is_ignored_when_the_request_does_not_enable_web_search() {
    let fixture = premium_world(Vec::new());
    fixture.policy.add_snapshot(with_switches(
        snapshot(7, design_catalog()),
        KillSwitches {
            disable_web_search: true,
            ..NO_SWITCHES
        },
    ));

    let decision = fixture.service.preflight(&request("p")).await.unwrap();

    assert!(!decision.web_search_allowed);
}

fn web_rows(daily_calls: i64) -> Vec<BucketRow> {
    vec![BucketRow::new(Daily, Total, 0, 0).with_tool_calls(daily_calls, 0)]
}

#[tokio::test]
async fn the_daily_web_search_quota_is_checked_at_seventy_five_calls() {
    let mut req = request("p");
    req.web_search_enabled = true;

    let at_limit = premium_world(web_rows(75));
    let error = at_limit.service.preflight(&req).await.unwrap_err();
    assert!(
        matches!(error, DomainError::QuotaExceeded(QuotaSubject::WebSearch)),
        "{error:?}"
    );

    let below = premium_world(web_rows(74));
    let decision = below.service.preflight(&req).await.unwrap();
    assert!(decision.web_search_allowed);
}

#[tokio::test]
async fn the_web_search_quota_only_applies_to_requests_that_use_the_tool() {
    // without the request flag
    let fixture = premium_world(web_rows(75));
    assert!(fixture.service.preflight(&request("p")).await.is_ok());

    // monthly calls never block
    let fixture = premium_world(vec![
        BucketRow::new(Monthly, Total, 0, 0).with_tool_calls(1000, 0),
    ]);
    let mut req = request("p");
    req.web_search_enabled = true;
    assert!(fixture.service.preflight(&req).await.is_ok());

    // an effective model without web search support
    let catalog = vec![
        model("p", ModelTier::Premium)
            .tools(false, true, true)
            .build(),
    ];
    let fixture = world(catalog, design_limits(7), web_rows(75));
    let decision = fixture.service.preflight(&req).await.unwrap();
    assert!(!decision.web_search_allowed);
}

#[tokio::test]
async fn the_web_search_quota_follows_the_effective_model_after_a_downgrade() {
    // premium (web search supported) is exhausted; the standard model has no web search
    let catalog = vec![
        model("p", ModelTier::Premium)
            .tools(true, true, true)
            .build(),
        model("s", ModelTier::Standard)
            .tools(false, true, true)
            .build(),
    ];
    let mut rows = web_rows(75);
    rows.push(BucketRow::new(Daily, TierPremium, 22_000_000, 0));
    let fixture = world(catalog, design_limits(7), rows);
    let mut req = request("p");
    req.web_search_enabled = true;

    let decision = fixture.service.preflight(&req).await.unwrap();

    assert_eq!(decision.effective_model, "s");
    assert!(!decision.web_search_allowed);
}

// ---- code interpreter ----------------------------------------------------------------------------------

fn ci_rows(daily_calls: i64) -> Vec<BucketRow> {
    vec![BucketRow::new(Daily, Total, 0, 0).with_tool_calls(0, daily_calls)]
}

fn ci_request() -> crate::domain::service::quota::preflight::PreflightRequest {
    let mut req = request("p");
    req.has_ready_code_interpreter_files = true;
    req
}

#[tokio::test]
async fn the_daily_code_interpreter_quota_is_checked_at_fifty_calls() {
    let at_limit = premium_world(ci_rows(50));
    let error = at_limit.service.preflight(&ci_request()).await.unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::QuotaExceeded(QuotaSubject::CodeInterpreter)
        ),
        "{error:?}"
    );

    let below = premium_world(ci_rows(49));
    let decision = below.service.preflight(&ci_request()).await.unwrap();
    assert!(decision.code_interpreter_allowed);
}

#[tokio::test]
async fn the_code_interpreter_quota_needs_ready_files_support_and_the_switch_off() {
    // no ready files
    let fixture = premium_world(ci_rows(50));
    let decision = fixture.service.preflight(&request("p")).await.unwrap();
    assert!(!decision.code_interpreter_allowed);

    // the kill switch is on: not checked, not allowed
    let fixture = premium_world(ci_rows(50));
    fixture.policy.add_snapshot(with_switches(
        snapshot(7, design_catalog()),
        KillSwitches {
            disable_code_interpreter: true,
            ..NO_SWITCHES
        },
    ));
    let decision = fixture.service.preflight(&ci_request()).await.unwrap();
    assert!(!decision.code_interpreter_allowed);

    // the effective model does not support the tool
    let catalog = vec![
        model("p", ModelTier::Premium)
            .tools(true, true, false)
            .build(),
    ];
    let fixture = world(catalog, design_limits(7), ci_rows(50));
    let decision = fixture.service.preflight(&ci_request()).await.unwrap();
    assert!(!decision.code_interpreter_allowed);
}

#[tokio::test]
async fn the_decision_reports_the_vision_capability_of_the_effective_model() {
    let catalog = vec![
        model("p", ModelTier::Premium).vision(true).build(),
        model("s", ModelTier::Standard).vision(false).build(),
    ];
    let fixture = world(catalog, design_limits(7), Vec::new());

    assert!(
        fixture
            .service
            .preflight(&request("p"))
            .await
            .unwrap()
            .vision_input
    );
    assert!(
        !fixture
            .service
            .preflight(&request("s"))
            .await
            .unwrap()
            .vision_input
    );
}

// ---- preflight metrics ---------------------------------------------------------------------------------

#[tokio::test]
async fn an_allow_counts_the_preflight_and_records_the_estimated_tokens() {
    let fixture = premium_world(Vec::new());

    fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(
        fixture.metrics.events(),
        vec![
            Metric::Preflight(PreflightLabel::Allow, "p".to_owned(), ModelTier::Premium),
            Metric::Estimated(1500)
        ]
    );
}

#[tokio::test]
async fn a_downgrade_counts_the_effective_model_and_its_tier() {
    let fixture = design_fixture();

    fixture.service.preflight(&request("p")).await.unwrap();

    assert_eq!(
        fixture.metrics.events(),
        vec![
            Metric::Preflight(
                PreflightLabel::Downgrade,
                "s".to_owned(),
                ModelTier::Standard
            ),
            Metric::Estimated(1500)
        ]
    );
}

#[tokio::test]
async fn a_rejection_counts_the_selected_model_and_the_starting_tier_without_estimated_tokens() {
    let fixture = premium_world(vec![BucketRow::new(Daily, Total, 60_000_000, 0)]);

    fixture.service.preflight(&request("p")).await.unwrap_err();

    assert_eq!(
        fixture.metrics.events(),
        vec![Metric::Preflight(
            PreflightLabel::Reject,
            "p".to_owned(),
            ModelTier::Premium
        )]
    );
}

#[tokio::test]
async fn a_tool_quota_rejection_does_not_record_a_second_preflight_count() {
    let mut req = request("p");
    req.web_search_enabled = true;
    let fixture = premium_world(web_rows(75));

    fixture.service.preflight(&req).await.unwrap_err();

    assert_eq!(
        fixture.metrics.events(),
        vec![
            Metric::Preflight(PreflightLabel::Allow, "p".to_owned(), ModelTier::Premium),
            // 1000 text + 500 web search surcharge + 500 output cap
            Metric::Estimated(2000)
        ],
        "the decision count and the estimate stay; no reject count"
    );
}

// ================================================================================================
// Reserve
// ================================================================================================

#[tokio::test]
async fn a_committed_reserve_counts_one_reserve_per_period() {
    let fixture = design_fixture();
    let decision = fixture.service.preflight(&request("p")).await.unwrap();
    *fixture.repo.reserve_result.lock().unwrap() = Ok(ReservedColumns::from_decision(&decision));
    fixture.metrics.events.lock().unwrap().clear();

    let columns = fixture
        .service
        .reserve(&ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: USER,
            mode: ReserveMode::Send,
            decision: &decision,
            now: noon(),
        })
        .await
        .unwrap();

    assert_eq!(columns, ReservedColumns::from_decision(&decision));
    assert_eq!(
        fixture.metrics.events(),
        vec![Metric::Reserve(Daily), Metric::Reserve(Monthly)]
    );
}

#[tokio::test]
async fn a_rejected_reserve_counts_nothing() {
    let fixture = design_fixture();
    let decision = fixture.service.preflight(&request("p")).await.unwrap();
    fixture.metrics.events.lock().unwrap().clear();
    // the fake repository answers with an error by default

    let result = fixture
        .service
        .reserve(&ReserveRequest {
            tenant_id: USER_TENANT,
            user_id: USER,
            mode: ReserveMode::Fill {
                turn_id: Uuid::from_u128(1),
            },
            decision: &decision,
            now: noon(),
        })
        .await;

    assert!(result.is_err());
    assert!(fixture.metrics.events().is_empty());
}

#[test]
fn the_reserve_metrics_can_be_recorded_by_a_pipeline_that_commits_its_own_transaction() {
    let fixture = design_fixture();

    fixture.service.record_reserve_committed();

    assert_eq!(
        fixture.metrics.events(),
        vec![Metric::Reserve(Daily), Metric::Reserve(Monthly)]
    );
}

// ================================================================================================
// Finalization
// ================================================================================================

fn won(method: SettlementMethod) -> FinalizeWon {
    FinalizeWon {
        terminal_state: TerminalState::Completed,
        billing_outcome: BillingOutcome::Completed,
        settlement_method: method,
        committed_credits_micro: 1_200_000,
        settled: true,
        usage_event_enqueued: true,
        actual_input_tokens: 900,
        actual_output_tokens: 300,
        reserve_tokens: 1500,
        overshoot_capped: false,
        fallback: None,
        effective_model: Some("s".to_owned()),
        code_interpreter_calls: 0,
        summary_trigger: None,
    }
}

fn orphan_request() -> FinalizeRequest {
    FinalizeRequest::orphan(
        Uuid::from_u128(0x77),
        utc(2026, 3, 10, 11, 55, 0),
        SettlementSnapshot::Unavailable,
        noon(),
    )
}

#[test]
fn an_actual_settlement_counts_the_commit_per_period_and_the_actual_tokens() {
    let fixture = design_fixture();

    fixture
        .service
        .record_finalized(&won(SettlementMethod::Actual));

    assert_eq!(
        sorted(fixture.metrics.events()),
        sorted(vec![
            Metric::Commit(Daily),
            Metric::Commit(Monthly),
            Metric::Actual(1200)
        ])
    );
}

#[test]
fn code_interpreter_calls_of_an_actual_settlement_are_counted_per_effective_model() {
    let fixture = design_fixture();
    let mut finalized = won(SettlementMethod::Actual);
    finalized.code_interpreter_calls = 3;

    fixture.service.record_finalized(&finalized);

    assert!(
        fixture
            .metrics
            .events()
            .contains(&Metric::CodeInterpreter("s".to_owned(), 3))
    );
}

#[test]
fn the_overshoot_is_counted_per_period_when_actual_tokens_exceed_the_reserve_capped_or_not() {
    for capped in [false, true] {
        let fixture = design_fixture();
        let mut finalized = won(SettlementMethod::Actual);
        finalized.reserve_tokens = 1000;
        finalized.actual_input_tokens = 900;
        finalized.actual_output_tokens = 150;
        finalized.overshoot_capped = capped;

        fixture.service.record_finalized(&finalized);

        let events = fixture.metrics.events();
        assert!(
            events.contains(&Metric::Overshoot(Daily)),
            "capped={capped}"
        );
        assert!(
            events.contains(&Metric::Overshoot(Monthly)),
            "capped={capped}"
        );
    }
}

#[test]
fn actual_tokens_equal_to_the_reserve_are_not_an_overshoot() {
    let fixture = design_fixture();
    let mut finalized = won(SettlementMethod::Actual);
    finalized.reserve_tokens = 1200;

    fixture.service.record_finalized(&finalized);

    assert!(
        !fixture
            .metrics
            .events()
            .iter()
            .any(|metric| matches!(metric, Metric::Overshoot(_)))
    );
}

#[test]
fn estimated_and_released_settlements_record_no_commit_or_token_metric() {
    for method in [SettlementMethod::Estimated, SettlementMethod::Released] {
        let fixture = design_fixture();

        fixture.service.record_finalized(&won(method));

        assert!(fixture.metrics.events().is_empty(), "{method:?}");
    }
}

#[test]
fn a_fallback_settlement_is_estimated_and_records_no_actual_metric() {
    let fixture = design_fixture();
    let mut fell_back = won(SettlementMethod::Estimated);
    fell_back.fallback =
        Some(crate::domain::service::quota::settle::FallbackCause::SnapshotUnavailable);

    fixture.service.record_finalized(&fell_back);

    assert!(fixture.metrics.events().is_empty());
}

#[test]
fn a_finalization_that_settled_nothing_records_no_metric() {
    let fixture = design_fixture();
    let mut nothing = won(SettlementMethod::Estimated);
    nothing.settled = false;
    nothing.usage_event_enqueued = false;

    fixture.service.record_finalized(&nothing);

    assert!(fixture.metrics.events().is_empty());
}

#[tokio::test]
async fn a_lost_finalization_records_nothing() {
    let fixture = design_fixture();
    *fixture.repo.finalize_result.lock().unwrap() = FinalizeResult::Lost;

    let result = fixture
        .service
        .finalize_turn(&orphan_request())
        .await
        .unwrap();

    assert_eq!(result, FinalizeResult::Lost);
    assert!(fixture.metrics.events().is_empty());
}

#[tokio::test]
async fn a_won_finalization_records_its_settlement_metrics_after_the_repository_returns() {
    let fixture = design_fixture();
    *fixture.repo.finalize_result.lock().unwrap() =
        FinalizeResult::Won(Box::new(won(SettlementMethod::Actual)));

    let result = fixture
        .service
        .finalize_turn(&orphan_request())
        .await
        .unwrap();

    assert!(matches!(result, FinalizeResult::Won(_)));
    assert_eq!(
        sorted(fixture.metrics.events()),
        sorted(vec![
            Metric::Commit(Daily),
            Metric::Commit(Monthly),
            Metric::Actual(1200)
        ])
    );
    assert!(matches!(
        fixture.repo.calls().last(),
        Some(RepoCall::Finalize(_))
    ));
}

#[tokio::test]
async fn the_orphan_snapshot_is_prefetched_for_the_applied_version_by_the_service() {
    let fixture = design_fixture();
    fixture.policy.set_current(9);

    let fetched = fixture
        .service
        .prefetch_settlement_snapshot(USER, Uuid::from_u128(0x77), Some(7))
        .await;

    assert!(matches!(fetched, SettlementSnapshot::Available(_)));
    assert_eq!(fixture.policy.calls(), vec![PolicyCall::Snapshot(USER, 7)]);
}

// ================================================================================================
// Tool limits
// ================================================================================================

#[test]
fn the_service_applies_the_configured_per_turn_tool_limits() {
    let fixture = fixture_with(
        FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7)),
        FakeRepo::empty(),
        QuotaServiceConfig {
            quota: QuotaConfig {
                web_search_max_calls_per_message: 1,
                ..QuotaConfig::default()
            },
            ..default_config()
        },
    );

    assert_eq!(
        fixture.service.tool_call_limit(ToolKind::WebSearch, 1),
        ToolLimit::Within
    );
    assert_eq!(
        fixture.service.tool_call_limit(ToolKind::WebSearch, 2),
        ToolLimit::Exceeded {
            error_code: "web_search_calls_exceeded"
        }
    );
}

// ================================================================================================
// Status and warnings
// ================================================================================================

fn status_world(rows: Vec<BucketRow>) -> Fixture {
    let fixture = world(
        design_catalog(),
        limits(7, 100_000_000, 1_000_000_000, 40_000_000, 400_000_000),
        Vec::new(),
    );
    *fixture.repo.buckets.lock().unwrap() = rows;
    fixture
}

#[tokio::test]
async fn the_status_is_built_from_the_current_limits_and_the_bucket_rows() {
    let fixture = status_world(vec![BucketRow::new(Daily, Total, 80_000_001, 0)]);

    let status = fixture.service.quota_status(&user_ctx()).await.unwrap();

    assert_eq!(status.warning_threshold_pct, 80);
    assert_eq!(status.tiers.len(), 2);
    assert_eq!(status.tiers[0].tier, QuotaTier::Premium);
    let total = &status.tiers[1];
    assert_eq!(total.tier, QuotaTier::Total);
    assert_eq!(total.periods[0].period, QuotaPeriod::Daily);
    assert_eq!(total.periods[0].remaining_credits_micro, 19_999_999);
    assert_eq!(total.periods[0].remaining_percentage, 19);
    assert!(total.periods[0].warning);
    assert_eq!(total.periods[0].next_reset, utc(2026, 3, 11, 0, 0, 0));
    assert_eq!(total.periods[1].next_reset, utc(2026, 4, 1, 0, 0, 0));
}

#[tokio::test]
async fn the_status_reads_the_limits_of_the_current_version_and_never_the_snapshot() {
    let fixture = status_world(Vec::new());

    fixture.service.quota_status(&user_ctx()).await.unwrap();

    assert_eq!(
        fixture.policy.calls(),
        vec![PolicyCall::Version(USER), PolicyCall::Limits(USER, 7)]
    );
    assert!(
        fixture
            .repo
            .calls()
            .iter()
            .all(|call| matches!(call, RepoCall::ReadBuckets))
    );
}

#[tokio::test]
async fn the_status_reports_the_configured_warning_threshold() {
    let fixture = fixture_with(
        FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7)),
        FakeRepo::empty(),
        QuotaServiceConfig {
            quota: QuotaConfig {
                warning_threshold_pct: 60,
                ..QuotaConfig::default()
            },
            ..default_config()
        },
    );

    let status = fixture.service.quota_status(&user_ctx()).await.unwrap();

    assert_eq!(status.warning_threshold_pct, 60);
}

#[tokio::test]
async fn the_status_authorizes_read_on_the_user_quota_resource_without_a_resource_id() {
    let fixture = status_world(Vec::new());

    fixture.service.quota_status(&user_ctx()).await.unwrap();

    let requests = fixture.pdp.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].action.name, "read");
    assert_eq!(
        requests[0].resource.resource_type,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
    assert_eq!(requests[0].resource.id, None);
}

#[tokio::test]
async fn a_pdp_failure_stops_the_status_before_any_policy_or_database_read() {
    for (reply, expected_denied) in [(PdpReply::Deny, true), (PdpReply::Failure, false)] {
        let pdp = crate::test_doubles::FakePdp::new(reply);
        let Fixture { policy, repo, .. } = status_world(Vec::new());
        let metrics = crate::domain::service::quota::test_support::RecordingMetrics::new();
        let service = QuotaService::new(super::QuotaDeps {
            authz: std::sync::Arc::new(crate::domain::authz::AuthzService::new(
                std::sync::Arc::clone(&pdp)
                    as std::sync::Arc<dyn authz_resolver_sdk::AuthZResolverApi>,
            )),
            policy: std::sync::Arc::clone(&policy)
                as std::sync::Arc<dyn crate::domain::ports::PolicyCatalog>,
            repo: std::sync::Arc::clone(&repo)
                as std::sync::Arc<dyn crate::domain::service::quota::ports::QuotaRepository>,
            metrics,
            config: default_config(),
        })
        .with_clock(fixed_clock(noon()));

        let error = service.quota_status(&user_ctx()).await.unwrap_err();

        if expected_denied {
            assert!(matches!(error, DomainError::AuthzDenied), "{error:?}");
        } else {
            assert!(matches!(error, DomainError::PdpUnavailable), "{error:?}");
        }
        assert!(policy.calls().is_empty());
        assert!(repo.calls().is_empty());
    }
}

#[tokio::test]
async fn a_policy_or_read_failure_of_the_status_is_internal() {
    for step in [PolicyStep::Version, PolicyStep::Limits] {
        let fixture = status_world(Vec::new());
        fixture.policy.fail(step);

        let error = fixture.service.quota_status(&user_ctx()).await.unwrap_err();

        assert!(
            matches!(error, DomainError::PolicyResolution(_)),
            "{step:?}"
        );
        assert!(fixture.repo.calls().is_empty(), "{step:?}: no row is read");
    }

    let fixture = status_world(Vec::new());
    fixture.repo.fail_reads();
    let error = fixture.service.quota_status(&user_ctx()).await.unwrap_err();
    assert!(matches!(error, DomainError::Database(_)));
}

#[tokio::test]
async fn the_warnings_use_the_captured_limits_without_calling_the_plugin() {
    let fixture = status_world(vec![BucketRow::new(Daily, Total, 85_000_000, 0)]);
    let captured = limits(7, 100_000_000, 1_000_000_000, 40_000_000, 400_000_000);

    let warnings = fixture
        .service
        .quota_warnings(USER_TENANT, USER, &captured)
        .await
        .unwrap();

    assert!(
        fixture.policy.calls().is_empty(),
        "no plugin call on the terminal path"
    );
    let daily_total = warnings
        .iter()
        .find(|w| w.tier == QuotaTier::Total && w.period == Daily)
        .unwrap();
    assert_eq!(daily_total.remaining_percentage, 15);
    assert!(daily_total.warning);
    assert_eq!(daily_total.next_reset, Some(utc(2026, 3, 11, 0, 0, 0)));
}

#[tokio::test]
async fn a_failing_read_of_the_warnings_is_a_database_error() {
    let fixture = status_world(Vec::new());
    fixture.repo.fail_reads();

    let error = fixture
        .service
        .quota_warnings(USER_TENANT, USER, &design_limits(7))
        .await
        .unwrap_err();

    assert!(matches!(error, DomainError::Database(_)));
}
