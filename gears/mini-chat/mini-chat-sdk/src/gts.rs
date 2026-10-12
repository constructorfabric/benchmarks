//! GTS plugin specifications of the mini-chat gear.
//!
//! The gear registers these schemas; each plugin registers an instance of the matching type
//! with the types registry. The gear picks the instance whose `vendor` equals its configured
//! vendor and has the lowest `priority`.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS plugin specification of the model-policy plugin.
///
/// Instance ids look like
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~<vendor>.<package>.<name>.v1`.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini Chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS plugin specification of the audit plugin.
///
/// Instance ids look like
/// `gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~<vendor>.<package>.<name>.v1`.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini Chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
