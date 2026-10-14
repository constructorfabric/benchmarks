//! GTS (Global Type System) declarations for the mini-chat SDK.
//!
//! Plugin specs nest under the toolkit base,
//! `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.<spec>.v1~`. Each
//! `#[gts_type_schema(...)]` struct submits its schema to the link-time
//! inventory that the types-registry loads at boot.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS plugin spec of the model-policy plugin (`mini-chat-model-policy-plugin`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model-policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS plugin spec of the audit plugin.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;

#[cfg(test)]
#[path = "gts_tests.rs"]
mod gts_tests;
