//! GTS plugin specifications of the mini-chat gear.

use toolkit::gts::PluginV1;
#[allow(unused_imports)]
use toolkit_gts::{gts_id, gts_type_schema};

/// Plugin spec of the model policy plugin (`MiniChatModelPolicyPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_model_policy.plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// Plugin spec of the audit plugin (`MiniChatAuditPluginClientV1`).
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat_audit.plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
