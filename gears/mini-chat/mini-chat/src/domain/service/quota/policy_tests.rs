#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: `cpt-cf-mini-chat-algo-quota-billing-resolve-policy` and
//! `cpt-cf-mini-chat-dod-quota-billing-policy-binding`.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, ModelTier, PolicyPluginError};
use tracing::Level;
use uuid::Uuid;

use super::{
    SettlementSnapshot, prefetch_settlement_snapshot, resolve_preflight_policy,
    resolve_status_policy,
};
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::domain::service::quota::test_support::{
    FakePolicy, PolicyCall, PolicyStep, design_catalog, design_limits, limits, model, snapshot,
};
use crate::test_doubles::{Behavior, FakePolicyPlugin, LogCapture, PluginWorld, USER};

fn world() -> Arc<FakePolicy> {
    FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7))
}

fn turn() -> Uuid {
    Uuid::from_u128(0x7001)
}

// ---- preflight mode -----------------------------------------------------------------------------

#[tokio::test]
async fn preflight_reads_the_current_version_then_its_snapshot_then_its_limits() {
    let policy = world();

    let resolved = resolve_preflight_policy(&*policy, USER).await.unwrap();

    assert_eq!(resolved.policy_version, 7);
    assert_eq!(resolved.snapshot.policy_version, 7);
    assert_eq!(resolved.snapshot.model_catalog.len(), 2);
    assert_eq!(resolved.limits, design_limits(7));
    assert_eq!(
        policy.calls(),
        vec![
            PolicyCall::Version(USER),
            PolicyCall::Snapshot(USER, 7),
            PolicyCall::Limits(USER, 7)
        ]
    );
}

#[tokio::test]
async fn preflight_binds_the_current_version_not_an_older_one() {
    let policy = world();
    policy.add_snapshot(snapshot(6, vec![model("old", ModelTier::Standard).build()]));
    policy
        .limits
        .lock()
        .unwrap()
        .insert(6, limits(6, 1, 1, 1, 1));
    policy.set_current(7);

    let resolved = resolve_preflight_policy(&*policy, USER).await.unwrap();

    assert_eq!(resolved.policy_version, 7);
    assert_eq!(resolved.limits.policy_version, 7);
    assert!(
        policy
            .calls()
            .iter()
            .all(|call| !matches!(call, PolicyCall::Snapshot(_, 6) | PolicyCall::Limits(_, 6)))
    );
}

#[tokio::test]
async fn every_preflight_repeats_the_plugin_calls_nothing_is_cached() {
    let policy = world();

    resolve_preflight_policy(&*policy, USER).await.unwrap();
    resolve_preflight_policy(&*policy, USER).await.unwrap();

    assert_eq!(policy.calls().len(), 6);
}

#[tokio::test]
async fn a_new_policy_version_is_picked_up_by_the_next_preflight() {
    let policy = world();
    policy.add_snapshot(snapshot(8, design_catalog()));
    policy.limits.lock().unwrap().insert(8, design_limits(8));

    let first = resolve_preflight_policy(&*policy, USER).await.unwrap();
    policy.set_current(8);
    let second = resolve_preflight_policy(&*policy, USER).await.unwrap();

    assert_eq!((first.policy_version, second.policy_version), (7, 8));
}

#[tokio::test]
async fn a_failing_step_of_the_preflight_is_an_internal_error_and_stops_the_sequence() {
    for (step, expected_calls) in [
        (PolicyStep::Version, 1),
        (PolicyStep::Snapshot, 2),
        (PolicyStep::Limits, 3),
    ] {
        let policy = world();
        policy.fail(step);

        let error = resolve_preflight_policy(&*policy, USER).await.unwrap_err();

        assert!(
            matches!(error, DomainError::PolicyResolution(_)),
            "{step:?}: {error:?}"
        );
        assert_eq!(policy.calls().len(), expected_calls, "{step:?}");
    }
}

// ---- status mode --------------------------------------------------------------------------------

#[tokio::test]
async fn status_reads_the_version_and_the_limits_but_never_the_snapshot() {
    let policy = world();

    let resolved = resolve_status_policy(&*policy, USER).await.unwrap();

    assert_eq!(resolved.policy_version, 7);
    assert_eq!(resolved.limits, design_limits(7));
    assert_eq!(
        policy.calls(),
        vec![PolicyCall::Version(USER), PolicyCall::Limits(USER, 7)]
    );
}

#[tokio::test]
async fn a_failing_step_of_the_status_is_an_internal_error() {
    for step in [PolicyStep::Version, PolicyStep::Limits] {
        let policy = world();
        policy.fail(step);

        let error = resolve_status_policy(&*policy, USER).await.unwrap_err();

        assert!(
            matches!(error, DomainError::PolicyResolution(_)),
            "{step:?}: {error:?}"
        );
    }
}

// ---- settlement mode ----------------------------------------------------------------------------

#[tokio::test]
async fn settlement_fetches_the_snapshot_of_the_applied_version_never_the_current_one() {
    let policy = world();
    policy.add_snapshot(snapshot(9, vec![model("new", ModelTier::Premium).build()]));
    policy.set_current(9);

    let fetched = prefetch_settlement_snapshot(&*policy, USER, turn(), Some(7)).await;

    let SettlementSnapshot::Available(snapshot) = fetched else {
        panic!("the snapshot of version 7 is available");
    };
    assert_eq!(snapshot.policy_version, 7);
    assert_eq!(policy.calls(), vec![PolicyCall::Snapshot(USER, 7)]);
}

#[tokio::test]
async fn a_null_applied_version_is_not_fetched() {
    let policy = world();

    let fetched = prefetch_settlement_snapshot(&*policy, USER, turn(), None).await;

    assert_eq!(fetched, SettlementSnapshot::Unavailable);
    assert!(policy.calls().is_empty());
}

