//! GTS schema definitions for mini-chat plugins.
//!
//! Plugins register instances of these types with the types-registry; the
//! mini-chat gear discovers them by vendor and priority.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type of model-policy plugin instances (`MiniChatModelPolicyPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of audit plugin instances (`MiniChatAuditPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
