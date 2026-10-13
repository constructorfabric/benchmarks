#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the summary-model check of
//! `cpt-cf-mini-chat-flow-gear-foundation-gear-start` (steps
//! `inst-check-summary-model` .. `inst-log-summary-model`): it logs an error
//! naming the model and lets the startup continue.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{MiniChatModelPolicyPluginClientV1, PolicyPluginError};
use tracing::Level;

use super::{DEFAULT_SUMMARY_MODEL_ID, SummaryModelStatus, check_summary_model};
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::test_doubles::{
    Behavior, FakePolicyPlugin, LogCapture, POLICY_VERSION, PluginWorld, USER, entry, entry_json,
};

fn world_with(models: &[(&str, bool)]) -> (PluginWorld, Arc<FakePolicyPlugin>) {
    let plugin = FakePolicyPlugin::with_catalog(
        models
            .iter()
            .map(|(id, enabled)| entry(entry_json(id, *enabled)))
            .collect(),
    );
    let world = PluginWorld::new();
    world.register_policy(Arc::clone(&plugin) as Arc<dyn MiniChatModelPolicyPluginClientV1>);
    (world, plugin)
}

fn gateway(world: &PluginWorld) -> ModelPolicyGateway {
    world
        .policy_gateway()
        .with_call_timeout(Duration::from_millis(50))
}

#[test]
fn the_default_summary_model_is_gpt_4_1_mini() {
    assert_eq!(DEFAULT_SUMMARY_MODEL_ID, "gpt-4.1-mini");
}

#[tokio::test]
async fn an_enabled_summary_model_logs_nothing() {
    let (world, _plugin) = world_with(&[("sum-model", true), ("other", true)]);
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::Enabled);
    assert!(
        logs.at(Level::ERROR).is_empty(),
        "{:?}",
        logs.at(Level::ERROR)
    );
}

#[tokio::test]
async fn the_check_reads_the_current_version_and_then_the_snapshot_of_that_version() {
    let (world, plugin) = world_with(&[("sum-model", true)]);

    check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(
        plugin.snapshot_requests.lock().unwrap().clone(),
        vec![(USER, POLICY_VERSION)]
    );
}

#[tokio::test]
async fn an_empty_model_id_means_the_default_summary_model() {
    let (world, _plugin) = world_with(&[("gpt-4.1-mini", true)]);
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "").await;

    assert_eq!(status, SummaryModelStatus::Enabled);
    assert!(logs.at(Level::ERROR).is_empty());

    let (world, _plugin) = world_with(&[("something-else", true)]);
    let logs = LogCapture::start();
    let status = check_summary_model(&gateway(&world), USER, "").await;
    assert_eq!(status, SummaryModelStatus::Missing);
    assert_eq!(logs.at_containing(Level::ERROR, "gpt-4.1-mini").len(), 1);
}

#[tokio::test]
async fn a_missing_summary_model_logs_an_error_naming_it() {
    let (world, _plugin) = world_with(&[("other", true)]);
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::Missing);
    assert_eq!(
        logs.at_containing(Level::ERROR, "sum-model").len(),
        1,
        "{:?}",
        logs.records()
    );
}

#[tokio::test]
async fn a_disabled_summary_model_logs_an_error_naming_it() {
    let (world, _plugin) = world_with(&[("sum-model", false)]);
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::Disabled);
    assert_eq!(logs.at_containing(Level::ERROR, "sum-model").len(), 1);
}

#[tokio::test]
async fn an_unregistered_policy_plugin_logs_an_error_naming_the_model() {
    let world = PluginWorld::new();
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::SnapshotUnavailable);
    assert_eq!(logs.at_containing(Level::ERROR, "sum-model").len(), 1);
}

#[tokio::test]
async fn a_registry_failure_logs_an_error_and_continues() {
    let (world, _plugin) = world_with(&[("sum-model", true)]);
    world.registry.fail_list("registry down");
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::SnapshotUnavailable);
    assert_eq!(logs.at_containing(Level::ERROR, "sum-model").len(), 1);
}

#[tokio::test]
async fn a_plugin_error_logs_an_error_and_continues() {
    let (world, plugin) = world_with(&[("sum-model", true)]);
    plugin.set_snapshot(Behavior::Fail(PolicyPluginError::Internal(
        "boom".to_owned(),
    )));
    let logs = LogCapture::start();

    let status = check_summary_model(&gateway(&world), USER, "sum-model").await;

    assert_eq!(status, SummaryModelStatus::SnapshotUnavailable);
    assert_eq!(logs.at_containing(Level::ERROR, "sum-model").len(), 1);
}

#[tokio::test]
async fn a_policy_call_that_never_answers_logs_an_error_and_continues() {
    for hanging_version in [true, false] {
        let (world, plugin) = world_with(&[("sum-model", true)]);
        if hanging_version {
            plugin.set_version(Behavior::Hang);
        } else {
            plugin.set_snapshot(Behavior::Hang);
        }
        let logs = LogCapture::start();

        let status = tokio::time::timeout(
            Duration::from_secs(5),
            check_summary_model(&gateway(&world), USER, "sum-model"),
        )
        .await
        .expect("each plugin call is bounded");

        assert_eq!(status, SummaryModelStatus::SnapshotUnavailable);
        assert_eq!(logs.at_containing(Level::ERROR, "sum-model").len(), 1);
    }
}
