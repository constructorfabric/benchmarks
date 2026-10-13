#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the GTS plugin specifications.

use crate::gts::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};

#[test]
fn plugin_specs_are_registered_in_the_inventory() {
    let schemas = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    let ids: Vec<String> = schemas
        .iter()
        .filter_map(|s| s["$id"].as_str().map(str::to_owned))
        .collect();

    for want in [
        "gts://gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~",
        "gts://gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~",
    ] {
        assert!(
            ids.iter().any(|id| id == want),
            "{want} not registered; have {ids:?}"
        );
    }
}

#[test]
fn plugin_specs_build_instance_ids_under_their_type() {
    let policy =
        MiniChatModelPolicyPluginSpecV1::gts_make_instance_id("cf.core._.static_policy.v1");
    let audit = MiniChatAuditPluginSpecV1::gts_make_instance_id("cf.core._.static_audit.v1");

    assert!(
        policy
            .as_ref()
            .starts_with(&MiniChatModelPolicyPluginSpecV1::gts_type_id().to_string())
    );
    assert!(
        audit
            .as_ref()
            .starts_with(&MiniChatAuditPluginSpecV1::gts_type_id().to_string())
    );
}
