#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4: the thread-summary parameters of the stream settings and the
//! `ContextPlan` outputs carried into the finalization input
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`).

use super::{StreamSettings, summary_trigger_plan};
use crate::config::MiniChatConfig;
use crate::domain::service::stream::context::ContextPlan;

fn plan() -> ContextPlan {
    ContextPlan {
        thread_summary_applied: Some(17),
        assembled_context_tokens: 4321,
        messages_truncated: true,
        effective_input_budget: 9000,
        ..ContextPlan::default()
    }
}

#[test]
fn the_settings_take_the_trigger_switch_and_the_threshold_from_the_configuration() {
    let mut config = MiniChatConfig::default();
    assert!(StreamSettings::from_config(&config).summary_enabled);
    assert_eq!(
        StreamSettings::from_config(&config).summary_threshold_pct,
        80
    );

    config.thread_summary_worker.enabled = false;
    config.thread_summary_worker.compression_threshold_pct = 65;

    let settings = StreamSettings::from_config(&config);
    assert!(!settings.summary_enabled);
    assert_eq!(settings.summary_threshold_pct, 65);
}

#[test]
fn the_context_plan_outputs_are_carried_by_value() {
    let settings = StreamSettings::from_config(&MiniChatConfig::default());

    let carried = summary_trigger_plan(&settings, &plan());

    assert!(carried.enabled);
    assert_eq!(carried.compression_threshold_pct, 80);
    assert_eq!(carried.assembled_context_tokens, 4321);
    assert!(carried.messages_truncated);
    assert_eq!(
        carried.effective_input_budget, 9000,
        "the inst-input-limit result"
    );
    assert!(
        carried.summary_applied,
        "a loaded summary is thread_summary_applied"
    );
}

#[test]
fn a_plan_without_a_loaded_summary_carries_summary_applied_false() {
    let settings = StreamSettings::from_config(&MiniChatConfig::default());
    let mut without = plan();
    without.thread_summary_applied = None;

    assert!(!summary_trigger_plan(&settings, &without).summary_applied);
}

#[test]
fn each_turn_gets_its_own_system_request_id_generated_before_the_transaction() {
    let settings = StreamSettings::from_config(&MiniChatConfig::default());

    let first = summary_trigger_plan(&settings, &plan()).system_request_id;
    let second = summary_trigger_plan(&settings, &plan()).system_request_id;

    assert_eq!(first.get_version_num(), 4);
    assert_ne!(first, second);
}

#[test]
fn a_disabled_configuration_yields_a_disabled_plan() {
    let mut config = MiniChatConfig::default();
    config.thread_summary_worker.enabled = false;

    let carried = summary_trigger_plan(&StreamSettings::from_config(&config), &plan());

    assert!(!carried.enabled);
}