#[tokio::test]
async fn a_failing_fetch_is_an_unavailable_snapshot_logged_at_error_with_the_turn_id() {
    let capture = LogCapture::start();
    let policy = world();
    policy.fail(PolicyStep::Snapshot);

    let fetched = prefetch_settlement_snapshot(&*policy, USER, turn(), Some(7)).await;

    assert_eq!(fetched, SettlementSnapshot::Unavailable);
    assert!(
        !capture
            .at_containing(Level::ERROR, &turn().to_string())
            .is_empty(),
        "{}",
        capture.all_text()
    );
}

#[test]
fn a_snapshot_is_usable_only_for_its_own_version() {
    let available = SettlementSnapshot::Available(Box::new(snapshot(7, design_catalog())));

    assert!(available.usable_for(7).is_some());
    assert_eq!(available.usable_for(7).unwrap().policy_version, 7);
    assert!(available.usable_for(6).is_none());
    assert!(available.usable_for(8).is_none());
    assert!(SettlementSnapshot::Unavailable.usable_for(7).is_none());
}

// ---- the real gateway: missing plugin, failures and the call bound ----------------------------------

fn gateway_world() -> (PluginWorld, Arc<FakePolicyPlugin>) {
    let world = PluginWorld::new();
    let plugin = FakePolicyPlugin::with_catalog(design_catalog());
    plugin.set_limits(Some(design_limits(crate::test_doubles::POLICY_VERSION)));
    world.register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
    (world, plugin)
}

#[tokio::test]
async fn the_gateway_serves_the_three_resolutions_from_the_plugin() {
    let (world, _plugin) = gateway_world();
    let gateway = world.policy_gateway();

    let preflight = resolve_preflight_policy(&gateway, USER).await.unwrap();
    let status = resolve_status_policy(&gateway, USER).await.unwrap();
    let settlement = prefetch_settlement_snapshot(
        &gateway,
        USER,
        turn(),
        Some(crate::test_doubles::POLICY_VERSION),
    )
    .await;

    assert_eq!(
        preflight.policy_version,
        crate::test_doubles::POLICY_VERSION
    );
    assert_eq!(preflight.limits, status.limits);
    assert!(matches!(settlement, SettlementSnapshot::Available(_)));
}

#[tokio::test]
async fn a_missing_plugin_is_an_internal_error_for_preflight_and_status() {
    let world = PluginWorld::new();
    let gateway = world.policy_gateway();

    let preflight = resolve_preflight_policy(&gateway, USER).await.unwrap_err();
    let status = resolve_status_policy(&gateway, USER).await.unwrap_err();

    assert!(matches!(preflight, DomainError::PolicyResolution(_)));
    assert!(matches!(status, DomainError::PolicyResolution(_)));
}

#[tokio::test]
async fn a_missing_plugin_makes_the_settlement_snapshot_unavailable() {
    let world = PluginWorld::new();
    let gateway = world.policy_gateway();

    let fetched = prefetch_settlement_snapshot(&gateway, USER, turn(), Some(7)).await;

    assert_eq!(fetched, SettlementSnapshot::Unavailable);
}

#[tokio::test]
async fn a_failing_plugin_call_is_an_internal_error() {
    for failing in ["version", "snapshot", "limits"] {
        let (world, plugin) = gateway_world();
        let error = PolicyPluginError::Internal("boom".to_owned());
        match failing {
            "version" => plugin.set_version(Behavior::Fail(error)),
            "snapshot" => plugin.set_snapshot(Behavior::Fail(error)),
            _ => plugin.set_limits_behavior(Behavior::Fail(error)),
        }
        let gateway = world.policy_gateway();

        let result = resolve_preflight_policy(&gateway, USER).await;

        assert!(
            matches!(result, Err(DomainError::PolicyResolution(_))),
            "{failing}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_plugin_call_that_never_answers_is_bounded_by_the_call_timeout() {
    for hanging in ["version", "snapshot", "limits"] {
        let (world, plugin) = gateway_world();
        match hanging {
            "version" => plugin.set_version(Behavior::Hang),
            "snapshot" => plugin.set_snapshot(Behavior::Hang),
            _ => plugin.set_limits_behavior(Behavior::Hang),
        }
        let gateway = world
            .policy_gateway()
            .with_call_timeout(Duration::from_millis(50));

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            resolve_preflight_policy(&gateway, USER),
        )
        .await
        .expect("the call is bounded");

        assert!(
            matches!(result, Err(DomainError::PolicyResolution(_))),
            "{hanging}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_settlement_fetch_that_never_answers_is_an_unavailable_snapshot() {
    let (world, plugin) = gateway_world();
    plugin.set_snapshot(Behavior::Hang);
    let gateway = world
        .policy_gateway()
        .with_call_timeout(Duration::from_millis(50));

    let fetched = tokio::time::timeout(
        Duration::from_secs(5),
        prefetch_settlement_snapshot(
            &gateway,
            USER,
            turn(),
            Some(crate::test_doubles::POLICY_VERSION),
        ),
    )
    .await
    .expect("the call is bounded");

    assert_eq!(fetched, SettlementSnapshot::Unavailable);
}

#[tokio::test]
async fn the_gateway_passes_user_and_version_but_no_tenant_to_the_limits_call() {
    let (world, plugin) = gateway_world();
    let gateway = world.policy_gateway();

    let policy: &dyn PolicyCatalog = &gateway;
    let served = policy
        .user_limits(USER, crate::test_doubles::POLICY_VERSION)
        .await
        .unwrap();

    assert_eq!(served, design_limits(crate::test_doubles::POLICY_VERSION));
    assert!(plugin.snapshot_requests.lock().unwrap().is_empty());
}
