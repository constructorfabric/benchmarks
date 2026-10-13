#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-handle-usage`.

use std::sync::Arc;
use std::time::Duration;

use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
use tracing::Level;

use super::{PUBLISH_USAGE_TIMEOUT, UsageHandler};
use crate::test_doubles::{
    FakePolicyPlugin, LogCapture, PluginWorld, PublishBehavior, outbox_message, usage_event,
    usage_payload,
};

fn handler(world: &PluginWorld) -> UsageHandler {
    UsageHandler::new(Arc::new(world.policy_gateway()))
}

fn fast(world: &PluginWorld) -> UsageHandler {
    handler(world).with_publish_timeout(Duration::from_millis(50))
}

fn world_with(plugin: &Arc<FakePolicyPlugin>) -> PluginWorld {
    let world = PluginWorld::new();
    world.register_policy(
        Arc::clone(plugin) as Arc<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>
    );
    world
}

#[test]
fn the_publish_bound_is_twenty_seconds_below_the_thirty_second_lease() {
    assert_eq!(PUBLISH_USAGE_TIMEOUT, Duration::from_secs(20));
    assert!(PUBLISH_USAGE_TIMEOUT < Duration::from_secs(30));
}

// ---- inst-decode-usage .. inst-reject-malformed-usage ---------------------------------------

#[tokio::test]
async fn a_malformed_payload_is_rejected_with_a_reason() {
    let plugin = FakePolicyPlugin::empty();
    let world = world_with(&plugin);

    let result = handler(&world)
        .handle(&outbox_message(b"{ not json".to_vec(), 0))
        .await;

    match result {
        MessageResult::Reject(reason) => assert!(!reason.is_empty()),
        other => panic!("expected Reject, got {other:?}"),
    }
    assert!(plugin.published_events().is_empty());
}

#[tokio::test]
async fn a_payload_with_a_missing_field_is_rejected() {
    let plugin = FakePolicyPlugin::empty();
    let world = world_with(&plugin);
    let mut value: serde_json::Value = serde_json::from_slice(&usage_payload()).unwrap();
    value.as_object_mut().unwrap().remove("dedupe_key");

    let result = handler(&world)
        .handle(&outbox_message(serde_json::to_vec(&value).unwrap(), 0))
        .await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
}

#[tokio::test]
async fn a_malformed_payload_is_rejected_even_without_a_plugin() {
    let world = PluginWorld::new();

    let result = handler(&world)
        .handle(&outbox_message(b"\xff\xfe".to_vec(), 0))
        .await;

    assert!(matches!(result, MessageResult::Reject(_)), "{result:?}");
}

// ---- inst-resolve-policy -------------------------------------------------------------------------

#[tokio::test]
async fn no_registered_plugin_is_retry() {
    let world = PluginWorld::new();

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
}

#[tokio::test]
async fn a_registry_failure_is_retry() {
    let plugin = FakePolicyPlugin::empty();
    let world = world_with(&plugin);
    world.registry.fail_list("registry down");

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert!(plugin.published_events().is_empty());
}

#[tokio::test]
async fn an_instance_without_a_client_is_retry() {
    let world = PluginWorld::new();
    world.add_policy_instance();

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
}

// ---- inst-publish .. inst-return-usage-ok --------------------------------------------------------

#[tokio::test]
async fn a_successful_publish_is_ok_and_hands_the_decoded_event_to_the_plugin() {
    let plugin = FakePolicyPlugin::empty();
    let world = world_with(&plugin);

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    assert!(matches!(result, MessageResult::Ok), "{result:?}");
    assert_eq!(plugin.published_events(), vec![usage_event()]);
}

#[tokio::test]
async fn a_transient_publish_error_is_retry_on_any_attempt() {
    let plugin = FakePolicyPlugin::empty();
    plugin.set_publish(PublishBehavior::Transient);
    let world = world_with(&plugin);

    for attempts in [0, 1, 119, 120, 5000] {
        let result = handler(&world)
            .handle(&outbox_message(usage_payload(), attempts))
            .await;

        assert!(
            matches!(result, MessageResult::Retry),
            "attempt {attempts}: {result:?}"
        );
    }
}

#[tokio::test]
async fn a_permanent_publish_error_is_reject_with_a_reason() {
    let plugin = FakePolicyPlugin::empty();
    plugin.set_publish(PublishBehavior::Permanent);
    let world = world_with(&plugin);

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    match result {
        MessageResult::Reject(reason) => assert!(!reason.is_empty()),
        other => panic!("expected Reject, got {other:?}"),
    }
}

#[tokio::test]
async fn a_publish_that_never_answers_is_retry_and_logs_a_warning() {
    let plugin = FakePolicyPlugin::empty();
    plugin.set_publish(PublishBehavior::Hang);
    let world = world_with(&plugin);
    let logs = LogCapture::start();

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        fast(&world).handle(&outbox_message(usage_payload(), 0)),
    )
    .await
    .expect("the handler enforces its own bound");

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    assert!(!logs.at(Level::WARN).is_empty(), "{:?}", logs.records());
}

#[tokio::test(start_paused = true)]
async fn the_default_publish_bound_is_twenty_seconds() {
    let plugin = FakePolicyPlugin::empty();
    plugin.set_publish(PublishBehavior::Hang);
    let world = world_with(&plugin);
    let started = tokio::time::Instant::now();

    let result = handler(&world)
        .handle(&outbox_message(usage_payload(), 0))
        .await;

    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(20) && elapsed < Duration::from_secs(21),
        "{elapsed:?}"
    );
}
