//! GTS schema definitions for mini-chat plugins.
//!
//! Plugins register instances of these types with the types-registry to be
//! discovered by the gear.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type for model policy plugin instances.
///
/// Instance ID format:
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~<vendor>.<package>.<name>.v1`
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type for audit plugin instances.
///
/// Instance ID format:
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~<vendor>.<package>.<name>.v1`
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
