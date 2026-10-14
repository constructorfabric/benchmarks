#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginSpecV1};

const MODEL_POLICY_ID: &str =
    "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~";
const AUDIT_ID: &str = "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~";

#[test]
fn plugin_specs_carry_the_documented_gts_type_ids() {
    assert_eq!(
        <MiniChatModelPolicyPluginSpecV1 as gts::GtsSchema>::TYPE_ID,
        MODEL_POLICY_ID
    );
    assert_eq!(
        <MiniChatAuditPluginSpecV1 as gts::GtsSchema>::TYPE_ID,
        AUDIT_ID
    );
}

#[test]
fn plugin_specs_are_registered_in_the_gts_inventory() {
    let schemas = toolkit_gts::all_inventory_type_schemas().expect("inventory schemas parse");
    for id in [MODEL_POLICY_ID, AUDIT_ID] {
        let want = serde_json::json!(format!("gts://{id}"));
        assert!(
            schemas.iter().any(|s| s["$id"] == want),
            "{id} missing from the inventory"
        );
    }
}
