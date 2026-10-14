//! The example server imports these module paths to link the plugin registrations; they must
//! stay public and reachable (definition of done `sdk-crate`).
#![allow(clippy::unwrap_used, clippy::expect_used, unused_imports)]

// the exact import form of `apps/cf-gears-example-server/src/registered_gears.rs`
use mini_chat as _;
use mini_chat::infra::plugins::static_audit as _;
use mini_chat::infra::plugins::static_model_policy as _;

#[test]
fn the_plugin_modules_stay_importable_as_the_example_server_imports_them() {
    let _ =
        mini_chat::infra::plugins::static_model_policy::StaticMiniChatModelPolicyPlugin::default();
}

#[test]
fn the_gear_type_is_exported_at_the_crate_root() {
    let _ = mini_chat::MiniChatGear::default();
}

#[test]
fn the_sdk_is_a_dependency_of_the_gear_crate_and_exposes_the_plugin_contracts() {
    let policy = std::any::type_name::<dyn mini_chat_sdk::MiniChatModelPolicyPluginClientV1>();
    let audit = std::any::type_name::<dyn mini_chat_sdk::MiniChatAuditPluginClientV1>();
    assert!(policy.contains("MiniChatModelPolicyPluginClientV1"));
    assert!(audit.contains("MiniChatAuditPluginClientV1"));
}
