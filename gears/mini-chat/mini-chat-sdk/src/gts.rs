//! GTS plugin specifications.

use toolkit::gts::PluginV1;
use toolkit_gts::gts_type_schema;

/// GTS type of `mini-chat-model-policy-plugin` instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~"),
    description = "Mini-chat model policy plugin specification",
    properties = "",
)]
pub struct MiniChatModelPolicyPluginSpecV1;

/// GTS type of mini-chat audit plugin instances.
#[derive(Default)]
#[gts_type_schema(
    dir_path = "schemas",
    base = PluginV1,
    type_id = gts_id!("cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~"),
    description = "Mini-chat audit plugin specification",
    properties = "",
)]
pub struct MiniChatAuditPluginSpecV1;
